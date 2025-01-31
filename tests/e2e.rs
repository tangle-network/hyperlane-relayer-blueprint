use std::path::Path;
use std::process::Command;
use blueprint_sdk as sdk;
use gadget_sdk::tangle_subxt::tangle_testnet_runtime::api::runtime_types::bounded_collections::bounded_vec::BoundedVec;
use gadget_sdk::tangle_subxt::tangle_testnet_runtime::api::runtime_types::tangle_primitives::services::field::BoundedString;
use gadget_sdk::tangle_subxt::tangle_testnet_runtime::api::runtime_types::tangle_primitives::services::field::Field;
use gadget_sdk::tangle_subxt::tangle_testnet_runtime::api::services::calls::types::call::Args;
use blueprint_test_utils::test_ext::*;
use blueprint_test_utils::*;
use blueprint_test_utils::eigenlayer_test_env::start_anvil_testnet;
use cargo_tangle::deploy::Opts;
use gadget_sdk::error;
use gadget_sdk::info;
use gadget_sdk::docker::{bollard, connect_to_docker};
use gadget_sdk::docker::bollard::container::RemoveContainerOptions;
use gadget_sdk::docker::bollard::network::{ConnectNetworkOptions, CreateNetworkOptions, InspectNetworkOptions};
use tempfile::TempDir;
use testcontainers::ContainerAsync;
use testcontainers::GenericImage;

pub fn setup_testing_log() {
    use tracing_subscriber::util::SubscriberInitExt;
    let env_filter = tracing_subscriber::EnvFilter::from_default_env();
    let _ = tracing_subscriber::fmt::SubscriberBuilder::default()
        .without_time()
        .with_target(true)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
        .with_env_filter(env_filter)
        .with_test_writer()
        .finish()
        .try_init();
}

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
            testnet1_metadata.replace("{RPC_URL}", &rpc_url),
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

