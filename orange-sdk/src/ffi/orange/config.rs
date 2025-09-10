use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;

use crate::ChainSource as OrangeChainSource;
use crate::Seed as OrangeSeed;
use crate::StorageConfig as OrangeStorageConfig;
use crate::Tunables as OrangeTunables;
use crate::VssAuth as OrangeVssAuth;
use crate::VssConfig as OrangeVssConfig;
use crate::WalletConfig as OrangeWalletConfig;
use crate::ffi::Network;
use crate::ffi::bitcoin_payment_instructions::Amount;
use crate::ffi::ldk_node::Mnemonic;
use crate::ffi::orange::error::ConfigError;
use crate::trusted_wallet::ExtraConfig as OrangeExtraConfig;
use crate::{impl_from_core_type, impl_into_core_type};

#[derive(Debug, Clone, uniffi::Enum)]
pub enum Seed {
	/// A BIP 39 mnemonic seed.
	MnemonicSeed {
		/// The mnemonic phrase.
		mnemonic: Arc<Mnemonic>,
		/// The passphrase for the mnemonic.
		passphrase: Option<String>,
	},
	/// A 64-byte seed for the wallet.
	Seed64(Vec<u8>),
}

impl TryInto<OrangeSeed> for Seed {
	type Error = ConfigError;
	fn try_into(self) -> Result<OrangeSeed, Self::Error> {
		match self {
			Seed::MnemonicSeed { mnemonic, passphrase } => {
				Ok(OrangeSeed::Mnemonic { mnemonic: mnemonic.0.clone(), passphrase })
			},
			Seed::Seed64(bytes) => {
				let bytes = bytes
					.clone()
					.try_into()
					.map_err(|_| ConfigError::InvalidEntropySize(bytes.len() as u32))?;
				Ok(OrangeSeed::Seed64(bytes))
			},
		}
	}
}

/// Represents the authentication method for a Versioned Storage Service (VSS).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum VssAuth {
	/// Authentication using an LNURL-auth server.
	LNURLAuthServer(String),
	/// Authentication using a fixed set of HTTP headers.
	FixedHeaders(HashMap<String, String>),
}

impl From<VssAuth> for OrangeVssAuth {
	fn from(auth: VssAuth) -> Self {
		match auth {
			VssAuth::LNURLAuthServer(url) => OrangeVssAuth::LNURLAuthServer(url),
			VssAuth::FixedHeaders(headers) => OrangeVssAuth::FixedHeaders(headers),
		}
	}
}

impl From<OrangeVssAuth> for VssAuth {
	fn from(auth: OrangeVssAuth) -> Self {
		match auth {
			OrangeVssAuth::LNURLAuthServer(url) => VssAuth::LNURLAuthServer(url),
			OrangeVssAuth::FixedHeaders(headers) => VssAuth::FixedHeaders(headers),
		}
	}
}

/// Configuration for a Versioned Storage Service (VSS).
#[derive(Debug, Clone, uniffi::Object)]
pub struct VssConfig {
	/// The URL of the VSS.
	vss_url: String,
	/// The store ID for the VSS.
	store_id: String,
	/// Authentication method for the VSS.
	headers: VssAuth,
}

#[uniffi::export]
impl VssConfig {
	#[uniffi::constructor]
	pub fn new(vss_url: String, store_id: String, headers: VssAuth) -> Self {
		VssConfig { vss_url, store_id, headers }
	}
}

impl From<VssConfig> for OrangeVssConfig {
	fn from(config: VssConfig) -> Self {
		OrangeVssConfig {
			vss_url: config.vss_url,
			store_id: config.store_id,
			headers: config.headers.into(),
		}
	}
}

impl From<OrangeVssConfig> for VssConfig {
	fn from(config: OrangeVssConfig) -> Self {
		VssConfig {
			vss_url: config.vss_url,
			store_id: config.store_id,
			headers: config.headers.into(),
		}
	}
}

/// Configuration for wallet storage, either local SQLite or VSS.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum StorageConfig {
	/// Local SQLite database configuration.
	LocalSQLite(String),
	// todo VSS(VssConfig),
}

impl From<StorageConfig> for OrangeStorageConfig {
	fn from(config: StorageConfig) -> Self {
		match config {
			StorageConfig::LocalSQLite(path) => OrangeStorageConfig::LocalSQLite(path),
			// todo VSS(vss_config) => OrangeStorageConfig::VSS(vss_config.into()),
		}
	}
}

/// Configuration for the blockchain data source.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ChainSource {
	/// Electrum server configuration.
	Electrum(String),
	/// Esplora server configuration.
	Esplora {
		/// Esplora url
		url: String,
		/// Optional for Basic authentication for the Esplora server.
		username: Option<String>,
		/// Optional for Basic authentication for the Esplora server.
		password: Option<String>,
	},
	/// Bitcoind RPC configuration.
	BitcoindRPC {
		/// The host of the Bitcoind rpc server (e.g. 127.0.0.1).
		host: String,
		/// The port of the Bitcoind rpc server (e.g. 8332).
		port: u16,
		/// The username for the Bitcoind rpc server.
		user: String,
		/// The password for the Bitcoind rpc server.
		password: String,
	},
}

