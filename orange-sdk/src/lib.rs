#![deny(missing_docs)]
#![doc = include_str!("../../README.md")]

use bitcoin_payment_instructions as instructions;
use bitcoin_payment_instructions::{PaymentInstructions, http_resolver::HTTPHrnResolver};

pub use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use crate::rebalancer::{OrangeRebalanceEventHandler, OrangeTrigger};
use crate::store::{TxMetadata, TxMetadataStore, TxType};
#[cfg(feature = "cashu")]
use crate::trusted_wallet::cashu::Cashu;
#[cfg(feature = "_test-utils")]
use crate::trusted_wallet::dummy::DummyTrustedWallet;
#[cfg(feature = "spark")]
use crate::trusted_wallet::spark::Spark;
use crate::trusted_wallet::{DynTrustedWalletInterface, WalletTrusted};
use graduated_rebalancer::GraduatedRebalancer;
use ldk_node::bitcoin::Network;
use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::io;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::io::sqlite_store::SqliteStore;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::{log_debug, log_error, log_info, log_trace, log_warn};
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::payment::{PaymentDirection, PaymentKind};
use ldk_node::{BuildError, ChannelDetails, DynStore, NodeError};

use std::collections::HashMap;
use std::fmt::{self, Debug, Write};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

mod event;
#[cfg(feature = "uniffi")]
mod ffi;
mod lightning_wallet;
pub(crate) mod logging;
mod rebalancer;
mod runtime;
mod store;
pub mod trusted_wallet;

use lightning_wallet::LightningWallet;
use logging::Logger;
use trusted_wallet::TrustedError;

pub use crate::logging::LoggerType;
use crate::runtime::Runtime;
#[cfg(feature = "cashu")]
pub use crate::trusted_wallet::cashu::CashuConfig;
#[cfg(feature = "spark")]
pub use crate::trusted_wallet::spark::SparkWalletConfig;
pub use bitcoin_payment_instructions;
#[cfg(feature = "cashu")]
pub use cdk::nuts::nut00::CurrencyUnit;
pub use event::{Event, EventQueue};
pub use ldk_node::bip39::Mnemonic;
pub use ldk_node::bitcoin;
pub use ldk_node::payment::ConfirmationStatus;
pub use store::{PaymentId, PaymentType, Transaction, TxStatus};
pub use trusted_wallet::ExtraConfig;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

type Rebalancer = GraduatedRebalancer<
	WalletTrusted<DynTrustedWalletInterface>,
	LightningWallet,
	OrangeTrigger,
	OrangeRebalanceEventHandler,
	Logger,
>;

/// The wallet's balance breakdown across its different storage layers.
///
/// Funds in an orange-sdk [`Wallet`] live in up to three places:
///
/// | Layer | Field | Spendable? |
/// |-------|-------|------------|
/// | Trusted backend (Spark / Cashu) | [`trusted`](Self::trusted) | Yes |
/// | Lightning channel | [`lightning`](Self::lightning) | Yes |
/// | On-chain (pending splice / channel open) | [`pending_balance`](Self::pending_balance) | No |
///
/// Use [`available_balance`](Self::available_balance) to get the total amount the wallet can
/// spend right now (trusted + lightning).
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Balances {
	/// Balance held in the trusted wallet backend (e.g. Spark or Cashu).
	///
	/// These funds are available for instant, low-fee payments but are custodied
	/// by a third party.
	pub trusted: Amount,
	/// Balance available in self-custodial Lightning channels.
	///
	/// These funds are fully self-custodial and can be spent over Lightning
	/// without any third-party trust.
	pub lightning: Amount,
	/// Balance that is not yet spendable.
	///
	/// This includes all on-chain balances (e.g. funds waiting for a channel open
	/// or splice-in to confirm). Once the on-chain transaction confirms and the
	/// channel is ready, these funds move into [`lightning`](Self::lightning).
	pub pending_balance: Amount,
}

impl Balances {
	/// Returns the total spendable balance (trusted + lightning).
	///
	/// This excludes [`pending_balance`](Self::pending_balance) since those funds are not
	/// yet available for spending.
	pub fn available_balance(&self) -> Amount {
		self.lightning.saturating_add(self.trusted)
	}
}

/// The main implementation of the wallet, containing both trusted and lightning wallet components.
struct WalletImpl {
	/// The main implementation of the wallet, containing both trusted and lightning wallet components.
	ln_wallet: Arc<LightningWallet>,
	/// The trusted wallet interface for managing small balances.
	trusted: Arc<Box<DynTrustedWalletInterface>>,
	/// The event handler for processing wallet events.
	event_queue: Arc<EventQueue>,
	/// Configuration parameters for when the wallet decides to use the lightning or trusted wallet.
	tunables: Tunables,
	/// The rebalancer for managing the transfer of funds between the trusted and lightning wallets.
	rebalancer: Arc<Rebalancer>,
	/// The Bitcoin network the wallet operates on (e.g., Mainnet, Testnet).
	network: Network,
	/// Metadata store for tracking transactions.
	tx_metadata: TxMetadataStore,
	/// Key-value store for persistent storage.
	store: Arc<DynStore>,
	/// Logger for logging wallet operations.
	logger: Arc<Logger>,
	/// The Tokio runtime for asynchronous operations.
	runtime: Arc<Runtime>,
}

/// The primary entry point for orange-sdk providing a graduated custody Bitcoin wallet.
///
/// This struct combines a trusted wallet backend (e.g., Spark, Cashu) with self-custodial
/// Lightning channels, automatically managing fund distribution based on configurable thresholds.
///
/// See the [crate-level documentation](crate) for a comprehensive overview, payment flow diagram,
/// and usage examples.
///
/// # Thread Safety
///
/// `Wallet` is `Clone` and all operations are thread-safe. Cloning is cheap as it only
/// increments reference counts.
#[derive(Clone)]
pub struct Wallet {
	/// The internal wallet implementation.
	inner: Arc<WalletImpl>,
}

/// The secret key material used to derive all wallet keys.
///
/// Two representations are supported:
///
/// - [`Mnemonic`](Self::Mnemonic) – a standard BIP 39 mnemonic phrase (12 or 24 words).
///   This is the recommended form for end-user wallets because it can be backed up on paper.
/// - [`Seed64`](Self::Seed64) – a raw 64-byte seed, useful for programmatic key derivation
///   or when the seed is already available from another source.
///
/// The same seed will always produce the same wallet addresses and keys, which is what
/// enables recovery (see [`Wallet::new`] for recovery details).
#[derive(Debug, Clone)]
pub enum Seed {
	/// A BIP 39 mnemonic seed phrase.
	Mnemonic {
		/// The mnemonic phrase (typically 12 or 24 words).
		mnemonic: Mnemonic,
		/// Optional BIP 39 passphrase (sometimes called the "25th word").
		passphrase: Option<String>,
	},
	/// A raw 64-byte seed for the wallet.
	Seed64([u8; 64]),
}

/// Represents the authentication method for a Versioned Storage Service (VSS).
#[derive(Debug, Clone)]
pub enum VssAuth {
	/// Authentication using an LNURL-auth server.
	LNURLAuthServer(String),
	/// Authentication using a fixed set of HTTP headers.
	FixedHeaders(HashMap<String, String>),
}

