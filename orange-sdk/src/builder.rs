//! Builder for constructing [`Wallet`] instances with a fluent API.

#[cfg(feature = "spark")]
use crate::trusted_wallet::TrustedError;
use crate::{
	ChainSource, ExtraConfig, InitFailure, Seed, StorageConfig, Tunables, Wallet, WalletConfig,
};

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::{Network, io};
use ldk_node::lightning::ln::msgs::SocketAddress;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Runtime;

/// Represents possible errors during wallet builder configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuilderError {
	/// Storage configuration was not provided.
	MissingStorageConfig,
	/// Log file path was not provided.
	MissingLogFile,
	/// Chain source configuration was not provided.
	MissingChainSource,
	/// LSP configuration was not provided.
	MissingLsp,
	/// Network configuration was not provided.
	MissingNetwork,
	/// Seed was not provided.
	MissingSeed,
}

impl fmt::Display for BuilderError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			BuilderError::MissingStorageConfig => write!(f, "Storage configuration is required"),
			BuilderError::MissingLogFile => write!(f, "Log file path is required"),
			BuilderError::MissingChainSource => write!(f, "Chain source configuration is required"),
			BuilderError::MissingLsp => write!(f, "LSP configuration is required"),
			BuilderError::MissingNetwork => write!(f, "Network configuration is required"),
			BuilderError::MissingSeed => write!(f, "Seed is required"),
		}
	}
}

impl std::error::Error for BuilderError {}

impl From<BuilderError> for InitFailure {
	fn from(e: BuilderError) -> InitFailure {
		InitFailure::BuildError(e)
	}
}

/// Builder for constructing [`Wallet`] instances.
///
/// Provides a fluent API for setting up wallet configuration with sensible defaults.
///
/// # Example
///
/// ```rust,no_run
/// use orange_sdk::{WalletBuilder, Seed, ChainSource, StorageConfig};
/// use orange_sdk::trusted_wallet::dummy::{DummyTrustedWalletExtraConfig};
/// use orange_sdk::bitcoin::Network;
/// use ldk_node::bip39::Mnemonic;
/// use ldk_node::lightning::ln::msgs::SocketAddress;
/// use std::sync::Arc;
/// use uuid::Uuid;
///
/// # async fn example() -> Result<(), orange_sdk::InitFailure> {
/// let runtime = Arc::new(tokio::runtime::Runtime::new().unwrap());
/// let mnemonic = Mnemonic::from_entropy(&[0; 16]).unwrap(); // Actual entropy should be random
/// let seed = Seed::Mnemonic { mnemonic, passphrase: None };
/// let node_id = "02f7467f4de732f3b3cffc8d5e007aecdf6e58878edb6e46a8e80164421c1b90aa".parse().unwrap();
/// let socket_addr = SocketAddress::TcpIpV4 { addr: [127, 0, 0, 1], port: 9735 };
///
/// // Note: In practice, you'd need actual LSP and Bitcoin node instances for the wallet to function.
/// let wallet = WalletBuilder::new()
///     .seed(seed)
///     .network(Network::Bitcoin)
///     .storage_config(StorageConfig::LocalSQLite("/path/to/wallet.db".to_string()))
///     .log_file("/path/to/wallet.log".into())
///     .chain_source(ChainSource::Electrum("ssl://electrum.blockstream.info:60002".to_string()))
///     .lsp(node_id, socket_addr, None)
///     .build_spark(None)
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct WalletBuilder {
	runtime: Option<Arc<Runtime>>,
	storage_config: Option<StorageConfig>,
	log_file: Option<PathBuf>,
	chain_source: Option<ChainSource>,
	lsp: Option<(SocketAddress, PublicKey, Option<String>)>,
	scorer_url: Option<String>,
	network: Option<Network>,
	seed: Option<Seed>,
	tunables: Option<Tunables>,
}

impl WalletBuilder {
	/// Creates a new [`WalletBuilder`] with the provided tokio runtime.
	///
	/// # Arguments
	///
	/// * `runtime` - An Arc-wrapped tokio Runtime for async operations
	pub fn new() -> Self {
		Self {
			runtime: None,
			storage_config: None,
			log_file: None,
			chain_source: None,
			lsp: None,
			scorer_url: None,
			network: None,
			seed: None,
			tunables: None,
		}
	}

