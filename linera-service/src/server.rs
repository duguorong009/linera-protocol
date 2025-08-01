// Copyright (c) Facebook, Inc. and its affiliates.
// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::large_futures)]

use std::{
    borrow::Cow,
    num::NonZeroU16,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context};
use async_trait::async_trait;
use futures::{stream::FuturesUnordered, FutureExt as _, StreamExt, TryFutureExt as _};
use linera_base::{
    crypto::{CryptoRng, Ed25519SecretKey},
    listen_for_shutdown_signals,
};
use linera_client::config::{CommitteeConfig, ValidatorConfig, ValidatorServerConfig};
use linera_core::{worker::WorkerState, JoinSetExt as _};
use linera_execution::{WasmRuntime, WithWasmDefault};
#[cfg(with_metrics)]
use linera_metrics::prometheus_server;
use linera_persistent::{self as persistent, Persist};
use linera_rpc::{
    config::{
        CrossChainConfig, ExporterServiceConfig, NetworkProtocol, NotificationConfig, ProxyConfig,
        ShardConfig, ShardId, TlsConfig, ValidatorInternalNetworkConfig,
        ValidatorPublicNetworkConfig,
    },
    grpc, simple,
};
use linera_sdk::linera_base_types::{AccountSecretKey, ValidatorKeypair};
use linera_service::{
    storage::{CommonStorageOptions, Runnable, StorageConfig},
    util,
};
use linera_storage::Storage;
use serde::Deserialize;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

struct ServerContext {
    server_config: ValidatorServerConfig,
    cross_chain_config: CrossChainConfig,
    notification_config: NotificationConfig,
    shard: Option<usize>,
    grace_period: Duration,
}

impl ServerContext {
    fn make_shard_state<S>(
        &self,
        local_ip_addr: &str,
        shard_id: ShardId,
        storage: S,
    ) -> (WorkerState<S>, ShardId, ShardConfig)
    where
        S: Storage + Clone + Send + Sync + 'static,
    {
        let shard = self.server_config.internal_network.shard(shard_id);
        info!("Shard booted on {}", shard.host);
        info!(
            "Public key: {}",
            self.server_config.validator_secret.public()
        );
        let state = WorkerState::new(
            format!("Shard {} @ {}:{}", shard_id, local_ip_addr, shard.port),
            Some(self.server_config.validator_secret.copy()),
            storage,
        )
        .with_allow_inactive_chains(false)
        .with_allow_messages_from_deprecated_epochs(false)
        .with_grace_period(self.grace_period);
        (state, shard_id, shard.clone())
    }

    fn spawn_simple<S>(
        &self,
        listen_address: &str,
        states: Vec<(WorkerState<S>, ShardId, ShardConfig)>,
        protocol: simple::TransportProtocol,
        shutdown_signal: CancellationToken,
    ) -> JoinSet<()>
    where
        S: Storage + Clone + Send + Sync + 'static,
    {
        let mut join_set = JoinSet::new();
        let handles = FuturesUnordered::new();

        let internal_network = self
            .server_config
            .internal_network
            .clone_with_protocol(protocol);

        for (state, shard_id, shard) in states {
            let internal_network = internal_network.clone();
            let cross_chain_config = self.cross_chain_config.clone();
            let listen_address = listen_address.to_owned();

            #[cfg(with_metrics)]
            if let Some(port) = shard.metrics_port {
                Self::start_metrics(&listen_address, port, shutdown_signal.clone());
            }

            let server_handle = simple::Server::new(
                internal_network,
                listen_address,
                shard.port,
                state,
                shard_id,
                cross_chain_config,
            )
            .spawn(shutdown_signal.clone(), &mut join_set);

            handles.push(
                server_handle
                    .join()
                    .inspect_err(move |error| {
                        error!("Error running server for shard {shard_id}: {error:?}")
                    })
                    .map(|_| ()),
            );
        }

        join_set.spawn_task(handles.collect::<()>());

        join_set
    }