/// Configuration for a Versioned Storage Service (VSS).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct VssConfig {
	/// The URL of the VSS.
	pub vss_url: String,
	/// The store ID for the VSS.
	pub store_id: String,
	/// Authentication method for the VSS.
	pub headers: VssAuth,
}

/// Configuration for wallet persistence.
///
/// Controls where the wallet stores channel state, transaction metadata, and event history.
///
/// Currently only local SQLite is supported. A VSS (Versioned Storage Service) backend is
/// planned, which will enable cross-device state synchronization and Lightning channel recovery
/// from seed.
#[derive(Debug, Clone)]
pub enum StorageConfig {
	/// Store all data in a local SQLite database at the given directory path.
	///
	/// The directory will be created if it does not exist.
	LocalSQLite(String),
	// todo VSS(VssConfig),
}

/// The blockchain data source used to monitor on-chain transactions and fee rates.
///
/// The wallet needs a connection to the Bitcoin network to track on-chain balances,
/// confirm channel opens/closes, and estimate fees. Three backend types are supported:
///
/// - [`Electrum`](Self::Electrum) – connects to an Electrum server (supports SSL via `ssl://` prefix).
/// - [`Esplora`](Self::Esplora) – connects to an Esplora HTTP API (e.g. `https://blockstream.info/api`).
/// - [`BitcoindRPC`](Self::BitcoindRPC) – connects directly to a Bitcoin Core node via JSON-RPC.
#[derive(Debug, Clone)]
pub enum ChainSource {
	/// Connect to an Electrum server.
	///
	/// Use the `ssl://` prefix for TLS connections (e.g. `ssl://electrum.blockstream.info:60002`).
	Electrum(String),
	/// Connect to an Esplora HTTP API, with optional Basic authentication.
	Esplora {
		/// The base URL of the Esplora server (e.g. `https://blockstream.info/api`).
		url: String,
		/// Optional username for HTTP Basic authentication.
		username: Option<String>,
		/// Optional password for HTTP Basic authentication.
		password: Option<String>,
	},
	/// Connect directly to a Bitcoin Core node via JSON-RPC.
	BitcoindRPC {
		/// The host of the Bitcoin Core RPC server (e.g. `127.0.0.1`).
		host: String,
		/// The port of the Bitcoin Core RPC server (e.g. `8332` for mainnet).
		port: u16,
		/// The RPC username (configured in `bitcoin.conf`).
		user: String,
		/// The RPC password (configured in `bitcoin.conf`).
		password: String,
	},
}

/// Everything needed to initialize a [`Wallet`].
///
/// This struct bundles together all the configuration required to create a wallet instance:
/// storage, networking, keys, thresholds, and the trusted wallet backend.
///
/// See the [crate-level documentation](crate) for a full configuration example.
#[derive(Clone)]
pub struct WalletConfig {
	/// Where the wallet persists its state (channel data, transaction metadata, events).
	pub storage_config: StorageConfig,
	/// How the wallet emits log output.
	pub logger_type: LoggerType,
	/// The blockchain data source for on-chain monitoring and fee estimation.
	pub chain_source: ChainSource,
	/// Lightning Service Provider (LSP) connection details.
	///
	/// The tuple contains:
	/// 1. The LSP's network address (e.g. `127.0.0.1:9735`)
	/// 2. The LSP's node public key
	/// 3. An optional authentication token
	///
	/// The LSP is used to open JIT (Just-In-Time) channels for receiving Lightning payments.
	pub lsp: (SocketAddress, PublicKey, Option<String>),
	/// Optional URL to download a pre-built route scorer.
	///
	/// When set, the Lightning node fetches its route scorer from this remote server instead of
	/// probing and discovering optimal routes locally. This speeds up initial routing decisions.
	pub scorer_url: Option<String>,
	/// Optional URL to a [Rapid Gossip Sync](https://docs.rs/lightning-rapid-gossip-sync) server.
	///
	/// When set, the Lightning node downloads compressed gossip data from this server instead
	/// of learning the network graph through peer gossip, significantly reducing sync time.
	pub rgs_url: Option<String>,
	/// The Bitcoin network to operate on (e.g. `Network::Bitcoin`, `Network::Testnet`).
	pub network: Network,
	/// The secret key material for this wallet. See [`Seed`] for options.
	pub seed: Seed,
	/// Thresholds that control when funds move between the trusted and Lightning wallets.
	pub tunables: Tunables,
	/// Backend-specific configuration for the trusted wallet (Spark, Cashu, etc.).
	pub extra_config: ExtraConfig,
}

/// Thresholds that control the wallet's graduated custody behavior.
///
/// These parameters govern how the wallet distributes funds between the trusted backend
/// and self-custodial Lightning channels, and how it generates payment URIs.
///
/// The default values are a reasonable starting point for most wallets:
///
/// | Parameter | Default | Purpose |
/// |-----------|---------|---------|
/// | `trusted_balance_limit` | 100,000 sats | Trigger rebalance to Lightning above this |
/// | `rebalance_min` | 5,000 sats | Don't bother rebalancing amounts smaller than this |
/// | `onchain_receive_threshold` | 10,000 sats | Include on-chain address in receive URIs above this |
/// | `enable_amountless_receive_on_chain` | `true` | Include on-chain address for open-amount receives |
#[derive(Debug, Clone, Copy)]
pub struct Tunables {
	/// The maximum balance allowed in the trusted wallet before triggering automatic rebalancing.
	///
	/// When the trusted balance exceeds this limit, excess funds are automatically moved into
	/// a self-custodial Lightning channel. Set this based on how much you're comfortable
	/// holding in the trusted backend.
	pub trusted_balance_limit: Amount,
	/// The minimum amount worth rebalancing from trusted to Lightning.
	///
	/// Amounts below this threshold won't be transferred even if there's available Lightning
	/// capacity, avoiding unnecessary small transfers and their associated fees.
	pub rebalance_min: Amount,
	/// The minimum receive amount for which an on-chain address is included in payment URIs.
	///
	/// When generating a receive URI via [`Wallet::get_single_use_receive_uri`], amounts
	/// below this threshold will only include a Lightning invoice (no on-chain fallback).
	/// This avoids on-chain dust for small payments.
	pub onchain_receive_threshold: Amount,
	/// Whether to include an on-chain address in open-amount (no specific amount) receive URIs.
	///
	/// When `true`, [`Wallet::get_single_use_receive_uri`] called with `amount: None` will
	/// include an on-chain address alongside the Lightning invoice.
	pub enable_amountless_receive_on_chain: bool,
}

impl Default for Tunables {
	fn default() -> Self {
		Tunables {
			trusted_balance_limit: Amount::from_sats(100_000).expect("valid amount"),
			rebalance_min: Amount::from_sats(5_000).expect("valid amount"),
			onchain_receive_threshold: Amount::from_sats(10_000).expect("valid amount"),
			enable_amountless_receive_on_chain: true,
		}
	}
}

