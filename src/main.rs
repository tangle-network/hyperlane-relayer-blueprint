#[cfg(test)]
mod e2e;

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

    info!("Shutting down...");
    ctx.remove_existing_container().await?;

    Ok(())
}