    fn spawn_grpc<S>(
        &self,
        listen_address: &str,
        states: Vec<(WorkerState<S>, ShardId, ShardConfig)>,
        shutdown_signal: CancellationToken,
    ) -> JoinSet<()>
    where
        S: Storage + Clone + Send + Sync + 'static,
    {
        let mut join_set = JoinSet::new();
        let handles = FuturesUnordered::new();

        for (state, shard_id, shard) in states {
            #[cfg(with_metrics)]
            if let Some(port) = shard.metrics_port {
                Self::start_metrics(listen_address, port, shutdown_signal.clone());
            }

            let server_handle = grpc::GrpcServer::spawn(
                listen_address.to_string(),
                shard.port,
                state,
                shard_id,
                self.server_config.internal_network.clone(),
                self.cross_chain_config.clone(),
                self.notification_config.clone(),
                shutdown_signal.clone(),
                &mut join_set,
            );

            handles.push(
                server_handle
                    .join()
                    .inspect_err(move |error| {
                        error!("Error running server for shard {shard_id}: {error:?}")
                    })
                    .map(|_| ()),
            );
        }

        join_set.spawn_task(handles.collect::<()>());

        join_set
    }

    #[cfg(with_metrics)]
    fn start_metrics(host: &str, port: u16, shutdown_signal: CancellationToken) {
        prometheus_server::start_metrics((host.to_owned(), port), shutdown_signal);
    }

    fn get_listen_address(&self) -> String {
        // Allow local IP address to be different from the public one.
        "0.0.0.0".to_string()
    }
}

#[async_trait]
impl Runnable for ServerContext {
    type Output = anyhow::Result<()>;

    async fn run<S>(self, storage: S) -> anyhow::Result<()>
    where
        S: Storage + Clone + Send + Sync + 'static,
    {
        let shutdown_notifier = CancellationToken::new();
        let listen_address = self.get_listen_address();

        tokio::spawn(listen_for_shutdown_signals(shutdown_notifier.clone()));

        // Run the server
        let states = match self.shard {
            Some(shard) => {
                info!("Running shard number {}", shard);
                vec![self.make_shard_state(&listen_address, shard, storage)]
            }
            None => {
                info!("Running all shards");
                let num_shards = self.server_config.internal_network.shards.len();
                (0..num_shards)
                    .map(|shard| self.make_shard_state(&listen_address, shard, storage.clone()))
                    .collect()
            }
        };

        let mut join_set = match self.server_config.internal_network.protocol {
            NetworkProtocol::Simple(protocol) => {
                self.spawn_simple(&listen_address, states, protocol, shutdown_notifier)
            }
            NetworkProtocol::Grpc(tls_config) => match tls_config {
                TlsConfig::ClearText => self.spawn_grpc(&listen_address, states, shutdown_notifier),
                TlsConfig::Tls => bail!("TLS not supported between proxy and shards."),
            },
        };

        join_set.await_all_tasks().await;

        Ok(())
    }
}

#[derive(clap::Parser)]
#[command(
    name = "linera-server",
    about = "A byzantine fault tolerant payments sidechain with low-latency finality and high throughput",
    version = linera_version::VersionInfo::default_clap_str(),
)]
struct ServerOptions {
    /// Subcommands. Acceptable values are run and generate.
    #[command(subcommand)]
    command: ServerCommand,

    /// The number of Tokio worker threads to use.
    #[arg(long, env = "LINERA_SERVER_TOKIO_THREADS")]
    tokio_threads: Option<usize>,