/// Errors returned by [`PaymentInfo::build`] when the provided amount is incompatible
/// with the [`PaymentInstructions`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PaymentInfoBuildError {
	/// The amount given does not match either of the fixed amounts specified in the
	/// [`PaymentInstructions`].
	AmountMismatch {
		/// The amount given by the user.
		given: Amount,
		/// The fixed lightning amount specified in the [`PaymentInstructions`].
		ln_amount: Option<Amount>,
		/// The fixed on-chain amount specified in the [`PaymentInstructions`].
		onchain_amount: Option<Amount>,
	},
	/// No amount was given and the [`PaymentInstructions`] has no fixed amount.
	MissingAmount,
	/// The amount given is outside the range specified in the [`PaymentInstructions`].
	AmountOutOfRange {
		/// The amount given by the user.
		given: Amount,
		/// The minimum amount specified in the [`PaymentInstructions`].
		min: Option<Amount>,
		/// The maximum amount specified in the [`PaymentInstructions`].
		max: Option<Amount>,
	},
}

/// A validated, ready-to-pay combination of [`PaymentInstructions`] and an [`Amount`].
///
/// Created via [`PaymentInfo::build`], which validates that the amount is compatible
/// with the payment instructions. Pass this to [`Wallet::pay`] to initiate the payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentInfo {
	/// The payment instructions (e.g., BOLT 11 invoice, on-chain address).
	instructions: PaymentInstructions,
	/// The amount to be paid.
	amount: Amount,
}

impl PaymentInfo {
	/// Creates a [`PaymentInfo`] by pairing [`PaymentInstructions`] with an amount.
	///
	/// The amount is validated against the payment instructions:
	///
	/// - **[`ConfigurableAmount`](PaymentInstructions::ConfigurableAmount):** the amount is
	///   required and must fall within the optional min/max range.
	/// - **[`FixedAmount`](PaymentInstructions::FixedAmount):** the amount is optional (it
	///   defaults to the fixed amount) but if provided must match.
	///
	/// Returns [`PaymentInfoBuildError`] if the amount is missing, out of range, or mismatched.
	pub fn build(
		instructions: PaymentInstructions, amount: Option<Amount>,
	) -> Result<PaymentInfo, PaymentInfoBuildError> {
		match &instructions {
			PaymentInstructions::ConfigurableAmount(conf) => match amount {
				None => Err(PaymentInfoBuildError::MissingAmount),
				Some(amount) => {
					if conf.min_amt().unwrap_or(Amount::ZERO) > amount
						|| conf.max_amt().unwrap_or(Amount::MAX) < amount
					{
						Err(PaymentInfoBuildError::AmountOutOfRange {
							given: amount,
							min: conf.min_amt(),
							max: conf.max_amt(),
						})
					} else {
						Ok(PaymentInfo { instructions, amount })
					}
				},
			},
			PaymentInstructions::FixedAmount(fixed) => {
				match (fixed.ln_payment_amount(), fixed.onchain_payment_amount()) {
					(Some(ln), None) => match amount {
						None => Ok(PaymentInfo { instructions, amount: ln }),
						Some(amount) => {
							if amount != ln {
								Err(PaymentInfoBuildError::AmountMismatch {
									given: amount,
									ln_amount: Some(ln),
									onchain_amount: None,
								})
							} else {
								Ok(PaymentInfo { instructions, amount })
							}
						},
					},
					(None, Some(chain)) => match amount {
						None => Ok(PaymentInfo { instructions, amount: chain }),
						Some(amount) => {
							if amount != chain {
								Err(PaymentInfoBuildError::AmountMismatch {
									given: amount,
									ln_amount: None,
									onchain_amount: Some(chain),
								})
							} else {
								Ok(PaymentInfo { instructions, amount })
							}
						},
					},
					(Some(ln), Some(chain)) => match amount {
						None => {
							if ln == chain {
								Ok(PaymentInfo { instructions, amount: ln })
							} else {
								Err(PaymentInfoBuildError::MissingAmount)
							}
						},
						Some(amount) => {
							if ln != amount && chain != amount {
								Err(PaymentInfoBuildError::AmountMismatch {
									given: amount,
									ln_amount: Some(ln),
									onchain_amount: Some(chain),
								})
							} else {
								Ok(PaymentInfo { instructions, amount })
							}
						},
					},
					(None, None) => match amount {
						None => Err(PaymentInfoBuildError::MissingAmount),
						Some(amount) => Ok(PaymentInfo { instructions, amount }),
					},
				}
			},
		}
	}

	/// Get the payment instructions.
	pub fn instructions(&self) -> PaymentInstructions {
		self.instructions.clone()
	}

	/// Get the amount to be paid.
	pub fn amount(&self) -> Amount {
		self.amount
	}
}

/// Errors that can occur during [`Wallet::new`].
///
/// Initialization can fail due to I/O issues (e.g. storage), Lightning node setup errors,
/// or problems connecting to the trusted wallet backend.
#[derive(Debug)]
pub enum InitFailure {
	/// An I/O error occurred (e.g. failed to create storage directory).
	IoError(io::Error),
	/// Failed to build the underlying LDK node (invalid configuration).
	LdkNodeBuildFailure(BuildError),
	/// Failed to start the underlying LDK node (e.g. port already in use).
	LdkNodeStartFailure(NodeError),
	/// The trusted wallet backend failed to initialize.
	TrustedFailure(TrustedError),
}

impl From<io::Error> for InitFailure {
	fn from(e: io::Error) -> InitFailure {
		InitFailure::IoError(e)
	}
}

impl From<BuildError> for InitFailure {
	fn from(e: BuildError) -> InitFailure {
		InitFailure::LdkNodeBuildFailure(e)
	}
}

impl From<NodeError> for InitFailure {
	fn from(e: NodeError) -> InitFailure {
		InitFailure::LdkNodeStartFailure(e)
	}
}

impl From<TrustedError> for InitFailure {
	fn from(e: TrustedError) -> InitFailure {
		InitFailure::TrustedFailure(e)
	}
}

/// Errors that can occur during wallet operations (payments, balance queries, etc.).
#[derive(Debug)]
pub enum WalletError {
	/// The self-custodial Lightning node encountered an error.
	LdkNodeFailure(NodeError),
	/// The trusted wallet backend encountered an error.
	TrustedFailure(TrustedError),
}

impl From<TrustedError> for WalletError {
	fn from(e: TrustedError) -> WalletError {
		WalletError::TrustedFailure(e)
	}
}

impl From<NodeError> for WalletError {
	fn from(e: NodeError) -> WalletError {
		WalletError::LdkNodeFailure(e)
	}
}

/// A single-use [BIP 21](https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki) Bitcoin
/// URI for receiving a payment.
///
/// Generated by [`Wallet::get_single_use_receive_uri`]. The URI may contain both an on-chain
/// address and a BOLT 11 Lightning invoice (a "unified" URI), allowing the payer to choose the
/// best payment method. Whether an on-chain address is included depends on the wallet's
/// [`Tunables`] and the requested amount.
///
/// The [`Display`](std::fmt::Display) implementation formats this as a BIP 21 URI string
/// suitable for encoding in a QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleUseReceiveUri {
	/// The on-chain Bitcoin address, if included.
	///
	/// Present when the requested amount exceeds [`Tunables::onchain_receive_threshold`],
	/// or when [`Tunables::enable_amountless_receive_on_chain`] is `true` and no amount
	/// was specified.
	pub address: Option<bitcoin::Address>,
	/// The BOLT 11 Lightning invoice for this payment.
	pub invoice: Bolt11Invoice,
	/// The requested amount, if one was specified.
	pub amount: Option<Amount>,
	/// Whether the invoice was generated by the trusted wallet backend or the
	/// self-custodial LDK node.
	///
	/// When `amount` is `Some`, this reflects the routing decision: small amounts
	/// use the trusted backend (`true`), larger amounts use the LDK node (`false`).
	///
	/// When `amount` is `None` (amountless receive), this is always `false` even
	/// though the invoice is generated by the trusted backend, since the wallet
	/// cannot predict which layer will ultimately receive the payment.
	pub from_trusted: bool,
}

