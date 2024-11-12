use alloy_primitives::hex::hex;
use color_eyre::eyre::eyre;
use color_eyre::Result;
use gadget_sdk as sdk;
use gadget_sdk::keystore::BackendExt;
use sdk::config::StdGadgetConfiguration;
use sdk::ctx::{ServicesContext, TangleClientContext};
use sdk::docker::{bollard::Docker, connect_to_docker, Container};
use sdk::event_listener::tangle::jobs::{services_post_processor, services_pre_processor};
use sdk::event_listener::tangle::TangleEventListener;
use sdk::tangle_subxt::tangle_testnet_runtime::api::services::events::JobCalled;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(TangleClientContext, ServicesContext)]
pub struct HyperlaneContext {
    #[config]
    pub env: StdGadgetConfiguration,
    data_dir: PathBuf,
    connection: Arc<Docker>,
    container: Mutex<Option<String>>,
}

const IMAGE: &str = "gcr.io/abacus-labs-dev/hyperlane-agent:main";
impl HyperlaneContext {
    pub async fn new(env: StdGadgetConfiguration, data_dir: PathBuf) -> Result<Self> {
        let connection = connect_to_docker(None).await?;
        Ok(Self {
            env,
            data_dir,
            connection,
            container: Mutex::new(None),
        })
    }

    #[tracing::instrument(skip_all)]
    async fn spinup_container(&self) -> Result<()> {
        let mut container_guard = self.container.lock().await;
        if container_guard.is_some() {
            return Ok(());
        }

        tracing::info!("Spinning up new container");

        // TODO: Bollard isn't pulling the image for some reason?
        let output = Command::new("docker").args(["pull", IMAGE]).output()?;
        if !output.status.success() {
            return Err(eyre!("Docker pull failed"));
        }

        let mut container = Container::new(&self.connection, IMAGE);

        let keystore = self.env.keystore()?;
        let ecdsa = keystore.ecdsa_key()?.alloy_key()?;
        let secret = hex::encode(ecdsa.to_bytes());

        let mut binds = Vec::new();

        let hyperlane_db_path = self.hyperlane_db_path();
        if !hyperlane_db_path.exists() {
            tracing::warn!("Hyperlane DB does not exist, creating...");
            std::fs::create_dir_all(&hyperlane_db_path)?;
            tracing::info!("Hyperlane DB created at `{}`", hyperlane_db_path.display());
        }

        binds.push(format!("{}:/hyperlane_db", hyperlane_db_path.display()));

        let agent_config_path = self.agent_config_path();
        if agent_config_path.exists() {
            binds.push(format!(
                "{}:/config/agent-config.json:ro",
                agent_config_path.to_string_lossy()
            ));
        }

        let relay_chains_path = self.relay_chains_path();
        if relay_chains_path.exists() {
            let relay_chains = std::fs::read_to_string(relay_chains_path)?;
            container.env([format!("HYP_RELAYCHAINS={relay_chains}")]);
        }

        container.binds(binds).cmd([
            "./relayer",
            "--db /hyperlane_db",
            "--defaultSigner.key",
            &format!("0x{secret}"),
        ]);

        container.start(false).await?;
        *container_guard = container.id().map(ToString::to_string);

        // Allow time to spin up
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;

        let Ok(status) = container.status().await else {
            return Err(eyre!("Failed to get status of container, Docker issue?"));
        };

        // Container is down, something's wrong.
        if !status.unwrap().is_active() {
            return Err(eyre!("Failed to start container, config error?"));
        }

        Ok(())
    }

    async fn revert_config(&self) -> Result<()> {
        tracing::error!("Container failed to start with new config, reverting");

        self.remove_existing_container().await?;

        let original_config_path = self.original_agent_config_path();
        if !original_config_path.exists() {
            // There is no config to revert
            return Err(eyre!("Config failed to apply, with no fallback"));
        }

        let config_path = self.agent_config_path();

        tracing::debug!(
            "Moving `{}` to `{}`",
            original_config_path.display(),
            config_path.display()
        );
        std::fs::rename(original_config_path, config_path)?;

        let original_relay_chains = self.original_relay_chains_path();
        if original_relay_chains.exists() {
            let relay_chains_path = self.relay_chains_path();
            tracing::debug!(
                "Moving `{}` to `{}`",
                original_relay_chains.display(),
                relay_chains_path.display(),
            );
            std::fs::rename(original_relay_chains, relay_chains_path)?;
        }

        self.spinup_container().await?;
        Ok(())
    }

    async fn remove_existing_container(&self) -> Result<()> {
        let mut container_id = self.container.lock().await;
        if let Some(container_id) = container_id.take() {
            tracing::warn!("Removing existing container...");
            let mut c = Container::from_id(&self.connection, container_id).await?;
            c.stop().await?;
            c.remove(None).await?;
        }

        Ok(())
    }

    fn hyperlane_db_path(&self) -> PathBuf {
        self.data_dir.join("hyperlane_db")
    }

    fn agent_config_path(&self) -> PathBuf {
        self.data_dir.join("agent-config.json")
    }

    fn relay_chains_path(&self) -> PathBuf {
        self.data_dir.join("relay_chains.txt")
    }

    fn original_agent_config_path(&self) -> PathBuf {
        self.data_dir.join("agent-config.json.orig")
    }

    fn original_relay_chains_path(&self) -> PathBuf {
        self.data_dir.join("relay_chains.txt.orig")
    }
}

#[sdk::job(
    id = 0,
    params(config, relay_chains),
    result(_),
    event_listener(
        listener = TangleEventListener<Arc<HyperlaneContext>, JobCalled>,
        pre_processor = services_pre_processor,
        post_processor = services_post_processor,
    ),
)]
pub async fn set_config(
    ctx: Arc<HyperlaneContext>,
    config: String,
    relay_chains: String,
) -> Result<u64> {
    // TODO: First step, verify the config is valid
    if relay_chains.is_empty() || !relay_chains.contains(',') {
        return Err(eyre!(
            "`relay_chains` is invalid, ensure it contains at least two chains"
        ));
    }

    ctx.remove_existing_container().await?;

    let config_path = ctx.agent_config_path();
    if config_path.exists() {
        let orig_config_path = ctx.original_agent_config_path();
        tracing::info!("Config path exists, overwriting.");
        std::fs::rename(&config_path, orig_config_path)?;
    }

    let relay_chains_path = ctx.relay_chains_path();
    if relay_chains_path.exists() {
        let orig_relay_chains_path = ctx.original_relay_chains_path();
        tracing::info!("Relay chains list exists, overwriting.");
        std::fs::rename(&relay_chains_path, orig_relay_chains_path)?;
    }

    // TODO: Make this optional
    if config == "TODO" {
        tracing::info!("No config provided, using defaults");
    } else {
        std::fs::write(&config_path, config)?;
        tracing::info!("New config written to: {}", config_path.display());
    }

    std::fs::write(&relay_chains_path, relay_chains)?;
    tracing::info!("Relay chains written to: {}", config_path.display());

    if ctx.spinup_container().await.is_ok() {
        return Ok(0);
    }

    // Something went wrong spinning up the container, possibly bad config. Try to revert.
    ctx.revert_config().await?;

    Ok(0)
}
