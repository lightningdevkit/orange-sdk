#![deny(missing_docs)]

//! A library implementing the full backend for a modern, highly usable, Bitcoin wallet focusing on
//! maximizing security and self-custody without trading off user experience.
//!
//! This crate should do everything you need to build a great Bitcoin wallet, except the UI.
//!
//! In order to maximize the user experience, small balances are held in a trusted service (XXX
//! which one), avoiding expensive setup fees, while larger balances are moved into on-chain
//! lightning channels, ensuring trust is minimized in the trusted service.
//!
//! Despite funds being stored in multiple places, the full balance can be treated as a single
//! wallet - payments can draw on both balances simultaneously and deposits are automatically
//! shifted to minimize fees and ensure maximal security.

use bitcoin_payment_instructions as instructions;
use bitcoin_payment_instructions::{PaymentInstructions, http_resolver::HTTPHrnResolver};

pub use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use crate::rebalancer::{OrangeRebalanceEventHandler, OrangeTrigger};
use crate::store::{PaymentId, TxMetadata, TxMetadataStore, TxType};
use crate::trusted_wallet::cashu::Cashu;
#[cfg(feature = "_test-utils")]
use crate::trusted_wallet::dummy::DummyTrustedWallet;
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
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::{log_debug, log_error, log_info, log_trace, log_warn};
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::payment::PaymentKind;
use ldk_node::{BuildError, ChannelDetails, NodeError};

use tokio::runtime::Runtime;

use std::collections::HashMap;
use std::fmt::{self, Debug, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub mod builder;
mod event;
mod lightning_wallet;
pub(crate) mod logging;
mod rebalancer;
mod store;
pub mod trusted_wallet;

use lightning_wallet::LightningWallet;
use logging::Logger;
use trusted_wallet::TrustedError;

pub use bitcoin_payment_instructions;
pub use builder::{BuilderError, WalletBuilder};
pub use event::{Event, EventQueue};
pub use ldk_node::bip39::Mnemonic;
pub use ldk_node::bitcoin;
pub use ldk_node::payment::ConfirmationStatus;
pub use spark_wallet::{OperatorPoolConfig, ServiceProviderConfig, SparkWalletConfig};
pub use store::{PaymentType, Transaction, TxStatus};
pub use trusted_wallet::ExtraConfig;

type Rebalancer = GraduatedRebalancer<
	WalletTrusted<DynTrustedWalletInterface>,
	LightningWallet,
	OrangeTrigger,
	OrangeRebalanceEventHandler,
	Logger,
>;

/// Represents the balances of the wallet, including available and pending balances.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Balances {
	/// The balance in trusted wallet
	pub trusted: Amount,
	/// The balance in lightning wallet available for spending.
	pub lightning: Amount,
	/// The balance that is pending and not yet spendable.
	/// This includes all on-chain balances. The on-chain balance will become
	/// available after it has been spliced into a lightning channel.
	pub pending_balance: Amount,
}

impl Balances {
	/// Returns the total available balance, which is the sum of the lightning and trusted balances.
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
	store: Arc<dyn KVStore + Send + Sync>,
	/// Logger for logging wallet operations.
	logger: Arc<Logger>,
}

// todo better doc, include examples, etc
/// The primary entry point for orange-sdk. This is the main wallet struct that
/// contains the trusted wallet and the lightning wallet.
#[derive(Clone)]
pub struct Wallet {
	/// The internal wallet implementation.
	inner: Arc<WalletImpl>,
}

/// Represents the seed used for wallet generation.
#[derive(Debug, Clone)]
pub enum Seed {
	/// A BIP 39 mnemonic seed.
	Mnemonic {
		/// The mnemonic phrase.
		mnemonic: Mnemonic,
		/// The passphrase for the mnemonic.
		passphrase: Option<String>,
	},
	/// A 64-byte seed for the wallet.
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
	vss_url: String,
	/// The store ID for the VSS.
	store_id: String,
	/// Authentication method for the VSS.
	headers: VssAuth,
}

/// Configuration for wallet storage, either local SQLite or VSS.
#[derive(Debug, Clone)]
pub enum StorageConfig {
	/// Local SQLite database configuration.
	LocalSQLite(String),
	// todo VSS(VssConfig),
}

/// Configuration for the blockchain data source.
#[derive(Debug, Clone)]
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