impl fmt::Display for SingleUseReceiveUri {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match &self.address {
			Some(address) => {
				let mut uri = format!("BITCOIN:{address}");
				if let Some(amt) = self.amount {
					write!(&mut uri, "?AMOUNT={}&", amt.btc_decimal_rounding_up_to_sats())?;
				} else {
					write!(&mut uri, "?")?;
				}
				write!(&mut uri, "LIGHTNING={}", self.invoice)?;

				let res = uri.to_ascii_uppercase();
				write!(f, "{}", res)
			},
			None => write!(f, "LIGHTNING:{}", self.invoice.to_string().to_ascii_uppercase()),
		}
	}
}

impl Wallet {
	/// Constructs a new Wallet
	///
	/// ## Recovery
	///
	/// The wallet automatically performs recovery operations when initialized for the first time:
	///
	/// - **Trusted wallet backends** (Spark/Cashu): Automatically attempt to recover any existing
	///   funds associated with the wallet's seed on first initialization. Recovery is performed
	///   once per wallet instance and runs asynchronously without blocking initialization.
	///
	/// - **Lightning wallet**: Recovery will **NOT** work unless using VSS (Versioned Storage Service)
	///   for storage. With local SQLite storage, Lightning channel state and funds cannot be recovered
	///   from seed alone and will be lost if the local storage is deleted.
	///
	/// Recovery ensures trusted wallet funds can be restored when reconstructed from the same seed
	/// across different devices or installations.
	pub async fn new(config: WalletConfig) -> Result<Wallet, InitFailure> {
		let tunables = config.tunables;
		let network = config.network;
		let logger = Arc::new(Logger::new(&config.logger_type).expect("Failed to open log file"));

		log_info!(logger, "Initializing orange on network: {network}");

		let runtime = Arc::new(Runtime::new(Arc::clone(&logger)).map_err(|e| {
			log_error!(logger, "Failed to set up tokio runtime: {e}");
			BuildError::RuntimeSetupFailed
		})?);

		let store: Arc<DynStore> = match &config.storage_config {
			StorageConfig::LocalSQLite(path) => {
				Arc::new(SqliteStore::new(path.into(), Some("orange.sqlite".to_owned()), None)?)
			},
		};

		let event_queue = Arc::new(EventQueue::new(Arc::clone(&store), Arc::clone(&logger)));

		let tx_metadata = TxMetadataStore::new(Arc::clone(&store)).await;

		// Cashu must init before LDK Node because CashuKvDatabase does
		// synchronous SQLite reads that deadlock with LDK Node's background
		// store writes. Other backends can init concurrently.
		#[cfg(feature = "cashu")]
		let cashu_wallet = if let ExtraConfig::Cashu(cashu) = &config.extra_config {
			Some(
				Cashu::init(
					&config,
					cashu.clone(),
					Arc::clone(&store),
					Arc::clone(&event_queue),
					tx_metadata.clone(),
					Arc::clone(&logger),
					Arc::clone(&runtime),
				)
				.await?,
			)
		} else {
			None
		};

		let (trusted, ln_wallet) = tokio::join!(
			async {
				let trusted: Arc<Box<DynTrustedWalletInterface>> = match &config.extra_config {
					#[cfg(feature = "spark")]
					ExtraConfig::Spark(sp) => Arc::new(Box::new(
						Spark::init(
							&config,
							sp.clone(),
							Arc::clone(&store),
							Arc::clone(&event_queue),
							tx_metadata.clone(),
							Arc::clone(&logger),
							Arc::clone(&runtime),
						)
						.await?,
					)),
					#[cfg(feature = "cashu")]
					ExtraConfig::Cashu(_) => Arc::new(Box::new(cashu_wallet.expect("initialized above"))),
					#[cfg(feature = "_test-utils")]
					ExtraConfig::Dummy(cfg) => Arc::new(Box::new(
						DummyTrustedWallet::new(
							cfg.uuid,
							&cfg.lsp,
							&cfg.bitcoind,
							tx_metadata.clone(),
							Arc::clone(&event_queue),
							Arc::clone(&runtime),
						)
						.await,
					)),
				};
				Ok::<_, InitFailure>(trusted)
			},
			async {
				Ok::<_, InitFailure>(Arc::new(
					LightningWallet::init(
						Arc::clone(&runtime),
						config.clone(),
						Arc::clone(&store),
						Arc::clone(&event_queue),
						tx_metadata.clone(),
						Arc::clone(&logger),
					)
					.await?,
				))
			},
		);
		let trusted = trusted?;
		let ln_wallet = ln_wallet?;

		let wt = Arc::new(WalletTrusted(Arc::clone(&trusted)));

		let trigger = Arc::new(OrangeTrigger::new(
			Arc::clone(&ln_wallet),
			Arc::clone(&trusted),
			tunables,
			tx_metadata.clone(),
			Arc::clone(&event_queue),
			Arc::clone(&store),
			Arc::clone(&logger),
		));

		let rebalance_events = Arc::new(OrangeRebalanceEventHandler::new(
			tx_metadata.clone(),
			Arc::clone(&event_queue),
			Arc::clone(&logger),
		));

		let rebalancer = Arc::new(GraduatedRebalancer::new(
			wt,
			Arc::clone(&ln_wallet),
			trigger,
			rebalance_events,
			Arc::clone(&logger),
		));

		// Spawn a background thread to initiate a rebalance if needed.
		// We only do this once as we generally rebalance in response to
		// `Event`s which indicated our balance has changed.
		let rb = Arc::clone(&rebalancer);
		runtime.spawn_cancellable_background_task(async move {
			// Wait a second to get caught up, then try to rebalance.
			tokio::time::sleep(Duration::from_secs(1)).await;
			rb.do_rebalance_if_needed().await;

			// create loop for onchain rebalancing.
			// we only do onchain rebalancing here as trusted rebalancing is
			// handled by events but we cannot do that for onchain rebalancing.
			// So we need to detect onchain balance changes periodically.
			loop {
				tokio::time::sleep(Duration::from_secs(1)).await;
				rb.do_onchain_rebalance_if_needed().await;
			}
		});

		let inner = Arc::new(WalletImpl {
			trusted,
			ln_wallet,
			event_queue,
			network,
			tunables,
			rebalancer,
			tx_metadata,
			store,
			logger,
			runtime,
		});

		Ok(Wallet { inner })
	}

	/// Enables or disables automatic rebalancing from trusted/on-chain to Lightning.
	///
	/// Rebalancing is enabled by default. It is automatically disabled when a channel closes
	/// (to avoid an open-close loop). Call this with `true` to re-enable it after handling
	/// a [`Event::ChannelClosed`] event.
	pub async fn set_rebalance_enabled(&self, value: bool) {
		store::set_rebalance_enabled(self.inner.store.as_ref(), value).await
	}