	/// Sets a custom tokio runtime for the wallet.
	///
	/// # Arguments
	///
	/// * `rt` - A tokio Runtime instance to be used by the wallet
	pub fn runtime(mut self, rt: Arc<Runtime>) -> Self {
		self.runtime = Some(rt);
		self
	}

	/// Sets the storage configuration for the wallet.
	///
	/// # Arguments
	///
	/// * `storage_config` - Configuration for wallet storage (local SQLite or VSS)
	pub fn storage_config(mut self, storage_config: StorageConfig) -> Self {
		self.storage_config = Some(storage_config);
		self
	}

	/// Sets the log file path for the wallet.
	///
	/// # Arguments
	///
	/// * `log_file` - Path where wallet logs will be written
	pub fn log_file(mut self, log_file: PathBuf) -> Self {
		self.log_file = Some(log_file);
		self
	}

	/// Sets the blockchain data source for the wallet.
	///
	/// # Arguments
	///
	/// * `chain_source` - Configuration for blockchain data source (Electrum, Esplora, or Bitcoin Core RPC)
	pub fn chain_source(mut self, chain_source: ChainSource) -> Self {
		self.chain_source = Some(chain_source);
		self
	}

	/// Sets the Lightning Service Provider (LSP) configuration.
	///
	/// # Arguments
	///
	/// * `node_id` - The LSP node's public key
	/// * `address` - The network address to connect to the LSP
	/// * `auth_token` - Optional authentication token for the LSP
	pub fn lsp(
		mut self, node_id: PublicKey, address: SocketAddress, auth_token: Option<String>,
	) -> Self {
		self.lsp = Some((address, node_id, auth_token));
		self
	}

	/// Sets the scorer URL for lightning routing optimization.
	///
	/// # Arguments
	///
	/// * `scorer_url` - URL to download route scorer data from
	pub fn scorer_url(mut self, scorer_url: String) -> Self {
		self.scorer_url = Some(scorer_url);
		self
	}

	/// Sets the Bitcoin network for the wallet.
	///
	/// # Arguments
	///
	/// * `network` - Bitcoin network (Mainnet, Testnet, Signet, or Regtest)
	pub fn network(mut self, network: Network) -> Self {
		self.network = Some(network);
		self
	}

	/// Sets the seed for wallet generation.
	///
	/// # Arguments
	///
	/// * `seed` - Seed for deterministic wallet generation (mnemonic or 64-byte seed)
	pub fn seed(mut self, seed: Seed) -> Self {
		self.seed = Some(seed);
		self
	}

	/// Sets the tunables for wallet behavior configuration.
	///
	/// # Arguments
	///
	/// * `tunables` - Configuration parameters controlling wallet behavior
	pub fn tunables(mut self, tunables: Tunables) -> Self {
		self.tunables = Some(tunables);
		self
	}

	/// Builds and initializes the wallet with the configured parameters.
	///
	/// # Type Parameters
	///
	/// * `T` - The trusted wallet implementation type
	///
	/// # Arguments
	///
	/// * `extra_config` - Implementation-specific configuration for the trusted wallet
	///
	/// # Returns
	///
	/// Returns a [`Result`] containing the initialized [`Wallet`] on success, or an [`InitFailure`] on error.
	///
	/// # Errors
	///
	/// This method will return an error if:
	/// - Required configuration is missing
	/// - Storage initialization fails
	/// - Network connection fails
	/// - Trusted wallet initialization fails
	/// - LDK node fails to start
	pub async fn build(self, extra_config: ExtraConfig) -> Result<Wallet, InitFailure> {
		let config = WalletConfig {
			storage_config: self.storage_config.ok_or(BuilderError::MissingStorageConfig)?,
			log_file: self.log_file.ok_or(BuilderError::MissingLogFile)?,
			chain_source: self.chain_source.ok_or(BuilderError::MissingChainSource)?,
			lsp: self.lsp.ok_or(BuilderError::MissingLsp)?,
			scorer_url: self.scorer_url,
			network: self.network.ok_or(BuilderError::MissingNetwork)?,
			seed: self.seed.ok_or(BuilderError::MissingSeed)?,
			tunables: self.tunables.unwrap_or_default(),
			extra_config,
		};

		let runtime = match self.runtime {
			Some(runtime) => runtime,
			None => {
				let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(
					|_| {
						InitFailure::IoError(io::Error::new(
							io::ErrorKind::Other,
							"Failed to create tokio runtime",
						))
					},
				)?;
				Arc::new(rt)
			},
		};

		Wallet::new(runtime, config).await
	}