impl From<ChainSource> for OrangeChainSource {
	fn from(source: ChainSource) -> Self {
		match source {
			ChainSource::Electrum(url) => OrangeChainSource::Electrum(url),
			ChainSource::Esplora { url, username, password } => {
				OrangeChainSource::Esplora { url, username, password }
			},
			ChainSource::BitcoindRPC { host, port, user, password } => {
				OrangeChainSource::BitcoindRPC { host, port, user, password }
			},
		}
	}
}

/// Represents the balances of the wallet, including available and pending balances.
#[derive(Debug, Clone, uniffi::Object)]
pub struct Tunables(pub OrangeTunables);

#[uniffi::export]
impl Tunables {
	#[uniffi::constructor]
	pub fn new(
		trusted_balance_limit: Arc<Amount>, rebalance_min: Arc<Amount>,
		onchain_receive_threshold: Arc<Amount>, enable_amountless_receive_on_chain: bool,
	) -> Self {
		Tunables(OrangeTunables {
			trusted_balance_limit: trusted_balance_limit.0,
			rebalance_min: rebalance_min.0,
			onchain_receive_threshold: onchain_receive_threshold.0,
			enable_amountless_receive_on_chain,
		})
	}

	#[uniffi::constructor]
	pub fn default() -> Self {
		Tunables(OrangeTunables::default())
	}
}

impl_from_core_type!(OrangeTunables, Tunables);
impl_into_core_type!(Tunables, OrangeTunables);

#[derive(Clone, uniffi::Enum)]
/// Extra configuration needed for different types of wallets.
pub enum ExtraConfig {
	/// Configuration for Spark wallet.
	Spark(Arc<crate::ffi::spark::SparkWalletConfig>),
	/// Configuration for Cashu wallet.
	Cashu(Arc<crate::ffi::cashu::CashuConfig>),
}

impl From<ExtraConfig> for OrangeExtraConfig {
	fn from(config: ExtraConfig) -> Self {
		match config {
			ExtraConfig::Spark(config) => OrangeExtraConfig::Spark(config.deref().clone().into()),
			ExtraConfig::Cashu(config) => OrangeExtraConfig::Cashu(config.deref().clone().into()),
		}
	}
}

/// Configuration for initializing the wallet.
#[derive(Clone, uniffi::Record)]
pub struct WalletConfig {
	/// Configuration for wallet storage.
	pub storage_config: StorageConfig,
	/// Location of the wallet's log file.
	pub log_file: String,
	/// Configuration for the blockchain data source.
	pub chain_source: ChainSource,
	/// Lightning Service Provider (LSP) configuration.
	/// The address to connect to the LSP, the LSP node id, and an optional auth token.
	pub lsp_address: String,
	pub lsp_node_id: String,
	pub lsp_token: Option<String>,
	/// URL to download a scorer from. This is for the lightning node to get its route
	/// scorer from a remote server instead of having to probe and find optimal routes
	/// locally.
	pub scorer_url: Option<String>,
	/// URL to Rapid Gossip Sync server to get gossip data from.
	pub rgs_url: Option<String>,
	/// The Bitcoin network the wallet operates on.
	pub network: Network,
	/// The seed used for wallet generation.
	pub seed: Seed,
	/// Configuration parameters for when the wallet decides to use the lightning or trusted wallet.
	pub tunables: Arc<Tunables>,
	/// Extra configuration specific to the trusted wallet implementation.
	pub extra_config: ExtraConfig,
}

impl TryFrom<WalletConfig> for OrangeWalletConfig {
	type Error = ConfigError;
	fn try_from(config: WalletConfig) -> Result<Self, Self::Error> {
		let lsp_node_id = PublicKey::from_str(&config.lsp_node_id)
			.map_err(|e| ConfigError::InvalidLspNodeId(e.to_string()))?;
		let lsp_address = SocketAddress::from_str(&config.lsp_address)
			.map_err(|e| ConfigError::InvalidLspAddress(e.to_string()))?;
		Ok(OrangeWalletConfig {
			storage_config: config.storage_config.into(),
			log_file: PathBuf::from(config.log_file),
			chain_source: config.chain_source.into(),
			lsp: (lsp_address, lsp_node_id, config.lsp_token.clone()),
			scorer_url: config.scorer_url.clone(),
			rgs_url: config.rgs_url.clone(),
			network: config.network.into(),
			seed: config.seed.try_into()?,
			tunables: config.tunables.deref().clone().into(),
			extra_config: config.extra_config.into(),
		})
	}
}