	/// Whether the wallet should automatically rebalance from trusted/onchain to lightning.
	pub async fn get_rebalance_enabled(&self) -> bool {
		store::get_rebalance_enabled(self.inner.store.as_ref()).await
	}

	/// Returns the public key of the underlying Lightning node.
	///
	/// This is the node's identity on the Lightning Network and can be shared with peers.
	pub fn node_id(&self) -> PublicKey {
		self.inner.ln_wallet.inner.ldk_node.node_id()
	}

	/// Check if the lightning wallet is currently connected to the LSP.
	pub fn is_connected_to_lsp(&self) -> bool {
		self.inner.ln_wallet.is_connected_to_lsp()
	}

	/// Lists all open Lightning channels and their details (capacity, balance, state, etc.).
	pub fn channels(&self) -> Vec<ChannelDetails> {
		self.inner.ln_wallet.inner.ldk_node.list_channels()
	}

	/// Lists completed transactions and in-flight outbound payments, sorted by time.
	///
	/// Returns a unified list covering both trusted and self-custodial payments.
	/// Internal rebalance transfers are merged into single logical transactions
	/// with combined fees.
	///
	/// **Note:** Pending inbound invoices (issued but unpaid) and pending rebalances
	/// are excluded from the results.
	pub async fn list_transactions(&self) -> Result<Vec<Transaction>, WalletError> {
		let (trusted_payments, splice_outs) = tokio::join!(
			self.inner.trusted.list_payments(),
			store::read_splice_outs(self.inner.store.as_ref())
		);
		let trusted_payments = trusted_payments?;
		let mut lightning_payments = self.inner.ln_wallet.list_payments();
		lightning_payments.sort_by_key(|l| l.latest_update_timestamp);

		let mut res = Vec::with_capacity(
			trusted_payments.len() + lightning_payments.len() + splice_outs.len(),
		);
		let tx_metadata = self.inner.tx_metadata.read();

		let mut internal_transfers = HashMap::new();
		#[derive(Debug, Default)]
		struct InternalTransfer {
			receive_fee: Option<Amount>,
			send_fee: Option<Amount>,
			transaction: Option<Transaction>,
		}

		for payment in trusted_payments {
			if let Some(tx_metadata) = tx_metadata.get(&PaymentId::Trusted(payment.id)) {
				match &tx_metadata.ty {
					TxType::TrustedToLightning {
						trusted_payment,
						lightning_payment: _,
						payment_triggering_transfer,
					} => {
						let entry = internal_transfers
							.entry(*payment_triggering_transfer)
							.or_insert(InternalTransfer::default());
						if payment.id == *trusted_payment {
							debug_assert!(entry.send_fee.is_none());
							entry.send_fee = Some(payment.fee);
						} else {
							debug_assert!(false);
						}
					},
					TxType::OnchainToLightning { .. } => {
						debug_assert!(
							false,
							"Onchain to lightning transfer should not be in trusted payments list"
						);
					},
					TxType::PaymentTriggeringTransferLightning { ty } => {
						let entry = internal_transfers
							.entry(PaymentId::Trusted(payment.id))
							.or_insert(InternalTransfer::default());
						debug_assert!(entry.transaction.is_none());
						entry.transaction = Some(Transaction {
							id: PaymentId::Trusted(payment.id),
							status: payment.status,
							outbound: payment.outbound,
							amount: Some(payment.amount),
							fee: Some(payment.fee),
							payment_type: *ty,
							time_since_epoch: tx_metadata.time,
						});
					},
					TxType::Payment { ty } => {
						debug_assert!(!matches!(ty, PaymentType::OutgoingOnChain { .. }));
						debug_assert!(!matches!(ty, PaymentType::IncomingOnChain { .. }));
						res.push(Transaction {
							id: PaymentId::Trusted(payment.id),
							status: payment.status,
							outbound: payment.outbound,
							amount: Some(payment.amount),
							fee: Some(payment.fee),
							payment_type: *ty,
							time_since_epoch: tx_metadata.time,
						});
					},
					TxType::PendingRebalance { .. } => {
						// Pending rebalances are not shown in the transaction list.
						continue;
					},
				}
			} else {
				if payment.outbound {
					log_warn!(
						self.inner.logger,
						"Missing outbound trusted payment metadata entry on {:?}",
						payment.id
					);
					#[cfg(feature = "_test-utils")]
					debug_assert!(
						false,
						"Missing outbound trusted payment metadata entry on {:?}",
						payment.id
					);
				}

				if payment.status != TxStatus::Completed {
					// We don't bother to surface pending inbound transactions (i.e. issued but
					// unpaid invoices) in our transaction list.
					continue;
				}

				let payment_type = if payment.outbound {
					PaymentType::OutgoingLightningBolt11 { payment_preimage: None }
				} else {
					PaymentType::IncomingLightning {}
				};

				res.push(Transaction {
					id: PaymentId::Trusted(payment.id),
					status: payment.status,
					outbound: payment.outbound,
					amount: Some(payment.amount),
					fee: Some(payment.fee),
					payment_type,
					time_since_epoch: payment.time_since_epoch,
				});
			}
		}
		for payment in lightning_payments {
			use ldk_node::payment::PaymentDirection;
			let lightning_receive_fee = match payment.kind {
				PaymentKind::Bolt11Jit { counterparty_skimmed_fee_msat, .. } => {
					let msats = counterparty_skimmed_fee_msat.unwrap_or(0);
					debug_assert_eq!(payment.direction, PaymentDirection::Inbound);
					Some(Amount::from_milli_sats(msats).expect("Must be valid"))
				},
				_ => None,
			};
			let fee = if payment.direction == PaymentDirection::Outbound {
				match payment.fee_paid_msat {
					None => Some(lightning_receive_fee.unwrap_or(Amount::ZERO)),
					Some(fee) => Some(
						Amount::from_milli_sats(fee)
							.unwrap()
							.saturating_add(lightning_receive_fee.unwrap_or(Amount::ZERO)),
					),
				}
			} else {
				Some(lightning_receive_fee.unwrap_or(Amount::ZERO))
			};
			if let Some(tx_metadata) = tx_metadata.get(&PaymentId::SelfCustodial(payment.id.0)) {
				match &tx_metadata.ty {
					TxType::TrustedToLightning {
						trusted_payment: _,
						lightning_payment,
						payment_triggering_transfer,
					} => {
						let entry = internal_transfers
							.entry(*payment_triggering_transfer)
							.or_insert(InternalTransfer::default());
						if payment.id.0 == *lightning_payment {
							debug_assert!(entry.receive_fee.is_none());
							entry.receive_fee = lightning_receive_fee.or(Some(Amount::ZERO));
						} else {
							debug_assert!(false);
						}
					},
					TxType::OnchainToLightning { channel_txid, triggering_txid } => {
						let entry = internal_transfers
							.entry(PaymentId::SelfCustodial(triggering_txid.to_byte_array()))
							.or_insert(InternalTransfer::default());
						if &payment.id.0 == channel_txid.as_byte_array() {
							debug_assert!(entry.send_fee.is_none());
							entry.send_fee = payment
								.fee_paid_msat
								.map(|fee| Amount::from_milli_sats(fee).expect("Must be valid"));
						} else {
							debug_assert!(false);
						}
					},
					TxType::PaymentTriggeringTransferLightning { ty: _ } => {
						let entry = internal_transfers
							.entry(PaymentId::SelfCustodial(payment.id.0))
							.or_insert(InternalTransfer {
								receive_fee: lightning_receive_fee,
								send_fee: None,
								transaction: None,
							});
						debug_assert!(entry.transaction.is_none());

						entry.transaction = Some(Transaction {
							id: PaymentId::SelfCustodial(payment.id.0),
							status: payment.status.into(),
							outbound: payment.direction == PaymentDirection::Outbound,
							amount: payment
								.amount_msat
								.map(|a| Amount::from_milli_sats(a).expect("Must be valid")),
							fee,
							payment_type: (&payment).into(),
							time_since_epoch: tx_metadata.time,
						});
					},
					TxType::Payment { ty: _ } => res.push(Transaction {
						id: PaymentId::SelfCustodial(payment.id.0),
						status: payment.status.into(),
						outbound: payment.direction == PaymentDirection::Outbound,
						amount: payment
							.amount_msat
							.map(|a| Amount::from_milli_sats(a).expect("Must be valid")),
						fee,
						payment_type: (&payment).into(),
						time_since_epoch: tx_metadata.time,
					}),
					TxType::PendingRebalance { .. } => {
						// Pending rebalances are not shown in the transaction list.
						continue;
					},
				}
			} else {
				debug_assert_ne!(
					payment.direction,
					PaymentDirection::Outbound,
					"Missing outbound lightning payment metadata entry on {}",
					payment.id
				);

				let status = payment.status.into();
				if status != TxStatus::Completed {
					// We don't bother to surface pending inbound transactions (i.e. issued but
					// unpaid invoices) in our transaction list, in part because these may be
					// failed rebalances.
					continue;
				}
				res.push(Transaction {
					id: PaymentId::SelfCustodial(payment.id.0),
					status,
					outbound: payment.direction == PaymentDirection::Outbound,
					amount: payment.amount_msat.map(|a| Amount::from_milli_sats(a).unwrap()),
					fee,
					payment_type: (&payment).into(),
					time_since_epoch: Duration::from_secs(payment.latest_update_timestamp),
				})
			}
		}

		for details in splice_outs {
			res.push(Transaction {
				id: PaymentId::SelfCustodial(details.id.0),
				status: details.status.into(),
				outbound: details.direction == PaymentDirection::Outbound,
				amount: details.amount_msat.map(|a| Amount::from_milli_sats(a).unwrap()),
				fee: details.fee_paid_msat.map(|fee| Amount::from_milli_sats(fee).unwrap()),
				payment_type: (&details).into(),
				time_since_epoch: Duration::from_secs(details.latest_update_timestamp),
			});
		}

		for (id, tx_info) in internal_transfers {
			debug_assert!(
				tx_info.send_fee.is_some(),
				"Internal transfers must have a send fee, got {id}: {tx_info:?}",
			);
			debug_assert!(tx_info.transaction.is_some());

			if let Some(mut transaction) = tx_info.transaction {
				transaction.fee = Some(
					transaction
						.fee
						.unwrap_or(Amount::ZERO)
						.saturating_add(tx_info.receive_fee.unwrap_or(Amount::ZERO))
						.saturating_add(tx_info.send_fee.unwrap_or(Amount::ZERO)),
				);
				res.push(transaction);
			}
		}

		res.sort_by_key(|e| e.time_since_epoch);
		Ok(res)
	}

