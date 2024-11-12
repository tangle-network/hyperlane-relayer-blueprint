# <h1 align="center"> Hyperlane Relayer Blueprint 🌐 </h1>

## 📚 Overview

This blueprint contains tasks for an operator to initialize and manage their
own [Hyperlane relayer](https://docs.hyperlane.xyz/docs/operate/overview-agents#relayer).

## 🚀 Features

This Blueprint provides the following key feature:

* Automated devops for running Hyperlane relayers
* Tangle Network integration for on-demand instancing of relayers

## 📋 Pre-requisites

* [Docker](https://docs.docker.com/engine/install/)
* [Docker Compose](https://docs.docker.com/compose/install/)
* [cargo-tangle](https://crates.io/crates/cargo-tangle)

## 💻 Usage

To use this blueprint:

1. Review the blueprint specifications in the `src/` directory.
2. Follow the [Hyperlane documentation](https://docs.hyperlane.xyz/docs/operate/relayer/run-relayer) to understand the
   relayer setup process.
3. Adapt the blueprint to your specific relayer configuration needs.
4. Deploy the blueprint on the Tangle Network using the Tangle CLI:

```shell
$ cargo tangle blueprint deploy
```

Upon deployment, the Blueprint will be able to be instanced and executed by any Tangle operator registered on the
blueprint.

### Starting a relayer

There are two ways to start a relayer:

1. With user-generated configs, and optional relay chains
2. With the [default configs](https://github.com/hyperlane-xyz/hyperlane-monorepo/tree/main/rust/main/config), and
   specified relay chains

Once you've determined which path to choose, you can call the `set_config` job.

#### Set config job

To spin up a relayer instance, use the `set_config` job:

This job will save the existing config, attempt to start the relayer with the new config, and on failure will spin back
up using the old config.

It has two parameters:

1. `config`: An optional config file, if not specified it will use
   the [defaults](https://github.com/hyperlane-xyz/hyperlane-monorepo/tree/main/rust/main/config).
2. `relay_chains`: A comma-separated list of origin and destination chains for relaying messages between.

**NOTE: Ensure that when using a manually specified config, `relayChains` is specified, either as a job parameter or in
the config itself**

## 🔗 External Links

- [Hyperlane Documentation](https://docs.hyperlane.xyz)
- [Tangle Network](https://www.tangle.tools/)

## 📜 License

Licensed under either of

* Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license
  ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## 📬 Feedback and Contributions

We welcome feedback and contributions to improve this blueprint.
Please open an issue or submit a pull request on our GitHub repository.
Please let us know if you fork this blueprint and extend it too!

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.