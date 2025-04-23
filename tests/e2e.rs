use blueprint::HyperlaneContext;
use blueprint_sdk as sdk;
use color_eyre::Report;
use docktopus::bollard::container::RemoveContainerOptions;
use docktopus::bollard::network::{
    ConnectNetworkOptions, CreateNetworkOptions, InspectNetworkOptions,
};
use docktopus::{DockerBuilder, bollard};
use hyperlane_relayer_blueprint_lib as blueprint;
use sdk::Job;
use sdk::tangle::layers::TangleLayer;
use sdk::tangle::serde::to_field;
use sdk::tangle_subxt::tangle_testnet_runtime::api::services::calls::types::call::Args;
use sdk::testing::chain_setup::anvil::AnvilTestnet;
use sdk::testing::chain_setup::anvil::start_anvil_container;
use sdk::testing::tempfile;
use sdk::testing::tempfile::TempDir;
use sdk::testing::utils::setup_log;
use sdk::testing::utils::tangle::{OutputValue, TangleTestHarness};
use sdk::tokio;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};

const AGENT_CONFIG_TEMPLATE_PATH: &str = "./tests/assets/agent-config.json.template";
const CORE_CONFIG_PATH: &str = "./tests/assets/core-config.yaml";
const TEST_ASSETS_PATH: &str = "./tests/assets";

fn setup_temp_dir(
    (testnet1_docker_rpc_url, testnet1_host_rpc_url): (String, String),
    (testnet2_docker_rpc_url, testnet2_host_rpc_url): (String, String),
) -> TempDir {
    const FILE_PREFIXES: [&str; 2] = ["testnet1", "testnet2"];

    let tempdir = tempfile::tempdir().unwrap();

    // Create the registry
    let registry_path = tempdir.path().join("chains");
    fs::create_dir(&registry_path).unwrap();

    for (prefix, rpc_url) in FILE_PREFIXES
        .iter()
        .zip([&*testnet1_host_rpc_url, &*testnet2_host_rpc_url])
    {
        let testnet_path = registry_path.join(prefix);
        fs::create_dir(&testnet_path).unwrap();

        let addresses_path = Path::new(TEST_ASSETS_PATH).join(format!("{prefix}-addresses.yaml"));
        fs::copy(addresses_path, testnet_path.join("addresses.yaml")).unwrap();

        let metadata_template_path =
            Path::new(TEST_ASSETS_PATH).join(format!("{prefix}-metadata.yaml.template"));
        let testnet1_metadata = fs::read_to_string(metadata_template_path).unwrap();
        fs::write(
            testnet_path.join("metadata.yaml"),
            testnet1_metadata.replace("{RPC_URL}", rpc_url),
        )
        .unwrap();
    }

    // Create the core config
    let configs_dir = tempdir.path().join("configs");
    fs::create_dir(&configs_dir).unwrap();
    fs::copy(CORE_CONFIG_PATH, configs_dir.join("core-config.yaml")).unwrap();

    // Create agent config
    let agent_config_template = fs::read_to_string(AGENT_CONFIG_TEMPLATE_PATH).unwrap();
    fs::write(
        tempdir.path().join("agent-config.json"),
        agent_config_template
            .replace("{TESTNET_1_RPC}", &testnet1_docker_rpc_url)
            .replace("{TESTNET_2_RPC}", &testnet2_docker_rpc_url),
    )
    .unwrap();

    tempdir
}

const TESTNET1_STATE_PATH: &str = "./tests/assets/testnet1-state.json";
const TESTNET2_STATE_PATH: &str = "./tests/assets/testnet2-state.json";

#[allow(dead_code)]
struct AnvilContainer {
    inner: AnvilTestnet,
    ip: String,
}

async fn spinup_anvil_testnets() -> color_eyre::Result<(AnvilContainer, AnvilContainer)> {
    let origin_state = fs::read_to_string(TESTNET1_STATE_PATH)?;
    let origin_testnet = start_anvil_container(Some(&origin_state), false).await;

    let dest_state = fs::read_to_string(TESTNET2_STATE_PATH)?;
    let dest_testnet = start_anvil_container(Some(&dest_state), false).await;

    let connection = DockerBuilder::new().await?;
    if let Err(e) = connection
        .create_network(CreateNetworkOptions {
            name: "hyperlane_relayer_test_net",
            ..Default::default()
        })
        .await
    {
        match e {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 409, ..
            } => {}
            _ => return Err(e.into()),
        }
    }

    connection
        .connect_network(
            "hyperlane_relayer_test_net",
            ConnectNetworkOptions {
                container: origin_testnet.container.id(),
                ..Default::default()
            },
        )
        .await?;

    connection
        .connect_network(
            "hyperlane_relayer_test_net",
            ConnectNetworkOptions {
                container: dest_testnet.container.id(),
                ..Default::default()
            },
        )
        .await?;

    let origin_container_inspect = connection
        .inspect_container(origin_testnet.container.id(), None)
        .await?;
    let origin_network_settings = origin_container_inspect
        .network_settings
        .unwrap()
        .networks
        .unwrap()["hyperlane_relayer_test_net"]
        .clone();

    let dest_container_inspect = connection
        .inspect_container(dest_testnet.container.id(), None)
        .await?;
    let dest_network_settings = dest_container_inspect
        .network_settings
        .unwrap()
        .networks
        .unwrap()["hyperlane_relayer_test_net"]
        .clone();

    Ok((
        AnvilContainer {
            inner: origin_testnet,
            ip: origin_network_settings.ip_address.unwrap(),
        },
        AnvilContainer {
            inner: dest_testnet,
            ip: dest_network_settings.ip_address.unwrap(),
        },
    ))
}