	/// Returns the wallet's current balance across all layers (trusted, Lightning, and pending).
	///
	/// See [`Balances`] for details on each field.
	pub async fn get_balance(&self) -> Result<Balances, WalletError> {
		let trusted_balance = self.inner.trusted.get_balance().await?;
		let ln_balance = self.inner.ln_wallet.get_balance();
		log_debug!(
			self.inner.logger,
			"Have trusted balance of {trusted_balance:?}, lightning available balance of {:?}, and on chain balance of {:?}",
			ln_balance.lightning,
			ln_balance.onchain
		);
		Ok(Balances {
			trusted: trusted_balance,
			lightning: ln_balance.lightning,
			pending_balance: ln_balance.onchain,
		})
	}

	/// Fetches a unique reusable BIP 321 bitcoin: URI.
	///
	/// This will generally include an on-chain address as well as a BOLT 12 lightning offer. Note
	/// that BOLT 12 offers are not universally supported across the lightning ecosystem yet, and,
	/// as such, may result in on-chain fallbacks. Use [`Self::get_single_use_receive_uri`] if
	/// possible.
	///
	/// This is suitable for inclusion in a QR code or in a BIP 353 DNS record
	pub async fn get_reusable_receive_uri(&self) -> Result<String, WalletError> {
		Ok(self.inner.trusted.get_reusable_receive_uri().await?)
	}

	/// Fetches a unique single-use BIP 321 bitcoin: URI.
	///
	/// This will generally include an on-chain address as well as a BOLT 11 lightning invoice.
	///
	/// This is suitable for inclusion in a QR code.
	pub async fn get_single_use_receive_uri(
		&self, amount: Option<Amount>,
	) -> Result<SingleUseReceiveUri, WalletError> {
		let (enable_onchain, bolt11, from_trusted) = if let Some(amt) = amount {
			let enable_onchain = amt >= self.inner.tunables.onchain_receive_threshold;
			// We always assume lighting balance is an overestimate by `rebalance_min`.
			let lightning_receivable = self
				.inner
				.ln_wallet
				.estimate_receivable_balance()
				.saturating_sub(self.inner.tunables.rebalance_min);
			let lsp_online = self.inner.ln_wallet.is_connected_to_lsp();

			// use self custodial if the amount is either:
			//   - Greater than our trusted balance limit
			//   - Less than our incoming lightning receivable balance when the LSP is online
			let use_ln = amt > self.inner.tunables.trusted_balance_limit
				|| (amt < lightning_receivable && lsp_online);

			let mut from_trusted = !use_ln;

			let bolt11 = if use_ln {
				let res = self.inner.ln_wallet.get_bolt11_invoice(amount).await;
				match res {
					Ok(inv) => inv,
					Err(NodeError::ConnectionFailed) => {
						log_warn!(
							self.inner.logger,
							"Failed to connect to LSP when getting BOLT 11 invoice, falling back to trusted wallet"
						);
						from_trusted = true;
						self.inner.trusted.get_bolt11_invoice(amount).await?
					},
					Err(e) => {
						log_error!(self.inner.logger, "Failed to get BOLT 11 invoice: {e:?}");
						return Err(WalletError::LdkNodeFailure(e));
					},
				}
			} else {
				self.inner.trusted.get_bolt11_invoice(amount).await?
			};
			(enable_onchain, bolt11, from_trusted)
		} else {
			(
				self.inner.tunables.enable_amountless_receive_on_chain,
				self.inner.trusted.get_bolt11_invoice(amount).await?,
				false,
			)
		};
		if enable_onchain {
			let address = self.inner.ln_wallet.get_on_chain_address()?;
			Ok(SingleUseReceiveUri {
				address: Some(address),
				invoice: bolt11,
				amount,
				from_trusted,
			})
		} else {
			Ok(SingleUseReceiveUri { address: None, invoice: bolt11, amount, from_trusted })
		}
	}