/// Configuration for initializing the wallet.
pub struct WalletConfig {
	/// Configuration for wallet storage.
	pub storage_config: StorageConfig,
	/// Location of the wallet's log file.
	pub log_file: PathBuf,
	/// Configuration for the blockchain data source.
	pub chain_source: ChainSource,
	/// Lightning Service Provider (LSP) configuration.
	/// The address to connect to the LSP, the LSP node id, and an optional auth token.
	pub lsp: (SocketAddress, PublicKey, Option<String>),
	/// URL to download a scorer from. This is for the lightning node to get its route
	/// scorer from a remote server instead of having to probe and find optimal routes
	/// locally.
	pub scorer_url: Option<String>,
	/// The Bitcoin network the wallet operates on.
	pub network: Network,
	/// The seed used for wallet generation.
	pub seed: Seed,
	/// Configuration parameters for when the wallet decides to use the lightning or trusted wallet.
	pub tunables: Tunables,
	/// Extra configuration specific to the trusted wallet implementation.
	pub extra_config: ExtraConfig,
}

/// Configuration parameters for when the wallet decides to use the lightning or trusted wallet.
#[derive(Debug, Clone, Copy)]
pub struct Tunables {
	/// The maximum balance that can be held in the trusted wallet.
	pub trusted_balance_limit: Amount,
	/// Trusted balances below this threshold will not be transferred to non-trusted balance
	/// even if we have capacity to do so without paying for a new channel.
	///
	/// This avoids unnecessary transfers and fees.
	pub rebalance_min: Amount,
	/// Payment instructions generated using [`Wallet::get_single_use_receive_uri`] for an amount
	/// below this threshold will not include an on-chain address.
	pub onchain_receive_threshold: Amount,
	/// Payment instructions generated using [`Wallet::get_single_use_receive_uri`] with no amount
	/// will only include an on-chain address if this is set.
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

/// Represents errors that can occur when building a [`PaymentInfo`] from [`PaymentInstructions`].
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

/// A payable version of [`PaymentInstructions`] (i.e. with a set amount).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentInfo {
	/// The payment instructions (e.g., BOLT 11 invoice, on-chain address).
	instructions: PaymentInstructions,
	/// The amount to be paid.
	amount: Amount,
}

impl PaymentInfo {
	/// Prepares us to pay a [`PaymentInstructions`] by setting the amount.
	///
	/// If [`PaymentInstructions`] is a [`PaymentInstructions::ConfigurableAmount`], the amount must be
	/// within the specified range (if any).
	///
	/// If [`PaymentInstructions`] is a [`PaymentInstructions::FixedAmount`], the amount must match the
	/// fixed on-chain or lightning amount specified.
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
}

/// Represents possible failures during wallet initialization.
#[derive(Debug)]
pub enum InitFailure {
	/// Failure to build the wallet using the builder pattern.
	BuildError(BuilderError),
	/// I/O error during initialization.
	IoError(io::Error),
	/// Failure to build the LDK node.
	LdkNodeBuildFailure(BuildError),
	/// Failure to start the LDK node.
	LdkNodeStartFailure(NodeError),
	/// Failure in the trusted wallet implementation.
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

/// Represents possible errors during wallet operations.
#[derive(Debug)]
pub enum WalletError {
	/// Failure in the LDK node.
	LdkNodeFailure(NodeError),
	/// Failure in the trusted wallet implementation.
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

/// Represents a single-use Bitcoin URI for receiving payments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleUseReceiveUri {
	/// The optional on-chain Bitcoin address. Will be present based on the
	/// wallet's configured tunables.
	pub address: Option<bitcoin::Address>,
	/// The BOLT 11 Lightning invoice.
	pub invoice: Bolt11Invoice,
	/// The optional amount for the payment.
	pub amount: Option<Amount>,
	/// Whether the URI was generated from a trusted wallet or from
	/// the self-custodial LN wallet.
	pub from_trusted: bool,
}

impl fmt::Display for SingleUseReceiveUri {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let mut uri = "BITCOIN:".to_owned();
		match &self.address {
			Some(address) => {
				write!(&mut uri, "{address}")?;
				if let Some(amt) = self.amount {
					write!(&mut uri, "?AMOUNT={}&", amt.btc_decimal_rounding_up_to_sats())?;
				} else {
					write!(&mut uri, "?")?;
				}
			},
			None => {
				write!(&mut uri, "?")?;
			},
		}
		write!(&mut uri, "LIGHTNING={}", self.invoice)?;

		let res = uri.to_ascii_uppercase();
		write!(f, "{}", res)
	}
}