static HYPERLANE_CLI_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    Path::new(".")
        .join("node_modules")
        .join(".bin")
        .join("hyperlane")
        .canonicalize()
        .unwrap()
});

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::needless_return)]
async fn relayer() -> color_eyre::Result<()> {
    setup_log();

    if !HYPERLANE_CLI_PATH.exists() {
        return Err(Report::msg(
            "Hyperlane CLI not found, make sure to run `npm install`!",
        ));
    }

    // Test logic is separated so that cleanup is performed regardless of failure
    let res = relayer_test_inner().await;

    // Cleanup network
    let connection = DockerBuilder::new().await?;
    let network = connection
        .inspect_network(
            "hyperlane_relayer_test_net",
            None::<InspectNetworkOptions<String>>,
        )
        .await?;
    for container in network.containers.unwrap().keys() {
        connection
            .remove_container(
                container,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await?;
    }

    connection
        .remove_network("hyperlane_relayer_test_net")
        .await?;

    res
}

async fn relayer_test_inner() -> color_eyre::Result<()> {
    let (origin, dest) = spinup_anvil_testnets().await?;

    // The relayer itself uses the IPs internal to the Docker network.
    // When it comes time to relay the message, the command is run outside the Docker network,
    // so we need to get both addresses.
    //
    // The internal address is written to `agent-config.json`.
    // The host addresses are written to `testnet{1,2}-metadata.yaml`.
    let testnet1_docker_rpc_url = format!("{}:8545", origin.ip);
    let testnet2_docker_rpc_url = format!("{}:8545", dest.ip);

    let origin_ports = origin.inner.container.ports().await?;
    let dest_ports = dest.inner.container.ports().await?;

    let testnet1_host_rpc_url = format!(
        "127.0.0.1:{}",
        origin_ports.map_to_host_port_ipv4(8545).unwrap()
    );
    let testnet2_host_rpc_url = format!(
        "127.0.0.1:{}",
        dest_ports.map_to_host_port_ipv4(8545).unwrap()
    );

    let tempdir = setup_temp_dir(
        (testnet1_docker_rpc_url, testnet1_host_rpc_url.clone()),
        (testnet2_docker_rpc_url, testnet2_host_rpc_url),
    );
    let temp_dir_path = tempdir.path().to_path_buf();

    let harness = TangleTestHarness::setup(tempdir).await?;

    // Setup service
    let (mut test_env, service_id, _) = harness.setup_services::<1>(false).await?;
    test_env.initialize().await?;

    test_env
        .add_job(blueprint::set_config.layer(TangleLayer))
        .await;

    let ctx = Arc::new(HyperlaneContext::new(harness.env().clone(), temp_dir_path.clone()).await?);
    test_env.start(ctx).await?;

    // Pass the arguments
    let agent_config_path = std::path::absolute(temp_dir_path.join("agent-config.json"))?;
    let config_urls = to_field(Some(vec![format!(
        "file://{}",
        agent_config_path.display()
    )]))?;
    let relay_chains = to_field(String::from("testnet1,testnet2"))?;

    // Execute job and verify result
    let call = harness
        .submit_job(service_id, 0, Args::from([config_urls, relay_chains]))
        .await?;

    let results = harness.wait_for_job_execution(0, call).await?;
    harness.verify_job(&results, vec![OutputValue::Uint64(0)]);

    assert_eq!(results.service_id, service_id);

    // The relayer is now running, send a message
    std::env::set_current_dir(temp_dir_path)?;
    let send_msg_output = Command::new(&*HYPERLANE_CLI_PATH)
        .args([
            "send",
            "message",
            "--registry",
            ".",
            "--origin",
            "testnet1",
            "--destination",
            "testnet2",
            "--quick",
        ])
        .env(
            "HYP_KEY",
            "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
        )
        .output()?;

    if !send_msg_output.status.success() {
        sdk::error!(
            "Failed to send test message: {}",
            String::from_utf8_lossy(&send_msg_output.stderr)
        );
        return Err(Report::msg("Failed to send test message"));
    }

    let stdout = String::from_utf8_lossy(&send_msg_output.stdout);

    let mut msg_id = None;
    for line in String::from_utf8_lossy(&send_msg_output.stdout).lines() {
        let Some(id) = line.strip_prefix("Message ID: ") else {
            continue;
        };

        msg_id = Some(id.to_string());
        break;
    }

    let Some(msg_id) = msg_id else {
        panic!("No message ID found in output: {stdout}")
    };

    sdk::info!("Message ID: {msg_id}");

    // Give the command a few seconds
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    sdk::info!("Mining a block");
    Command::new("cast")
        .args([
            "rpc",
            "anvil_mine",
            "1",
            "--rpc-url",
            &*testnet1_host_rpc_url,
        ])
        .output()?;

    // Give the command a few seconds
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let msg_status_output = Command::new(&*HYPERLANE_CLI_PATH)
        .args([
            "status",
            "--registry",
            ".",
            "--origin",
            "testnet1",
            "--id",
            &*msg_id,
        ])
        .env(
            "HYP_KEY",
            "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
        )
        .output()?;

    if !msg_status_output.status.success() {
        sdk::error!(
            "Failed to check message status: {}",
            String::from_utf8_lossy(&msg_status_output.stderr)
        );
        return Err(Report::msg("Failed to check message status"));
    }

    if !String::from_utf8_lossy(&msg_status_output.stdout)
        .contains(&format!("Message {msg_id} was delivered"))
    {
        sdk::error!(
            "Message was not delivered: {}",
            String::from_utf8_lossy(&msg_status_output.stderr)
        );
        return Err(Report::msg("Message was not delivered"));
    }

    Ok(())
}