async fn spinup_anvil_testnets() -> (
    (ContainerAsync<GenericImage>, String),
    (ContainerAsync<GenericImage>, String),
) {
    let (origin_container, _, _) = start_anvil_testnet(TESTNET1_STATE_PATH, false).await;

    let (dest_container, _, _) = start_anvil_testnet(TESTNET2_STATE_PATH, false).await;

    let connection = connect_to_docker(None).await.unwrap();
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
            _ => panic!("{e}"),
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
        .await
        .unwrap();

    connection
        .connect_network(
            "hyperlane_relayer_test_net",
            ConnectNetworkOptions {
                container: dest_container.id(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let origin_container_inspect = connection
        .inspect_container(origin_container.id(), None)
        .await
        .unwrap();
    let origin_network_settings = origin_container_inspect
        .network_settings
        .unwrap()
        .networks
        .unwrap()["hyperlane_relayer_test_net"]
        .clone();

    let dest_container_inspect = connection
        .inspect_container(dest_container.id(), None)
        .await
        .unwrap();
    let dest_network_settings = dest_container_inspect
        .network_settings
        .unwrap()
        .networks
        .unwrap()["hyperlane_relayer_test_net"]
        .clone();

    (
        (
            origin_container,
            origin_network_settings.ip_address.unwrap(),
        ),
        (dest_container, dest_network_settings.ip_address.unwrap()),
    )
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::needless_return)]
async fn relayer() {
    setup_testing_log();

    let ((origin_container, origin_container_ip), (dest_container, dest_container_ip)) =
        spinup_anvil_testnets().await;

    // The relayer itself uses the IPs internal to the Docker network.
    // When it comes time to relay the message, the command is run outside the Docker network,
    // so we need to get both addresses.
    //
    // The internal address is written to `agent-config.json`.
    // The host addresses are written to `testnet{1,2}-metadata.yaml`.
    let testnet1_docker_rpc_url = format!("{}:8545", origin_container_ip);
    let testnet2_docker_rpc_url = format!("{}:8545", dest_container_ip);

    let origin_ports = origin_container.ports().await.unwrap();
    let dest_ports = dest_container.ports().await.unwrap();

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
    let temp_dir_path = tempdir.path();

    let tangle = tangle::run().unwrap();
    let base_path = std::env::current_dir().expect("Failed to get current directory");
    let base_path = base_path
        .canonicalize()
        .expect("File could not be normalized");

    let manifest_path = base_path.join("Cargo.toml");

    let opts = Opts {
        pkg_name: option_env!("CARGO_BIN_NAME").map(ToOwned::to_owned),
        http_rpc_url: format!("http://127.0.0.1:{}", tangle.ws_port()),
        ws_rpc_url: format!("ws://127.0.0.1:{}", tangle.ws_port()),
        manifest_path,
        signer: None,
        signer_evm: None,
    };

    const N: usize = 1;

    new_test_ext_blueprint_manager::<N, 1, _, _, _>("", opts, run_test_blueprint_manager)
        .await
        .execute_with_async(move |client, handles, svcs| async move {
            // At this point, blueprint has been deployed, every node has registered
            // as an operator for the relevant services, and, all gadgets are running

            let keypair = handles[0].sr25519_id().clone();

            let service = svcs.services.last().unwrap();

            let service_id = service.id;
            let call_id = get_next_call_id(client)
                .await
                .expect("Failed to get next job id")
                .saturating_sub(1);

            info!("Submitting job with params service ID: {service_id}, call ID: {call_id}");

            // Pass the arguments
            let agent_config_path =
                std::path::absolute(temp_dir_path.join("agent-config.json")).unwrap();
            let config_urls = Field::List(BoundedVec(vec![Field::String(BoundedString(
                BoundedVec(format!("file://{}", agent_config_path.display()).into_bytes()),
            ))]));
            let relay_chains = Field::String(BoundedString(BoundedVec(
                String::from("testnet1,testnet2").into_bytes(),
            )));

            // Next step: submit a job under that service/job id
            if let Err(err) = submit_job(
                client,
                &keypair,
                service_id,
                0,
                Args::from([config_urls, relay_chains]),
            )
            .await
            {
                error!("Failed to submit job: {err}");
                panic!("Failed to submit job: {err}");
            }

            // Step 2: wait for the job to complete
            let job_results = wait_for_completion_of_tangle_job(client, service_id, call_id, N)
                .await
                .expect("Failed to wait for job completion");

            // Step 3: Get the job results, compare to expected value(s)
            assert_eq!(job_results.service_id, service_id);
            assert_eq!(job_results.call_id, call_id);
            assert_eq!(job_results.result[0], Field::Uint64(0));

            // The relayer is now running, send a message
            std::env::set_current_dir(temp_dir_path).expect("Failed to change directory");
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
                .output()
                .expect("Failed to run command");

            if !send_msg_output.status.success() {
                panic!(
                    "Failed to send test message: {}",
                    String::from_utf8_lossy(&send_msg_output.stderr)
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

            tracing::info!("Message ID: {msg_id}");

            // Give the command a few seconds
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            tracing::info!("Mining a block");
            Command::new("cast")
                .args([
                    "rpc",
                    "anvil_mine",
                    "1",
                    "--rpc-url",
                    &*testnet1_host_rpc_url,
                ])
                .output()
                .unwrap();

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
        })
        .await;

    drop(origin_container);
    drop(dest_container);

    // Cleanup network
    let connection = connect_to_docker(None).await.unwrap();
    let network = connection
        .inspect_network(
            "hyperlane_relayer_test_net",
            None::<InspectNetworkOptions<String>>,
        )
        .await
        .unwrap();
    for container in network.containers.unwrap().keys() {
        connection
            .remove_container(
                container,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
    }

    connection
        .remove_network("hyperlane_relayer_test_net")
        .await
        .unwrap();
}