	/// Parses payment instructions from an arbitrary string provided by a user, scanned from a QR
	/// code, or read from a link which the user opened with this application.
	///
	/// See [`PaymentInstructions`] for the list of supported formats.
	// Note that a user might also be pasting or scanning a QR code containing a lightning BOLT 12
	// refund, which allow us to *receive* funds, and must be parsed with
	// [`Self::parse_claim_instructions`].
	pub async fn parse_payment_instructions(
		&self, instructions: &str,
	) -> Result<PaymentInstructions, instructions::ParseError> {
		PaymentInstructions::parse(instructions, self.inner.network, &HTTPHrnResolver::new(), true)
			.await
	}

	// /// Verifies instructions which allow us to claim funds given as:
	//  * A lightning BOLT 12 refund
	//  * An on-chain private key, which we will sweep
	// TODO: consider LNURL claim thing
	// pub fn parse_claim_instructions(&self, instructions: &str) -> Result<..., ...> {
	//
	// }

	/// Estimates the fees required to pay a [`PaymentInstructions`].
	///
	/// **Note:** Fee estimation is not yet implemented and currently always returns zero.
	pub async fn estimate_fee(&self, _payment_info: &PaymentInstructions) -> Amount {
		// todo implement fee estimation
		Amount::ZERO
	}

	/// Sends a payment using the provided [`PaymentInfo`].
	///
	/// The wallet automatically selects the best funding source using this priority:
	///
	/// 1. **Trusted wallet over Lightning** (BOLT 11/12) – lowest fees for small payments
	/// 2. **Self-custodial Lightning** (BOLT 11/12) – if trusted balance is insufficient
	/// 3. **Trusted wallet on-chain** – for on-chain payment methods
	/// 4. **Self-custodial on-chain** (splice-out) – last resort
	///
	/// After a successful payment, automatic rebalancing may be triggered if the
	/// resulting trusted balance exceeds [`Tunables::trusted_balance_limit`].
	///
	/// Returns a [`PaymentId`] once the payment has been **initiated**. The payment
	/// may still be in-flight; listen for [`Event::PaymentSuccessful`] or
	/// [`Event::PaymentFailed`] to confirm the outcome.
	pub async fn pay(&self, instructions: &PaymentInfo) -> Result<PaymentId, WalletError> {
		let trusted_balance = self.inner.trusted.get_balance().await?;
		let ln_balance = self.inner.ln_wallet.get_balance();

		let mut last_trusted_err = None;
		let mut last_lightning_err = None;

		let mut pay_trusted = async |method: PaymentMethod, ty: fn() -> PaymentType| {
			if instructions.amount <= trusted_balance {
				// attempt to estimate the fee for the trusted payment
				// if we fail to estimate the fee, just assume it is zero and try to pay
				// sometimes the fee estimation can fail but payments can still succeed
				let trusted_fee = match self
					.inner
					.trusted
					.estimate_fee(method.clone(), instructions.amount)
					.await
				{
					Ok(fee) => {
						log_trace!(
							self.inner.logger,
							"Estimated trusted fee: {}msats",
							fee.milli_sats()
						);
						fee
					},

					Err(e) => {
						log_warn!(self.inner.logger, "Failed to estimate trusted fee: {e:?}");
						Amount::ZERO
					},
				};

				if trusted_fee.saturating_add(instructions.amount) <= trusted_balance {
					let res = self.inner.trusted.pay(method, instructions.amount).await;
					match res {
						Ok(id) => {
							self.inner
								.tx_metadata
								.insert(
									PaymentId::Trusted(id),
									TxMetadata {
										ty: TxType::Payment { ty: ty() },
										time: SystemTime::now()
											.duration_since(SystemTime::UNIX_EPOCH)
											.unwrap(),
									},
								)
								.await;
							return Ok(PaymentId::Trusted(id));
						},
						Err(e) => {
							log_debug!(self.inner.logger, "Trusted payment failed with {e:?}");
							last_trusted_err = Some(e.into())
						},
					}
				}
			}
			Err(())
		};

		let mut pay_lightning = async |method, ty: fn() -> PaymentType| {
			let typ = ty();
			let balance = if matches!(typ, PaymentType::OutgoingOnChain { .. }) {
				// if we are paying on-chain, we can either use the on-chain balance or the
				// lightning balance with a splice. Use the larger of the two.
				ln_balance.onchain.max(ln_balance.lightning)
			} else {
				ln_balance.lightning
			};
			if instructions.amount <= balance {
				if let Ok(lightning_fee) =
					self.inner.ln_wallet.estimate_fee(method, instructions.amount).await
				{
					if lightning_fee.saturating_add(instructions.amount) <= balance {
						let res = self.inner.ln_wallet.pay(method, instructions.amount).await;
						match res {
							Ok(id) => {
								// Note that the Payment Id can be repeated if we make a payment,
								// it fails, then we attempt to pay the same (BOLT 11) invoice
								// again.
								self.inner
									.tx_metadata
									.upsert(
										PaymentId::SelfCustodial(id.0),
										TxMetadata {
											ty: TxType::Payment { ty: typ },
											time: SystemTime::now()
												.duration_since(SystemTime::UNIX_EPOCH)
												.unwrap(),
										},
									)
									.await;
								let inner_ref = Arc::clone(&self.inner);
								self.inner.runtime.spawn_cancellable_background_task(async move {
									inner_ref.rebalancer.do_rebalance_if_needed().await;
								});
								return Ok(PaymentId::SelfCustodial(id.0));
							},
							Err(e) => {
								log_debug!(self.inner.logger, "LN payment failed with {:?}", e);
								last_lightning_err = Some(e.into())
							},
						}
					}
				}
			}
			Err(())
		};

		let methods = match &instructions.instructions {
			PaymentInstructions::ConfigurableAmount(conf) => {
				let res =
					conf.clone().set_amount(instructions.amount, &HTTPHrnResolver::new()).await;
				let fixed_instr = res.map_err(|e| {
					log_error!(
						self.inner.logger,
						"Failed to set amount on payment instructions: {e}"
					);
					WalletError::LdkNodeFailure(NodeError::InvalidUri)
				})?;

				fixed_instr.methods().to_vec()
			},
			PaymentInstructions::FixedAmount(fixed) => fixed.methods().to_vec(),
		};

		// First try to pay via the trusted balance over lightning
		for method in methods.clone() {
			match method {
				PaymentMethod::LightningBolt11(_) => {
					if let Ok(id) = pay_trusted(method, || PaymentType::OutgoingLightningBolt11 {
						payment_preimage: None,
					})
					.await
					{
						return Ok(id);
					};
				},
				PaymentMethod::LightningBolt12(_) => {
					if let Ok(id) = pay_trusted(method, || PaymentType::OutgoingLightningBolt12 {
						payment_preimage: None,
					})
					.await
					{
						return Ok(id);
					}
				},
				PaymentMethod::OnChain { .. } => {},
			}
		}

		// If that doesn't work, try to pay via the non-trusted balance over lightning (the
		// trusted balance can top up the lightning balance in the background)
		for method in &methods {
			match method {
				PaymentMethod::LightningBolt11(_) => {
					if let Ok(id) = pay_lightning(method, || PaymentType::OutgoingLightningBolt11 {
						payment_preimage: None,
					})
					.await
					{
						return Ok(id);
					}
				},
				PaymentMethod::LightningBolt12(_) => {
					if let Ok(id) = pay_lightning(method, || PaymentType::OutgoingLightningBolt12 {
						payment_preimage: None,
					})
					.await
					{
						return Ok(id);
					}
				},
				PaymentMethod::OnChain { .. } => {},
			}
		}

		// TODO: Try to MPP the payment using both trusted and LN funds

		// Finally, try trusted on-chain first,
		for method in methods.clone() {
			if let PaymentMethod::OnChain { .. } = method {
				if let Ok(id) =
					pay_trusted(method, || PaymentType::OutgoingOnChain { txid: None }).await
				{
					return Ok(id);
				};
			}
		}

		// then pay on-chain out of the lightning wallet
		for method in &methods {
			if let PaymentMethod::OnChain { .. } = method {
				if let Ok(id) =
					pay_lightning(method, || PaymentType::OutgoingOnChain { txid: None }).await
				{
					return Ok(id);
				};
			}
		}

		Err(last_lightning_err.unwrap_or(
			last_trusted_err.unwrap_or(WalletError::LdkNodeFailure(NodeError::InsufficientFunds)),
		))
	}