    /// The number of Tokio blocking threads to use.
    #[arg(long, env = "LINERA_SERVER_TOKIO_BLOCKING_THREADS")]
    tokio_blocking_threads: Option<usize>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct ValidatorOptions {
    /// Path to the file containing the server configuration of this Linera validator (including its secret key)
    server_config_path: PathBuf,

    /// The host of the validator (IP address or hostname)
    host: String,

    /// The port of the validator
    port: u16,

    /// The server configurations for the linera-exporter.
    #[serde(default)]
    block_exporters: Vec<ExporterServiceConfig>,

    /// The network protocol for the frontend.
    external_protocol: NetworkProtocol,

    /// The network protocol for workers.
    internal_protocol: NetworkProtocol,

    /// The public name and the port of each of the shards
    shards: Vec<ShardConfig>,

    /// The name and the port of the proxies
    proxies: Vec<ProxyConfig>,
}

fn make_server_config<R: CryptoRng>(
    path: &Path,
    rng: &mut R,
    options: ValidatorOptions,
) -> anyhow::Result<persistent::File<ValidatorServerConfig>> {
    let validator_keypair = ValidatorKeypair::generate_from(rng);
    let account_secret = AccountSecretKey::Ed25519(Ed25519SecretKey::generate_from(rng));
    let public_key = validator_keypair.public_key;
    let network = ValidatorPublicNetworkConfig {
        protocol: options.external_protocol,
        host: options.host,
        port: options.port,
    };
    let internal_network = ValidatorInternalNetworkConfig {
        public_key,
        protocol: options.internal_protocol,
        shards: options.shards,
        block_exporters: options.block_exporters,
        proxies: options.proxies,
    };
    let validator = ValidatorConfig {
        network,
        public_key,
        account_key: account_secret.public(),
    };
    Ok(persistent::File::new(
        path,
        ValidatorServerConfig {
            validator,
            validator_secret: validator_keypair.secret_key,
            internal_network,
        },
    )?)
}

#[derive(clap::Parser)]
enum ServerCommand {
    /// Runs a service for each shard of the Linera validator")
    #[command(name = "run")]
    Run {
        /// Path to the file containing the server configuration of this Linera validator (including its secret key)
        #[arg(long = "server")]
        server_config_path: PathBuf,

        /// Storage configuration for the blockchain history, chain states and binary blobs.
        #[arg(long = "storage")]
        storage_config: StorageConfig,

        /// Common storage options.
        #[command(flatten)]
        common_storage_options: CommonStorageOptions,

        /// Configuration for cross-chain requests
        #[command(flatten)]
        cross_chain_config: CrossChainConfig,

        /// Configuration for notifications
        #[command(flatten)]
        notification_config: NotificationConfig,

        /// Runs a specific shard (from 0 to shards-1)
        #[arg(long)]
        shard: Option<usize>,

        /// Blocks with a timestamp this far in the future will still be accepted, but the validator
        /// will wait until that timestamp before voting.
        #[arg(long = "grace-period-ms", default_value = "500", value_parser = util::parse_millis)]
        grace_period: Duration,

        /// The WebAssembly runtime to use.
        #[arg(long)]
        wasm_runtime: Option<WasmRuntime>,
    },

    /// Act as a trusted third-party and generate all server configurations
    #[command(name = "generate")]
    Generate {
        /// Configuration file of each validator in the committee
        #[arg(long, num_args(0..))]
        validators: Vec<PathBuf>,

        /// Path where to write the description of the Linera committee
        #[arg(long)]
        committee: Option<PathBuf>,

        /// Force this command to generate keys using a PRNG and a given seed. USE FOR
        /// TESTING ONLY.
        #[arg(long)]
        testing_prng_seed: Option<u64>,
    },

