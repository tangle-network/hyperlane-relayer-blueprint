use std::path::Path;
use std::process::Command;
use blueprint_sdk as sdk;
use blueprint_sdk::logging::setup_log;
use blueprint_sdk::testing::tempfile;
use blueprint_sdk::testing::tempfile::TempDir;
use blueprint_sdk::testing::utils::anvil::start_anvil_testnet;
use blueprint_sdk::{logging, tokio};
use blueprint_sdk::tangle_subxt::tangle_testnet_runtime::api::services::calls::types::call::Args;
use blueprint_sdk::testing::utils::harness::TestHarness;
use blueprint_sdk::testing::utils::runner::TestEnv;
use blueprint_sdk::testing::utils::tangle::{OutputValue, TangleTestHarness};
use dockworker::bollard::network::{ConnectNetworkOptions, CreateNetworkOptions, InspectNetworkOptions};
use dockworker::{bollard, DockerBuilder};
use dockworker::bollard::container::RemoveContainerOptions;
use sdk::tangle_subxt::tangle_testnet_runtime::api::runtime_types::bounded_collections::bounded_vec::BoundedVec;
use sdk::tangle_subxt::tangle_testnet_runtime::api::runtime_types::tangle_primitives::services::field::BoundedString;
use sdk::tangle_subxt::tangle_testnet_runtime::api::runtime_types::tangle_primitives::services::field::Field;
use testcontainers::ContainerAsync;
use testcontainers::GenericImage;
use hyperlane_relayer_blueprint::{HyperlaneContext, SetConfigEventHandler};

const AGENT_CONFIG_TEMPLATE_PATH: &str = "./test_assets/agent-config.json.template";
const CORE_CONFIG_PATH: &str = "./test_assets/core-config.yaml";
const TEST_ASSETS_PATH: &str = "./test_assets";

fn setup_temp_dir(
    (testnet1_docker_rpc_url, testnet1_host_rpc_url): (String, String),
    (testnet2_docker_rpc_url, testnet2_host_rpc_url): (String, String),
) -> TempDir {
    const FILE_PREFIXES: [&str; 2] = ["testnet1", "testnet2"];

    let tempdir = tempfile::tempdir().unwrap();

    // Create the registry
    let registry_path = tempdir.path().join("chains");
    std::fs::create_dir(&registry_path).unwrap();

    for (prefix, rpc_url) in FILE_PREFIXES
        .iter()
        .zip([&*testnet1_host_rpc_url, &*testnet2_host_rpc_url])
    {
        let testnet_path = registry_path.join(prefix);
        std::fs::create_dir(&testnet_path).unwrap();

        let addresses_path = Path::new(TEST_ASSETS_PATH).join(format!("{prefix}-addresses.yaml"));
        std::fs::copy(addresses_path, testnet_path.join("addresses.yaml")).unwrap();

        let metadata_template_path =
            Path::new(TEST_ASSETS_PATH).join(format!("{prefix}-metadata.yaml.template"));
        let testnet1_metadata = std::fs::read_to_string(metadata_template_path).unwrap();
        std::fs::write(
            testnet_path.join("metadata.yaml"),
            testnet1_metadata.replace("{RPC_URL}", rpc_url),
        )
        .unwrap();
    }

    // Create the core config
    let configs_dir = tempdir.path().join("configs");
    std::fs::create_dir(&configs_dir).unwrap();
    std::fs::copy(CORE_CONFIG_PATH, configs_dir.join("core-config.yaml")).unwrap();

    // Create agent config
    let agent_config_template = std::fs::read_to_string(AGENT_CONFIG_TEMPLATE_PATH).unwrap();
    std::fs::write(
        tempdir.path().join("agent-config.json"),
        agent_config_template
            .replace("{TESTNET_1_RPC}", &testnet1_docker_rpc_url)
            .replace("{TESTNET_2_RPC}", &testnet2_docker_rpc_url),
    )
    .unwrap();

    tempdir
}

const TESTNET1_STATE_PATH: &str = "./test_assets/testnet1-state.json";
const TESTNET2_STATE_PATH: &str = "./test_assets/testnet2-state.json";

