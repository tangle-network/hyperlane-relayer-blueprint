use color_eyre::Result;
use gadget_sdk as sdk;
pub use hyperlane_relayer_blueprint as blueprint;
use sdk::ctx::TangleClientContext;
use sdk::info;
use sdk::runners::tangle::TangleConfig;
use sdk::runners::BlueprintRunner;
use sdk::tangle_subxt::subxt::tx::Signer;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn default_data_dir() -> PathBuf {
    const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");
    Path::new(MANIFEST_DIR).join("data")
}

#[sdk::main(env)]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let data_dir;
    match env.data_dir.clone() {
        Some(dir) => data_dir = dir,
        None => {
            tracing::warn!("Data dir not specified, using default");
            data_dir = default_data_dir();
        }
    }

    if !data_dir.exists() {
        tracing::warn!("Data dir does not exist, creating");
        std::fs::create_dir_all(&data_dir)?;
    }

    let ctx = Arc::new(blueprint::HyperlaneContext::new(env, data_dir).await?);

    let client = ctx.tangle_client().await?;
    let signer = ctx.env.first_sr25519_signer()?;

    let set_config = blueprint::SetConfigEventHandler {
        ctx: Arc::clone(&ctx),
        service_id: ctx.env.service_id().unwrap(),
        signer: signer.clone(),
        client,
    };

    info!("Starting the event watcher for {} ...", signer.account_id());
    let tangle_config = TangleConfig::default();
    BlueprintRunner::new(tangle_config, ctx.env.clone())
        .job(set_config)
        .run()
        .await?;

    Ok(())
}

#[cfg(test)]
mod e2e {
    use gadget_sdk::tangle_subxt::tangle_testnet_runtime::api::runtime_types::bounded_collections::bounded_vec::BoundedVec;
    use gadget_sdk::tangle_subxt::tangle_testnet_runtime::api::runtime_types::tangle_primitives::services::field::BoundedString;
    use gadget_sdk::tangle_subxt::tangle_testnet_runtime::api::runtime_types::tangle_primitives::services::field::Field;
    use gadget_sdk::tangle_subxt::tangle_testnet_runtime::api::services::calls::types::call::Args;
    use blueprint_test_utils::test_ext::*;
    use blueprint_test_utils::*;
    use cargo_tangle::deploy::Opts;
    use gadget_sdk::error;
    use gadget_sdk::info;

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

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::needless_return)]
    async fn relayer() {
        setup_testing_log();
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

        const N: usize = 3;

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
                let config = Field::None;
                let relay_chains = Field::String(BoundedString(BoundedVec(
                    String::from("ethereum,polygon,avalanche").into_bytes(),
                )));

                // Next step: submit a job under that service/job id
                if let Err(err) = submit_job(
                    client,
                    &keypair,
                    service_id,
                    0,
                    Args::from([config, relay_chains]),
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
            })
            .await;
    }
}