    /// Replaces the configurations of the shards by following the given template.
    #[command(name = "edit-shards")]
    EditShards {
        /// Path to the file containing the server configuration of this Linera validator.
        #[arg(long = "server")]
        server_config_path: PathBuf,

        /// The number N of shard configs to generate, possibly starting with zeroes. If
        /// `N` was written using `D` digits, we will replace the first occurrence of the
        /// string `"%" * D` (`%` repeated D times) by the shard number.
        #[arg(long)]
        num_shards: String,

        /// The host of the validator (IP address or hostname), possibly containing `%`
        /// for digits of the shard number.
        #[arg(long)]
        host: String,

        /// The port of the main endpoint, possibly containing `%` for digits of the shard
        /// number.
        #[arg(long)]
        port: String,

        /// The port for the metrics endpoint, possibly containing `%` for digits of the
        /// shard number.
        #[arg(long)]
        metrics_port: Option<String>,
    },
}

fn main() {
    let options = <ServerOptions as clap::Parser>::parse();

    linera_base::tracing::init(&log_file_name_for(&options.command));

    let mut runtime = if options.tokio_threads == Some(1) {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();

        if let Some(threads) = options.tokio_threads {
            builder.worker_threads(threads);
        }

        builder
    };

    if let Some(blocking_threads) = options.tokio_blocking_threads {
        runtime.max_blocking_threads(blocking_threads);
    }

    runtime
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime")
        .block_on(run(options))
}

/// Returns the log file name to use based on the [`ServerCommand`] that will run.
fn log_file_name_for(command: &ServerCommand) -> Cow<'static, str> {
    match command {
        ServerCommand::Run {
            shard,
            server_config_path,
            ..
        } => {
            let server_config: ValidatorServerConfig =
                util::read_json(server_config_path).expect("Failed to read server config");
            let public_key = &server_config.validator.public_key;

            if let Some(shard) = shard {
                format!("validator-{public_key}-shard-{shard}")
            } else {
                format!("validator-{public_key}")
            }
            .into()
        }
        ServerCommand::Generate { .. } | ServerCommand::EditShards { .. } => "server".into(),
    }
}

async fn run(options: ServerOptions) {
    match options.command {
        ServerCommand::Run {
            server_config_path,
            storage_config,
            common_storage_options,
            cross_chain_config,
            notification_config,
            shard,
            grace_period,
            wasm_runtime,
        } => {
            linera_version::VERSION_INFO.log();

            let server_config: ValidatorServerConfig =
                util::read_json(&server_config_path).expect("Failed to read server config");

            let job = ServerContext {
                server_config,
                cross_chain_config,
                notification_config,
                shard,
                grace_period,
            };
            let wasm_runtime = wasm_runtime.with_wasm_default();
            let store_config = storage_config
                .add_common_storage_options(&common_storage_options)
                .await
                .unwrap();
            store_config
                .run_with_storage(wasm_runtime, job)
                .boxed()
                .await
                .unwrap()
                .unwrap();
        }

        ServerCommand::Generate {
            validators,
            committee,
            testing_prng_seed,
        } => {
            let mut config_validators = Vec::new();
            let mut rng = Box::<dyn CryptoRng>::from(testing_prng_seed);
            for options_path in validators {
                let options_string = fs_err::tokio::read_to_string(options_path)
                    .await
                    .expect("Unable to read validator options file");
                let options: ValidatorOptions =
                    toml::from_str(&options_string).unwrap_or_else(|_| {
                        panic!("Invalid options file format: \n {}", options_string)
                    });
                let path = options.server_config_path.clone();
                let mut server = make_server_config(&path, &mut rng, options)
                    .expect("Unable to open server config file");
                Persist::persist(&mut server)
                    .await
                    .expect("Unable to write server config file");
                info!("Wrote server config {}", path.to_str().unwrap());
                println!(
                    "{},{}",
                    server.validator.public_key, server.validator.account_key
                );
                config_validators.push(Persist::into_value(server).validator);
            }
            if let Some(committee) = committee {
                let mut config = persistent::File::new(
                    &committee,
                    CommitteeConfig {
                        validators: config_validators,
                    },
                )
                .expect("Unable to open committee configuration");
                Persist::persist(&mut config)
                    .await
                    .expect("Unable to write committee description");
                info!("Wrote committee config {}", committee.to_str().unwrap());
            }
        }

        ServerCommand::EditShards {
            server_config_path,
            num_shards,
            host,
            port,
            metrics_port,
        } => {
            let mut server_config =
                persistent::File::<ValidatorServerConfig>::read(&server_config_path)
                    .expect("Failed to read server config");
            let shards = generate_shard_configs(num_shards, host, port, metrics_port)
                .expect("Failed to generate shard configs");
            server_config.internal_network.shards = shards;
            Persist::persist(&mut server_config)
                .await
                .expect("Failed to write updated server config");
        }
    }
}

