use blueprint_sdk::build;
use blueprint_sdk::tangle::blueprint;
use hyperlane_relayer_blueprint_lib::set_config;
use std::path::Path;
use std::process;

fn main() {
    let contracts_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("contracts");

    let contract_dirs: Vec<&str> = vec![contracts_dir.to_str().unwrap()];
    build::utils::soldeer_install();
    build::utils::soldeer_update();
    build::utils::build_contracts(contract_dirs);

    println!("cargo::rerun-if-changed=../src");

    let blueprint = blueprint! {
        name: "experiment",
        master_manager_revision: "Latest",
        manager: { Evm = "HyperlaneRelayerBlueprint" },
        jobs: [set_config]
    };

    match blueprint {
        Ok(blueprint) => {
            // TODO: Should be a helper function probably
            let json = blueprint_sdk::tangle::metadata::macros::ext::serde_json::to_string_pretty(
                &blueprint,
            )
            .unwrap();
            std::fs::write(
                Path::new(env!("CARGO_WORKSPACE_DIR")).join("blueprint.json"),
                json.as_bytes(),
            )
            .unwrap();
        }
        Err(e) => {
            println!("cargo::error={e:?}");
            process::exit(1);
        }
    }
}
