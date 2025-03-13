use blueprint_sdk as sdk;
use hyperlane_relayer_blueprint_lib as blueprint;
use sdk::Job;
use sdk::contexts::tangle::TangleClientContext;
use sdk::crypto::sp_core::SpSr25519;
use sdk::crypto::tangle_pair_signer::TanglePairSigner;
use sdk::keystore::backends::Backend;
use sdk::runner::BlueprintRunner;
use sdk::runner::config::BlueprintEnvironment;
use sdk::runner::tangle::config::TangleConfig;
use sdk::tangle::consumer::TangleConsumer;
use sdk::tangle::layers::TangleLayer;
use sdk::tangle::producer::TangleProducer;
use std::sync::Arc;
use tracing_subscriber::filter::LevelFilter;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    setup_log();

    let env = BlueprintEnvironment::load()?;

    let data_dir = env.data_dir.clone().unwrap_or_else(|| {
        tracing::warn!("Data dir not specified, using default");
        blueprint::default_data_dir()
    });

    if !data_dir.exists() {
        tracing::warn!("Data dir does not exist, creating");
        std::fs::create_dir_all(&data_dir)?;
    }

    // Signer
    let sr25519_signer = env.keystore().first_local::<SpSr25519>()?;
    let sr25519_pair = env.keystore().get_secret::<SpSr25519>(&sr25519_signer)?;
    let sr25519_signer = TanglePairSigner::new(sr25519_pair.0);

    // Producer
    let tangle_client = env.tangle_client().await?;
    let tangle_producer =
        TangleProducer::finalized_blocks(tangle_client.rpc_client.clone()).await?;

    // Consumer
    let tangle_consumer = TangleConsumer::new(tangle_client.rpc_client.clone(), sr25519_signer);

    let context = Arc::new(blueprint::HyperlaneContext::new(env.clone(), data_dir).await?);

    sdk::info!("Starting the event watcher ...");

    let result = BlueprintRunner::builder(TangleConfig::default(), env)
        .router(
            sdk::Router::new()
                .route(
                    blueprint::SET_CONFIG_JOB_ID,
                    blueprint::set_config.layer(TangleLayer),
                )
                .with_context(context.clone()),
        )
        .producer(tangle_producer)
        .consumer(tangle_consumer)
        .with_shutdown_handler(async move {
            tracing::info!("Shutting down...");
            context.remove_existing_container().await.ok();
        })
        .run()
        .await;

    if let Err(e) = result {
        sdk::error!("Runner failed! {e:?}");
    }

    Ok(())
}

pub fn setup_log() {
    use tracing_subscriber::util::SubscriberInitExt;

    let _ = tracing_subscriber::fmt::SubscriberBuilder::default()
        .without_time()
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .finish()
        .try_init();
}
