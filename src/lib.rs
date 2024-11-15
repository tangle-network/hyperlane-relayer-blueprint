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

        let hyperlane_db_path = self.hyperlane_db_path();
        if !hyperlane_db_path.exists() {
            tracing::warn!("Hyperlane DB does not exist, creating...");
            std::fs::create_dir_all(&hyperlane_db_path)?;
            tracing::info!("Hyperlane DB created at `{}`", hyperlane_db_path.display());
        }

        let mut binds = vec![format!("{}:/hyperlane_db", hyperlane_db_path.display())];

        let agent_configs_path = self.agent_configs_path();
        let agent_configs_path_exists = agent_configs_path.exists();
        if agent_configs_path_exists {
            binds.push(format!(
                "{}:/config:ro",
                agent_configs_path.to_string_lossy()
            ));
        }

        let mut env = Vec::new();

        if agent_configs_path_exists {
            let mut config_files = Vec::new();

            let files = std::fs::read_dir(agent_configs_path)?;
            for config in files {
                let path = config?.path();
                if path.is_file() {
                    config_files.push(path.to_string_lossy().to_string());
                }
            }

            env.push(format!("CONFIG_FILES={}", config_files.join(",")));
        }

        let relay_chains_path = self.relay_chains_path();
        if relay_chains_path.exists() {
            let relay_chains = std::fs::read_to_string(relay_chains_path)?;
            env.push(format!("HYP_RELAYCHAINS={relay_chains}"));
        }

        container.env(env).binds(binds).cmd([
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

    async fn revert_configs(&self) -> Result<()> {
        tracing::error!("Container failed to start with new configs, reverting");

        self.remove_existing_container().await?;

        let original_configs_path = self.original_agent_configs_path();
        if !original_configs_path.exists() {
            // There is no config to revert
            return Err(eyre!("Configs failed to apply, with no fallback"));
        }

        let configs_path = self.agent_configs_path();

        tracing::debug!(
            "Moving `{}` to `{}`",
            original_configs_path.display(),
            configs_path.display()
        );
        std::fs::rename(original_configs_path, configs_path)?;

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

    fn agent_configs_path(&self) -> PathBuf {
        self.data_dir.join("agent_configs")
    }

    fn relay_chains_path(&self) -> PathBuf {
        self.data_dir.join("relay_chains.txt")
    }

    fn original_agent_configs_path(&self) -> PathBuf {
        self.data_dir.join("agent_configs.orig")
    }

    fn original_relay_chains_path(&self) -> PathBuf {
        self.data_dir.join("relay_chains.txt.orig")
    }
}

#[sdk::job(
    id = 0,
    params(configs, relay_chains),
    result(_),
    event_listener(
        listener = TangleEventListener<Arc<HyperlaneContext>, JobCalled>,
        pre_processor = services_pre_processor,
        post_processor = services_post_processor,
    ),
)]
pub async fn set_config(
    ctx: Arc<HyperlaneContext>,
    configs: Option<Vec<String>>,
    relay_chains: String,
) -> Result<u64> {
    // TODO: First step, verify the config is valid. Is there an easy way to do so?
    if relay_chains.is_empty() || !relay_chains.contains(',') {
        return Err(eyre!(
            "`relay_chains` is invalid, ensure it contains at least two chains"
        ));
    }

    ctx.remove_existing_container().await?;

    let configs_path = ctx.agent_configs_path();
    if configs_path.exists() {
        let orig_configs_path = ctx.original_agent_configs_path();
        tracing::info!("Configs path exists, overwriting.");
        std::fs::rename(&configs_path, orig_configs_path)?;
    }

    let relay_chains_path = ctx.relay_chains_path();
    if relay_chains_path.exists() {
        let orig_relay_chains_path = ctx.original_relay_chains_path();
        tracing::info!("Relay chains list exists, overwriting.");
        std::fs::rename(&relay_chains_path, orig_relay_chains_path)?;
    }

    match configs {
        Some(configs) if !configs.is_empty() => {
            // TODO: Limit number of configs?
            for (index, config) in configs.iter().enumerate() {
                std::fs::write(configs_path.join(format!("{index}.json")), config)?;
            }
            tracing::info!("New configs written to: {}", configs_path.display());
        }
        _ => tracing::info!("No configs provided, using defaults"),
    }

    std::fs::write(&relay_chains_path, relay_chains)?;
    tracing::info!("Relay chains written to: {}", configs_path.display());

    if ctx.spinup_container().await.is_ok() {
        return Ok(0);
    }

    // Something went wrong spinning up the container, possibly bad config. Try to revert.
    ctx.revert_configs().await?;

    Ok(0)
}
