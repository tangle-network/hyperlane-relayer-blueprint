#[cfg(test)]
mod e2e;

use blueprint_sdk as sdk;
use blueprint_sdk::runners::core::runner::BlueprintRunner;
use color_eyre::Result;
pub use hyperlane_relayer_blueprint as blueprint;
use sdk::logging;
use sdk::runners::tangle::tangle::TangleConfig;
use std::path::{Path, PathBuf};

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
            logging::warn!("Data dir not specified, using default");
            data_dir = default_data_dir();
        }
    }

    if !data_dir.exists() {
        logging::warn!("Data dir does not exist, creating");
        std::fs::create_dir_all(&data_dir)?;
    }

    let ctx = blueprint::HyperlaneContext::new(env, data_dir).await?;

    let set_config = blueprint::SetConfigEventHandler::new(&ctx.env, ctx.clone()).await?;

    let tangle_config = TangleConfig::default();
    BlueprintRunner::new(tangle_config, ctx.env.clone())
        .job(set_config)
        .run()
        .await?;

    logging::info!("Shutting down...");
    ctx.remove_existing_container().await?;

    Ok(())
}