	/// Builds and initializes the wallet with the configured parameters for Spark wallet.
	///
	/// # Type Parameters
	///
	/// * `T` - The trusted wallet implementation type
	///
	/// # Returns
	///
	/// Returns a [`Result`] containing the initialized [`Wallet`] on success, or an [`InitFailure`] on error.
	///
	/// # Errors
	///
	/// This method will return an error if:
	/// - Required configuration is missing
	/// - Storage initialization fails
	/// - Network connection fails
	/// - Trusted wallet initialization fails
	/// - LDK node fails to start
	#[cfg(feature = "spark")]
	pub async fn build_spark(
		self, spark_config: Option<spark_wallet::SparkWalletConfig>,
	) -> Result<Wallet, InitFailure> {
		let spark_config = match spark_config {
			Some(cfg) => cfg,
			None => {
				let network = self.network.ok_or(BuilderError::MissingNetwork)?;
				spark_wallet::SparkWalletConfig::default_config(
					network
						.try_into()
						.map_err(|_| InitFailure::TrustedFailure(TrustedError::InvalidNetwork))?,
				)
			},
		};

		let runtime = match self.runtime {
			Some(runtime) => runtime,
			None => {
				let rt = tokio::runtime::Builder::new_multi_thread()
					.enable_all()
					.build()
					.map_err(|e| InitFailure::IoError(e.into()))?;
				Arc::new(rt)
			},
		};

		let config = WalletConfig {
			storage_config: self.storage_config.ok_or(BuilderError::MissingStorageConfig)?,
			log_file: self.log_file.ok_or(BuilderError::MissingLogFile)?,
			chain_source: self.chain_source.ok_or(BuilderError::MissingChainSource)?,
			lsp: self.lsp.ok_or(BuilderError::MissingLsp)?,
			scorer_url: self.scorer_url,
			network: self.network.ok_or(BuilderError::MissingNetwork)?,
			seed: self.seed.ok_or(BuilderError::MissingSeed)?,
			tunables: self.tunables.unwrap_or_default(),
			extra_config: ExtraConfig::Spark(spark_config),
		};

		Wallet::new(runtime, config).await
	}

	/// Builds and initializes the wallet with the configured parameters for Cashu wallet.
	///
	/// # Type Parameters
	///
	/// * `T` - The trusted wallet implementation type
	///
	/// # Returns
	///
	/// Returns a [`Result`] containing the initialized [`Wallet`] on success, or an [`InitFailure`] on error.
	///
	/// # Errors
	///
	/// This method will return an error if:
	/// - Required configuration is missing
	/// - Storage initialization fails
	/// - Network connection fails
	/// - Trusted wallet initialization fails
	/// - LDK node fails to start
	#[cfg(feature = "cashu")]
	pub async fn build_cashu(
		self, mint_url: String, unit: Option<cdk::nuts::CurrencyUnit>,
	) -> Result<Wallet, InitFailure> {
		let cashu_config =
			crate::CashuConfig { mint_url, unit: unit.unwrap_or(cdk::nuts::CurrencyUnit::Sat) };

		let runtime = match self.runtime {
			Some(runtime) => runtime,
			None => {
				let rt = tokio::runtime::Builder::new_multi_thread()
					.enable_all()
					.build()
					.map_err(|e| InitFailure::IoError(e.into()))?;
				Arc::new(rt)
			},
		};

		let config = WalletConfig {
			storage_config: self.storage_config.ok_or(BuilderError::MissingStorageConfig)?,
			log_file: self.log_file.ok_or(BuilderError::MissingLogFile)?,
			chain_source: self.chain_source.ok_or(BuilderError::MissingChainSource)?,
			lsp: self.lsp.ok_or(BuilderError::MissingLsp)?,
			scorer_url: self.scorer_url,
			network: self.network.ok_or(BuilderError::MissingNetwork)?,
			seed: self.seed.ok_or(BuilderError::MissingSeed)?,
			tunables: self.tunables.unwrap_or_default(),
			extra_config: ExtraConfig::Cashu(cashu_config),
		};

		Wallet::new(runtime, config).await
	}
}