async fn spinup_anvil_testnets() -> color_eyre::Result<(
    (ContainerAsync<GenericImage>, String),
    (ContainerAsync<GenericImage>, String),
)> {
    let (origin_container, _, _) = start_anvil_testnet(TESTNET1_STATE_PATH, false).await;

    let (dest_container, _, _) = start_anvil_testnet(TESTNET2_STATE_PATH, false).await;

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
                container: origin_container.id(),
                ..Default::default()
            },
        )
        .await?;

    connection
        .connect_network(
            "hyperlane_relayer_test_net",
            ConnectNetworkOptions {
                container: dest_container.id(),
                ..Default::default()
            },
        )
        .await?;

    let origin_container_inspect = connection
        .inspect_container(origin_container.id(), None)
        .await?;
    let origin_network_settings = origin_container_inspect
        .network_settings
        .unwrap()
        .networks
        .unwrap()["hyperlane_relayer_test_net"]
        .clone();

    let dest_container_inspect = connection
        .inspect_container(dest_container.id(), None)
        .await?;
    let dest_network_settings = dest_container_inspect
        .network_settings
        .unwrap()
        .networks
        .unwrap()["hyperlane_relayer_test_net"]
        .clone();

    Ok((
        (
            origin_container,
            origin_network_settings.ip_address.unwrap(),
        ),
        (dest_container, dest_network_settings.ip_address.unwrap()),
    ))
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::needless_return)]
async fn relayer() -> color_eyre::Result<()> {
    setup_log();

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
    let ((origin_container, origin_container_ip), (dest_container, dest_container_ip)) =
        spinup_anvil_testnets().await?;

    // The relayer itself uses the IPs internal to the Docker network.
    // When it comes time to relay the message, the command is run outside the Docker network,
    // so we need to get both addresses.
    //
    // The internal address is written to `agent-config.json`.
    // The host addresses are written to `testnet{1,2}-metadata.yaml`.
    let testnet1_docker_rpc_url = format!("{}:8545", origin_container_ip);
    let testnet2_docker_rpc_url = format!("{}:8545", dest_container_ip);

    let origin_ports = origin_container.ports().await?;
    let dest_ports = dest_container.ports().await?;

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

    let ctx = HyperlaneContext::new(harness.env().clone(), temp_dir_path.clone()).await?;

    let handler = SetConfigEventHandler::new(harness.env(), ctx).await?;

    // Setup service
    let (mut test_env, service_id) = harness.setup_services().await?;
    test_env.add_job(handler);

    tokio::spawn(async move {
        test_env.run_runner().await.unwrap();
    });

    // Pass the arguments
    let agent_config_path = std::path::absolute(temp_dir_path.join("agent-config.json"))?;
    let config_urls = Field::List(BoundedVec(vec![Field::String(BoundedString(BoundedVec(
        format!("file://{}", agent_config_path.display()).into_bytes(),
    )))]));
    let relay_chains = Field::String(BoundedString(BoundedVec(
        String::from("testnet1,testnet2").into_bytes(),
    )));

    // Execute job and verify result
    let results = harness
        .execute_job(
            service_id,
            0,
            Args::from([config_urls, relay_chains]),
            vec![OutputValue::Uint64(0)],
        )
        .await?;

    assert_eq!(results.service_id, service_id);

    // The relayer is now running, send a message
    std::env::set_current_dir(temp_dir_path)?;
    let send_msg_output = Command::new("hyperlane")
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
        std::mem::forget(origin_container);
        std::mem::forget(dest_container);
        std::mem::forget(harness);
        panic!(
            "Failed to send test message: {}",
            String::from_utf8_lossy(&send_msg_output.stdout)
        );
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

    logging::info!("Message ID: {msg_id}");

    // Give the command a few seconds
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    logging::info!("Mining a block");
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

    let msg_status_output = Command::new("hyperlane")
        .args([
            "status",
            "--registry",
            ".",
            "--origin",
            "testnet1",
            "--destination",
            "testnet2",
            "--id",
            &*msg_id,
        ])
        .env(
            "HYP_KEY",
            "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
        )
        .output()
        .expect("Failed to run command");

    assert!(msg_status_output.status.success());
    assert!(String::from_utf8_lossy(&msg_status_output.stdout)
        .contains(&format!("Message {msg_id} was delivered")));

    drop(origin_container);
    drop(dest_container);

    Ok(())
}