impl Wallet {
	/// Constructs a new Wallet.
	///
	/// `runtime` must be a reference to the running `tokio` runtime which we are currently
	/// operating in.
	// TODO: WOW that is a terrible API lol
	pub async fn new(runtime: Arc<Runtime>, config: WalletConfig) -> Result<Wallet, InitFailure> {
		let tunables = config.tunables;
		let network = config.network;
		let logger = Arc::new(Logger::new(&config.log_file).expect("Failed to open log file"));

		log_info!(logger, "Initializing orange on network: {network}");

		let store: Arc<dyn KVStore + Send + Sync> = match &config.storage_config {
			StorageConfig::LocalSQLite(path) => {
				Arc::new(SqliteStore::new(path.into(), Some("orange.sqlite".to_owned()), None)?)
			},
		};

		let event_queue = Arc::new(EventQueue::new(Arc::clone(&store), Arc::clone(&logger)));

		let trusted: Arc<Box<DynTrustedWalletInterface>> = match &config.extra_config {
			ExtraConfig::Spark(_) => Arc::new(Box::new(
				Spark::init(
					&config,
					Arc::clone(&store),
					Arc::clone(&event_queue),
					Arc::clone(&logger),
				)
				.await?,
			)),
			ExtraConfig::Cashu(_) => Arc::new(Box::new(
				Cashu::init(
					&config,
					Arc::clone(&store),
					Arc::clone(&event_queue),
					Arc::clone(&logger),
				)
				.await?,
			)),
			#[cfg(feature = "_test-utils")]
			ExtraConfig::Dummy(cfg) => Arc::new(Box::new(
				DummyTrustedWallet::new(cfg.uuid, &cfg.lsp, &cfg.bitcoind, cfg.rt.clone()).await,
			)),
		};

		let ln_wallet = Arc::new(
			LightningWallet::init(
				Arc::clone(&runtime),
				config,
				Arc::clone(&store),
				Arc::clone(&event_queue),
				Arc::clone(&logger),
			)
			.await?,
		);

		let tx_metadata = TxMetadataStore::new(Arc::clone(&store));

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

		let inner = Arc::new(WalletImpl {
			trusted,
			ln_wallet,
			event_queue,
			network,
			tunables,
			rebalancer: Arc::clone(&rebalancer),
			tx_metadata,
			store,
			logger,
		});

		// Spawn a background thread that every second, we see if we should initiate a rebalance
		// This will withdraw from the trusted balance to our LN balance, possibly opening a channel.
		runtime.spawn(async move {
			loop {
				rebalancer.do_rebalance_if_needed().await;

				tokio::time::sleep(Duration::from_secs(1)).await;
			}
		});

		Ok(Wallet { inner })
	}

	/// Sets whether the wallet should automatically rebalance from trusted/onchain to lightning.
	pub fn set_rebalance_enabled(&self, value: bool) {
		store::set_rebalance_enabled(self.inner.store.as_ref(), value);
	}

	/// Whether the wallet should automatically rebalance from trusted/onchain to lightning.
	pub fn get_rebalance_enabled(&self) -> bool {
		store::get_rebalance_enabled(self.inner.store.as_ref())
	}

	/// Returns the lightning wallet's node id.
	pub fn node_id(&self) -> PublicKey {
		self.inner.ln_wallet.inner.ldk_node.node_id()
	}

	/// Check if the lightning wallet is currently connected to the LSP.
	pub fn is_connected_to_lsp(&self) -> bool {
		self.inner.ln_wallet.is_connected_to_lsp()
	}

	/// List our current channels
	pub fn channels(&self) -> Vec<ChannelDetails> {
		self.inner.ln_wallet.inner.ldk_node.list_channels()
	}