	/// Initiates closing all open Lightning channels.
	///
	/// Usable channels are closed cooperatively; non-usable channels (e.g. peer offline)
	/// are force-closed. Force closes have higher on-chain fees and a time-locked delay
	/// before funds become spendable.
	///
	/// This automatically disables rebalancing (see [`set_rebalance_enabled`](Self::set_rebalance_enabled))
	/// to prevent the wallet from immediately reopening channels.
	///
	/// The close is asynchronous — channels are not fully closed until you receive
	/// [`Event::ChannelClosed`] events. On-chain funds from the closed channels will
	/// appear in [`Balances::pending_balance`] until confirmed.
	pub async fn close_channels(&self) -> Result<(), WalletError> {
		// we are explicitly disabling rebalancing here, so that we don't try to
		// reopen channels after closing them.
		self.set_rebalance_enabled(false).await;

		self.inner.ln_wallet.close_channels()?;

		Ok(())
	}

	// Authenticates the user via [LNURL-auth] for the given LNURL string.
	//
	// [LNURL-auth]: https://github.com/lnurl/luds/blob/luds/04.md
	// pub fn lnurl_auth(&self, _lnurl: &str) -> Result<(), WalletError> {
	// 	// todo wait for merge, self.inner.ln_wallet.inner.ldk_node.lnurl_auth(lnurl)?;
	// 	Ok(())
	// }

	/// Returns the [`Tunables`] that were used to configure this wallet.
	pub fn get_tunables(&self) -> Tunables {
		self.inner.tunables
	}

	/// Returns the next event in the event queue, if currently available.
	///
	/// Will return `Some(...)` if an event is available and `None` otherwise.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`Wallet::event_handled`].
	///
	/// **Caution:** Users must handle events as quickly as possible to prevent a large event backlog,
	/// which can increase the memory footprint of [`Wallet`].
	pub fn next_event(&self) -> Option<Event> {
		self.inner.runtime.block_on(self.inner.event_queue.next_event())
	}

	/// Returns the next event in the event queue.
	///
	/// Will asynchronously poll the event queue until the next event is ready.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`Wallet::event_handled`].
	///
	/// **Caution:** Users must handle events as quickly as possible to prevent a large event backlog,
	/// which can increase the memory footprint of [`Wallet`].
	pub async fn next_event_async(&self) -> Event {
		self.inner.event_queue.next_event_async().await
	}

	/// Returns the next event in the event queue.
	///
	/// Will block the current thread until the next event is available.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`Wallet::event_handled`].
	///
	/// **Caution:** Users must handle events as quickly as possible to prevent a large event backlog,
	/// which can increase the memory footprint of [`Wallet`].
	pub fn wait_next_event(&self) -> Event {
		let fut = self.inner.event_queue.next_event_async();
		// We use our runtime for the sync variant to ensure `tokio::task::block_in_place` is
		// always called if we'd ever hit this in an outer runtime context.
		self.inner.runtime.block_on(fut)
	}

	/// Confirm the last retrieved event handled.
	///
	/// **Note:** This **MUST** be called after each event has been handled.
	pub fn event_handled(&self) -> Result<(), ()> {
		let res =
			self.inner.runtime.block_on(self.inner.event_queue.event_handled()).map_err(|e| {
				log_error!(
					self.inner.logger,
					"Couldn't mark event handled due to persistence failure: {e}"
				);
			});
		if res.is_ok() {
			// If an event was handled, probably our balances changed and we may need to rebalance.
			let inner_ref = Arc::clone(&self.inner);
			self.inner.runtime.spawn_cancellable_background_task(async move {
				inner_ref.rebalancer.do_rebalance_if_needed().await;
			});
		}
		res
	}

	/// Returns the wallet's registered [Lightning Address](https://lightningaddress.com),
	/// if one has been set via [`register_lightning_address`](Self::register_lightning_address).
	pub async fn get_lightning_address(&self) -> Result<Option<String>, WalletError> {
		Ok(self.inner.trusted.get_lightning_address().await?)
	}

	/// Registers a [Lightning Address](https://lightningaddress.com) (e.g. `name@domain.com`)
	/// for this wallet.
	///
	/// The `name` parameter is the local part (before the `@`). The domain is determined
	/// by the trusted wallet backend configuration.
	pub async fn register_lightning_address(&self, name: String) -> Result<(), WalletError> {
		Ok(self.inner.trusted.register_lightning_address(name).await?)
	}

	/// Gracefully shuts down the wallet.
	///
	/// This waits for any in-progress rebalances to complete, stops the trusted wallet
	/// backend, shuts down the Lightning node, and cancels background tasks. Call this
	/// before dropping the wallet to ensure data is persisted and resources are released.
	pub async fn stop(&self) {
		// wait for the balance mutex to ensure no other tasks are running
		log_info!(self.inner.logger, "Stopping...");
		log_info!(self.inner.logger, "Stopping rebalancer...");
		let _ = self.inner.rebalancer.stop().await;

		log_info!(self.inner.logger, "Stopping trusted wallet...");
		self.inner.trusted.stop().await;

		log_debug!(self.inner.logger, "Stopping ln wallet...");
		self.inner.ln_wallet.stop();

		// Cancel cancellable background tasks
		self.inner.runtime.abort_cancellable_background_tasks();

		// Wait until non-cancellable background tasks (mod LDK's background processor) are done.
		self.inner.runtime.wait_on_background_tasks();
	}

	/// Manually sync the LDK and BDK wallets with the current chain state and update the fee rate cache.
	///
	/// This is done automatically in the background, but can be triggered manually if needed. Often useful for
	/// testing purposes.
	pub fn sync_ln_wallet(&self) -> Result<(), WalletError> {
		self.inner.ln_wallet.inner.ldk_node.sync_wallets()?;
		Ok(())
	}
}