fn generate_shard_configs(
    num_shards: String,
    host: String,
    port: String,
    metrics_port: Option<String>,
) -> anyhow::Result<Vec<ShardConfig>> {
    let mut shards = Vec::new();
    let len = num_shards.len();
    let num_shards = num_shards
        .parse::<NonZeroU16>()
        .context("Failed to parse the number of shards")?;
    let pattern = "%".repeat(len);

    for i in 1u16..=num_shards.into() {
        let index = format!("{i:0len$}");
        let host = host.replacen(&pattern, &index, 1);
        let port = port
            .replacen(&pattern, &index, 1)
            .parse()
            .context("Failed to decode port into an integers")?;
        let metrics_port = metrics_port
            .as_ref()
            .map(|port| {
                port.replacen(&pattern, &index, 1)
                    .parse()
                    .context("Failed to decode metrics port into an integers")
            })
            .transpose()?;
        let shard = ShardConfig {
            host,
            port,
            metrics_port,
        };
        shards.push(shard);
    }
    Ok(shards)
}

#[cfg(test)]
mod test {
    use linera_rpc::simple::TransportProtocol;

    use super::*;

    #[test]
    fn test_validator_options() {
        let toml_str = r#"
            server_config_path = "server.json"
            host = "host"
            port = 9000
            external_protocol = { Simple = "Tcp" }
            internal_protocol = { Simple = "Udp" }

            [[proxies]]
            host = "proxy"
            public_port = 20100
            private_port = 20200
            metrics_host = "proxy"
            metrics_port = 21100

            [[shards]]
            host = "host1"
            port = 9001
            metrics_port = 5001

            [[shards]]
            host = "host2"
            port = 9002
            metrics_port = 5002

            [[block_exporters]]
            host = "exporter"
            port = 12000
        "#;
        let options: ValidatorOptions = toml::from_str(toml_str).unwrap();
        assert_eq!(
            options,
            ValidatorOptions {
                server_config_path: "server.json".into(),
                external_protocol: NetworkProtocol::Simple(TransportProtocol::Tcp),
                internal_protocol: NetworkProtocol::Simple(TransportProtocol::Udp),
                host: "host".into(),
                port: 9000,
                proxies: vec![ProxyConfig {
                    host: "proxy".into(),
                    public_port: 20100,
                    private_port: 20200,
                    metrics_port: 21100,
                }],
                block_exporters: vec![ExporterServiceConfig {
                    host: "exporter".into(),
                    port: 12000
                }],
                shards: vec![
                    ShardConfig {
                        host: "host1".into(),
                        port: 9001,
                        metrics_port: Some(5001),
                    },
                    ShardConfig {
                        host: "host2".into(),
                        port: 9002,
                        metrics_port: Some(5002),
                    },
                ],
            }
        );
    }

    #[test]
    fn test_generate_shard_configs() {
        assert_eq!(
            generate_shard_configs(
                "02".into(),
                "host%%".into(),
                "10%%".into(),
                Some("11%%".into())
            )
            .unwrap(),
            vec![
                ShardConfig {
                    host: "host01".into(),
                    port: 1001,
                    metrics_port: Some(1101),
                },
                ShardConfig {
                    host: "host02".into(),
                    port: 1002,
                    metrics_port: Some(1102),
                },
            ],
        );

        assert!(generate_shard_configs(
            "2".into(),
            "host%%".into(),
            "10%%".into(),
            Some("11%%".into())
        )
        .is_err());
    }
}