	/// Lists the transactions which have been made.
	pub async fn list_transactions(&self) -> Result<Vec<Transaction>, WalletError> {
		let trusted_payments = self.inner.trusted.list_payments().await?;
		let lightning_payments = self.inner.ln_wallet.list_payments();

		let mut res = Vec::with_capacity(trusted_payments.len() + lightning_payments.len());
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
				}
			} else {
				debug_assert!(
					!payment.outbound,
					"Missing outbound trusted payment metadata entry on {:?}",
					payment.id
				);

				if payment.status != TxStatus::Completed {
					// We don't bother to surface pending inbound transactions (i.e. issued but
					// unpaid invoices) in our transaction list.
					continue;
				}
				res.push(Transaction {
					id: PaymentId::Trusted(payment.id),
					status: payment.status,
					outbound: payment.outbound,
					amount: Some(payment.amount),
					fee: Some(payment.fee),
					payment_type: PaymentType::IncomingLightning {},
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
			let fee = match payment.fee_paid_msat {
				None => lightning_receive_fee,
				Some(fee) => Some(
					Amount::from_milli_sats(fee)
						.unwrap()
						.saturating_add(lightning_receive_fee.unwrap_or(Amount::ZERO)),
				),
			};
			if let Some(tx_metadata) = tx_metadata.get(&PaymentId::Lightning(payment.id.0)) {
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
							.entry(PaymentId::Lightning(triggering_txid.to_byte_array()))
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
							.entry(PaymentId::Lightning(payment.id.0))
							.or_insert(InternalTransfer {
								receive_fee: lightning_receive_fee,
								send_fee: None,
								transaction: None,
							});
						debug_assert!(entry.transaction.is_none());

						entry.transaction = Some(Transaction {
							id: PaymentId::Lightning(payment.id.0),
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
						id: PaymentId::Lightning(payment.id.0),
						status: payment.status.into(),
						outbound: payment.direction == PaymentDirection::Outbound,
						amount: payment
							.amount_msat
							.map(|a| Amount::from_milli_sats(a).expect("Must be valid")),
						fee,
						payment_type: (&payment).into(),
						time_since_epoch: tx_metadata.time,
					}),
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
					id: PaymentId::Lightning(payment.id.0),
					status,
					outbound: payment.direction == PaymentDirection::Outbound,
					amount: payment.amount_msat.map(|a| Amount::from_milli_sats(a).unwrap()),
					fee,
					payment_type: (&payment).into(),
					time_since_epoch: Duration::from_secs(payment.latest_update_timestamp),
				})
			}
		}

		for (_, tx_info) in internal_transfers {
			debug_assert!(
				tx_info.send_fee.is_some(),
				"Internal transfers must have a send fee, got {tx_info:?}",
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

	/// Gets our current total balance
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
	///
	// Note that a user might also be pasting or scanning a QR code containing a lightning BOLT 12
	// refund, which allow us to *receive* funds, and must be parsed with
	// [`Self::parse_claim_instructions`].
	pub async fn parse_payment_instructions(
		&self, instructions: &str,
	) -> Result<PaymentInstructions, instructions::ParseError> {
		PaymentInstructions::parse(instructions, self.inner.network, &HTTPHrnResolver, true).await
	}

	/*/// Verifies instructions which allow us to claim funds given as:
	///  * A lightning BOLT 12 refund
	///  * An on-chain private key, which we will sweep
	// TODO: consider LNURL claim thing
	pub fn parse_claim_instructions(&self, instructions: &str) -> Result<..., ...> {

	}*/

	/// Estimates the fees required to pay a [`PaymentInstructions`]
	pub async fn estimate_fee(&self, _payment_info: &PaymentInstructions) -> Amount {
		// todo implement fee estimation
		Amount::ZERO
	}

	/// Initiates a payment using the provided [`PaymentInfo`]. This will pay from the trusted
	/// wallet if possible, otherwise it will pay from the lightning wallet.
	///
	/// If applicable, this will also initiate a rebalance from the trusted wallet to the
	/// lightning wallet based on the resulting balance and configured tunables.
	///
	/// Returns true once the payment is pending, however, this does not mean that the
	/// payment has been completed. The payment may still fail.
	pub async fn pay(&self, instructions: &PaymentInfo) -> Result<(), WalletError> {
		let trusted_balance = self.inner.trusted.get_balance().await?;
		let ln_balance = self.inner.ln_wallet.get_balance();

		let mut last_trusted_err = None;
		let mut last_lightning_err = None;

		let mut pay_trusted = async |method: PaymentMethod, ty: &dyn Fn() -> PaymentType| {
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
							self.inner.tx_metadata.insert(
								PaymentId::Trusted(id),
								TxMetadata {
									ty: TxType::Payment { ty: ty() },
									time: SystemTime::now()
										.duration_since(SystemTime::UNIX_EPOCH)
										.unwrap(),
								},
							);
							return Ok(());
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

		let mut pay_lightning = async |method, ty: &dyn Fn() -> PaymentType| {
			let typ = ty();
			let balance = if matches!(typ, PaymentType::OutgoingOnChain { .. }) {
				ln_balance.onchain
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
								self.inner.tx_metadata.upsert(
									PaymentId::Lightning(id.0),
									TxMetadata {
										ty: TxType::Payment { ty: typ },
										time: SystemTime::now()
											.duration_since(SystemTime::UNIX_EPOCH)
											.unwrap(),
									},
								);
								let inner_ref = Arc::clone(&self.inner);
								tokio::spawn(async move {
									inner_ref.rebalancer.do_rebalance_if_needed().await;
								});
								return Ok(());
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
				let res = conf.clone().set_amount(instructions.amount, &HTTPHrnResolver).await;
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
					if pay_trusted(method, &|| PaymentType::OutgoingLightningBolt11 {
						payment_preimage: None,
					})
					.await
					.is_ok()
					{
						return Ok(());
					};
				},
				PaymentMethod::LightningBolt12(_) => {
					if pay_trusted(method, &|| PaymentType::OutgoingLightningBolt12 {
						payment_preimage: None,
					})
					.await
					.is_ok()
					{
						return Ok(());
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
					if pay_lightning(method, &|| PaymentType::OutgoingLightningBolt11 {
						payment_preimage: None,
					})
					.await
					.is_ok()
					{
						return Ok(());
					}
				},
				PaymentMethod::LightningBolt12(_) => {
					if pay_lightning(method, &|| PaymentType::OutgoingLightningBolt12 {
						payment_preimage: None,
					})
					.await
					.is_ok()
					{
						return Ok(());
					}
				},
				PaymentMethod::OnChain { .. } => {},
			}
		}

		//TODO: Try to MPP the payment using both trusted and LN funds

		// Finally, try trusted on-chain first,
		for method in methods.clone() {
			if let PaymentMethod::OnChain { .. } = method {
				if pay_trusted(method, &|| PaymentType::OutgoingOnChain { txid: None })
					.await
					.is_ok()
				{
					return Ok(());
				};
			}
		}

		// then pay on-chain out of the lightning wallet
		for method in &methods {
			if let PaymentMethod::OnChain { .. } = method {
				if pay_lightning(method, &|| PaymentType::OutgoingOnChain { txid: None })
					.await
					.is_ok()
				{
					return Ok(());
				};
			}
		}

		Err(last_lightning_err.unwrap_or(
			last_trusted_err.unwrap_or(WalletError::LdkNodeFailure(NodeError::InsufficientFunds)),
		))
	}

	/// Initiates closing all channels in the lightning wallet. The channel will not be closed
	/// until a [`Event::ChannelClosed`] event is emitted.
	/// This will disable rebalancing before closing channels, so that we don't try to reopen them.
	pub fn close_channels(&self) -> Result<(), WalletError> {
		// we are explicitly disabling rebalancing here, so that we don't try to
		// reopen channels after closing them.
		self.set_rebalance_enabled(false);

		self.inner.ln_wallet.close_channels()?;

		Ok(())
	}

	/// Returns the wallet's configured tunables.
	pub fn get_tunables(&self) -> Tunables {
		self.inner.tunables
	}

	/// Returns the next event in the event queue, if currently available.
	///
	/// Will return `Some(..)` if an event is available and `None` otherwise.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`Wallet::event_handled`].
	///
	/// **Caution:** Users must handle events as quickly as possible to prevent a large event backlog,
	/// which can increase the memory footprint of [`Wallet`].
	pub fn next_event(&self) -> Option<Event> {
		self.inner.event_queue.next_event()
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
		self.inner.event_queue.wait_next_event()
	}

	/// Confirm the last retrieved event handled.
	///
	/// **Note:** This **MUST** be called after each event has been handled.
	pub fn event_handled(&self) -> Result<(), ()> {
		self.inner.event_queue.event_handled().map_err(|e| {
			log_error!(
				self.inner.logger,
				"Couldn't mark event handled due to persistence failure: {e}"
			);
		})
	}

	/// Stops the wallet, which will stop the underlying LDK node and any background tasks.
	/// This will ensure that any critical tasks have completed before stopping.
	pub async fn stop(&self) {
		// wait for the balance mutex to ensure no other tasks are running
		log_info!(self.inner.logger, "Stopping...");
		log_info!(self.inner.logger, "Stopping rebalancer...");
		let _ = self.inner.rebalancer.stop().await;

		log_info!(self.inner.logger, "Stopping trusted wallet...");
		self.inner.trusted.stop().await;

		log_debug!(self.inner.logger, "Stopping ln wallet...");
		self.inner.ln_wallet.stop();
	}
}
