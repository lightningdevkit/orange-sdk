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
use bitcoin_payment_instructions::{
	PaymentInstructions, PossiblyResolvedPaymentMethod, http_resolver::HTTPHrnResolver,
};

pub use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use ldk_node::bitcoin::Network;
use ldk_node::bitcoin::io;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::io::sqlite_store::SqliteStore;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::{log_debug, log_info};
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::payment::{PaymentKind as LightningPaymentKind, PaymentKind};
use ldk_node::{BuildError, NodeError, bitcoin};

use tokio::runtime::Runtime;

use std::cmp;
use std::collections::HashMap;
use std::fmt::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

mod lightning_wallet;
pub(crate) mod logging;
mod store;
mod trusted_wallet;

use lightning_wallet::LightningWallet;
use logging::Logger;
use trusted_wallet::Error as TrustedError;
use trusted_wallet::TrustedWalletInterface;

type TrustedWallet = trusted_wallet::SparkWallet;

use store::{PaymentId, TxMetadata, TxMetadataStore, TxType};
pub use store::{PaymentType, Transaction, TxStatus};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Balances {
	pub available_balance: Amount,
	pub pending_balance: Amount,
}

struct WalletImpl {
	ln_wallet: LightningWallet,
	trusted: TrustedWallet,
	tunables: Tunables,
	network: Network,
	tx_metadata: TxMetadataStore,
	balance_mutex: tokio::sync::Mutex<()>,
	store: Arc<dyn KVStore + Send + Sync>,
	logger: Arc<Logger>,
}

pub struct Wallet {
	inner: Arc<WalletImpl>,
}

#[derive(Debug, Clone)]
pub enum VssAuth {
	LNURLAuthServer(String),
	FixedHeaders(HashMap<String, String>),
}

#[derive(Debug, Clone)]
pub struct VssConfig {
	vss_url: String,
	store_id: String,
	headers: VssAuth,
}

#[derive(Debug, Clone)]
pub enum StorageConfig {
	LocalSQLite(String),
	//VSS(VssConfig),
}

#[derive(Debug, Clone)]
pub enum ChainSource {
	//Electrum(String),
	Esplora(String),
	BitcoindRPC { host: String, port: u16, user: String, password: String },
}

#[derive(Debug, Clone)]
pub struct WalletConfig {
	pub storage_config: StorageConfig,
	pub chain_source: ChainSource,
	pub lsp: (SocketAddress, PublicKey, Option<String>),
	pub network: Network,
	pub seed: [u8; 64],
	pub tunables: Tunables,
}

#[derive(Debug, Clone, Copy)]
pub struct Tunables {
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

/// A payable version of [`PaymentInstructions`] (i.e. with a set amount).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentInfo((PaymentInstructions, Amount));

impl PaymentInfo {
	/// Prepares us to pay a [`PaymentInstructions`] by setting the amount.
	///
	/// If [`PaymentInstructions`] already contains an `amount`, the `amount` must match either
	/// [`PaymentInstructions::ln_payment_amount`] or
	/// [`PaymentInstructions::onchain_payment_amount`] exactly. Will return `Err(())` if this
	/// requirement is not met.
	///
	/// Otherwise, this amount can be any value.
	pub fn set_amount(
		instructions: PaymentInstructions, amount: Amount,
	) -> Result<PaymentInfo, ()> {
		Ok(PaymentInfo((instructions, amount)))
		// todo
		// let ln_amt = instructions.ln_payment_amount();
		// let onchain_amt = instructions.onchain_payment_amount();
		// let ln_amt_matches = ln_amt.is_some() && ln_amt.unwrap() == amount;
		// let onchain_amt_matches = onchain_amt.is_some() && onchain_amt.unwrap() == amount;
		//
		// if (ln_amt.is_some() || onchain_amt.is_some()) && !ln_amt_matches && !onchain_amt_matches {
		// 	Err(())
		// } else {
		// }
	}
}

#[derive(Debug)]
pub enum InitFailure {
	IoError(io::Error),
	LdkNodeBuildFailure(BuildError),
	LdkNodeStartFailure(NodeError),
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

#[derive(Debug)]
pub enum WalletError {
	LdkNodeFailure(NodeError),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleUseReceiveUri {
	pub address: Option<bitcoin::Address>,
	pub invoice: Bolt11Invoice,
	pub amount: Option<Amount>,
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
		let (store, logger): (Arc<dyn KVStore + Send + Sync>, _) = match &config.storage_config {
			StorageConfig::LocalSQLite(path) => {
				let mut path: PathBuf = path.into();
				let store = Arc::new(SqliteStore::new(
					path.clone(),
					Some("orange.sqlite".to_owned()),
					None,
				)?);
				path.push("orange.log");
				let logger = Arc::new(Logger::new(&path).expect("Failed to open log file"));
				(store, logger)
			},
		};
		let trusted = TrustedWallet::init(&config, Arc::clone(&logger)).await?;
		let ln_wallet = LightningWallet::init(
			Arc::clone(&runtime),
			config,
			Arc::clone(&store),
			Arc::clone(&logger),
		)?;

		let inner = Arc::new(WalletImpl {
			trusted,
			ln_wallet,
			network,
			tunables,
			tx_metadata: TxMetadataStore::new(Arc::clone(&store)),
			store,
			logger,
			balance_mutex: tokio::sync::Mutex::new(()),
		});

		// Spawn a background thread that every second, we see if we should initiate a rebalance
		// This will withdraw from the trusted balance to our LN balance, possibly opening a channel.
		let inner_ref = Arc::clone(&inner);
		runtime.spawn(async move {
			loop {
				if let Ok(trusted_payments) = inner_ref.trusted.list_payments().await {
					let mut new_txn = Vec::new();
					let mut latest_tx: Option<(Duration, _)> = None;
					for payment in trusted_payments.iter() {
						if payment.outbound {
							// Assume it'll be tracked by the sending task.
							// TODO: Maybe use this to backfill stuff we lost on crash?
							continue;
						}
						let payment_id = PaymentId::Trusted(payment.id);
						let have_metadata =
							if let Some(metadata) = inner_ref.tx_metadata.read().get(&payment_id) {
								if let TxType::Payment { .. } = &metadata.ty {
									if latest_tx.is_none()
										|| latest_tx.as_ref().unwrap().0 < metadata.time
									{
										latest_tx = Some((metadata.time, &payment.id));
									}
								}
								true
							} else {
								false
							};
						if !have_metadata {
							log_info!(
								inner_ref.logger,
								"Received new trusted payment with id {}",
								payment.id
							);
							new_txn.push((payment.amount, &payment.id));
							inner_ref.tx_metadata.insert(
								payment_id,
								TxMetadata {
									ty: TxType::Payment { ty: PaymentType::IncomingLightning {} },
									time: SystemTime::now()
										.duration_since(SystemTime::UNIX_EPOCH)
										.unwrap(),
								},
							);
						}
					}

					if Self::get_rebalance_amt(&inner_ref).is_some() {
						new_txn.sort_unstable();
						let victim_id = new_txn.first().map(|(_, id)| *id).unwrap_or_else(|| {
							// Should only happen due to races settling balance, pick the latest.
							latest_tx
								.expect("We cannot have a balance if we have no transactions")
								.1
						});
						Self::do_trusted_rebalance(&inner_ref, PaymentId::Trusted(*victim_id))
							.await;
					}
				}
				tokio::time::sleep(Duration::from_secs(1)).await;

				inner_ref.trusted.sync().await; // TODO: Remove this when spark fixes their shit
			}
		});

		// TODO: events from ldk-node up
		Ok(Wallet { inner })
	}

	fn get_rebalance_amt(inner: &Arc<WalletImpl>) -> Option<Amount> {
		// We always assume lighting balance is an overestimate by `rebalance_min`.
		let lightning_receivable = inner
			.ln_wallet
			.estimate_receivable_balance()
			.saturating_sub(inner.tunables.rebalance_min);
		let trusted_balance = inner.trusted.get_balance();
		let mut transfer_amt = cmp::min(lightning_receivable, trusted_balance);
		if trusted_balance.saturating_sub(transfer_amt) > inner.tunables.trusted_balance_limit {
			// We need to just get a new channel, there's too much that we need to get to lightning
			transfer_amt = trusted_balance;
		}
		if transfer_amt > inner.tunables.rebalance_min { Some(transfer_amt) } else { None }
	}

	async fn do_trusted_rebalance(inner: &Arc<WalletImpl>, triggering_transaction_id: PaymentId) {
		let _lock = inner.balance_mutex.lock().await;
		log_info!(
			inner.logger,
			"Initiating rebalance, assigning fees to {}",
			triggering_transaction_id
		);

		if let Some(transfer_amt) = Self::get_rebalance_amt(inner) {
			if let Ok(inv) = inner.ln_wallet.get_bolt11_invoice(Some(transfer_amt)).await {
				log_debug!(
					inner.logger,
					"Attempting to pay invoice {inv} to rebalance for {transfer_amt:?}",
				);
				let expected_hash = *inv.payment_hash();
				match inner.trusted.pay(&PaymentMethod::LightningBolt11(inv), transfer_amt).await {
					Ok(rebalance_id) => {
						log_debug!(
							inner.logger,
							"Rebalance trusted transaction initiated, id {rebalance_id}. Waiting for LN payment."
						);
						let mut received_payment_id = None;
						while received_payment_id.is_none() {
							for payment in inner.ln_wallet.list_payments() {
								if let LightningPaymentKind::Bolt11 { hash, .. } = payment.kind {
									if hash.0[..] == expected_hash[..] {
										match payment.status.into() {
											TxStatus::Completed => {
												received_payment_id = Some(payment.id);
											},
											TxStatus::Pending => {},
											TxStatus::Failed => return,
										}
										break;
									}
								}
							}
							if received_payment_id.is_none() {
								inner.ln_wallet.await_payment_receipt().await;
							}
						}
						let lightning_id = received_payment_id.map(|id| id.0).unwrap_or([0; 32]);
						log_info!(
							inner.logger,
							"Rebalance succeeded. Assigned fees to {} for trusted tx {} and lightning tx {}",
							triggering_transaction_id,
							rebalance_id,
							PaymentId::Lightning(lightning_id)
						);
						inner
							.tx_metadata
							.set_tx_caused_rebalance(&triggering_transaction_id)
							.expect(
								"TODO: This is race-y, we really need some kind of mutex on trusted rebalances happening",
							);
						let metadata = TxMetadata {
							ty: TxType::TransferToNonTrusted {
								trusted_payment: rebalance_id,
								lightning_payment: lightning_id,
								payment_triggering_transfer: triggering_transaction_id,
							},
							time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap(),
						};
						inner
							.tx_metadata
							.insert(PaymentId::Trusted(rebalance_id), metadata.clone());
						inner
							.tx_metadata
							.insert(PaymentId::Lightning(lightning_id), metadata.clone());
					},
					Err(e) => {
						log_info!(inner.logger, "Rebalance trusted transaction failed with {e:?}",);
					},
				}
			}
		}
	}

	/// Lists the transactions which have been made.
	pub async fn list_transactions(&self) -> Result<Vec<Transaction>, WalletError> {
		let trusted_payments = self.inner.trusted.list_payments().await?;
		let lightning_payments = self.inner.ln_wallet.list_payments();

		let mut res = Vec::with_capacity(trusted_payments.len() + lightning_payments.len());
		let tx_metadata = self.inner.tx_metadata.read();

		let mut internal_transfers = HashMap::new();
		struct InternalTransfer {
			lightning_receive_fee: Option<Amount>,
			trusted_send_fee: Option<Amount>,
			transaction: Option<Transaction>,
		}

		for payment in trusted_payments {
			if let Some(tx_metadata) = tx_metadata.get(&PaymentId::Trusted(payment.id)) {
				match &tx_metadata.ty {
					TxType::TransferToNonTrusted {
						trusted_payment,
						lightning_payment: _,
						payment_triggering_transfer,
					} => {
						let entry = internal_transfers
							.entry((*payment_triggering_transfer).clone())
							.or_insert(InternalTransfer {
								lightning_receive_fee: None,
								trusted_send_fee: None,
								transaction: None,
							});
						if payment.id == *trusted_payment {
							debug_assert!(entry.trusted_send_fee.is_none());
							entry.trusted_send_fee = Some(payment.fee);
						} else {
							debug_assert!(false);
						}
					},
					TxType::PaymentTriggeringTransferToNonTrusted { ty } => {
						let entry = internal_transfers
							.entry(PaymentId::Trusted(payment.id))
							.or_insert(InternalTransfer {
								lightning_receive_fee: None,
								trusted_send_fee: None,
								transaction: None,
							});
						debug_assert!(entry.transaction.is_none());
						entry.transaction = Some(Transaction {
							status: payment.status,
							outbound: payment.outbound,
							amount: Some(payment.amount),
							fee: Some(payment.fee),
							payment_type: ty.clone(),
							time_since_epoch: tx_metadata.time,
						});
					},
					TxType::Payment { ty } => {
						debug_assert!(!matches!(ty, PaymentType::OutgoingOnChain { .. }));
						debug_assert!(!matches!(ty, PaymentType::IncomingOnChain { .. }));
						res.push(Transaction {
							status: payment.status,
							outbound: payment.outbound,
							amount: Some(payment.amount),
							fee: Some(payment.fee),
							payment_type: ty.clone(),
							time_since_epoch: tx_metadata.time,
						});
					},
				}
			} else {
				// Apparently this can currently happen due to spark-internal transfers
				continue;
				debug_assert!(false, "Missing trusted payment {}", payment.id);
			}
		}
		for payment in lightning_payments {
			use ldk_node::payment::PaymentDirection;
			let lightning_receive_fee = match payment.kind {
				PaymentKind::Bolt11Jit { counterparty_skimmed_fee_msat, .. } => {
					let msats = counterparty_skimmed_fee_msat.unwrap_or(0);
					Some(Amount::from_milli_sats(msats).expect("Must be valid"))
				},
				_ => None,
			};
			if let Some(tx_metadata) = tx_metadata.get(&PaymentId::Lightning(payment.id.0)) {
				match &tx_metadata.ty {
					TxType::TransferToNonTrusted {
						trusted_payment: _,
						lightning_payment,
						payment_triggering_transfer,
					} => {
						let entry = internal_transfers
							.entry(payment_triggering_transfer.clone())
							.or_insert(InternalTransfer {
								lightning_receive_fee: None,
								trusted_send_fee: None,
								transaction: None,
							});
						if payment.id.0 == *lightning_payment {
							debug_assert!(entry.lightning_receive_fee.is_none());
							entry.lightning_receive_fee =
								lightning_receive_fee.or(Some(Amount::ZERO));
						} else {
							debug_assert!(false);
						}
					},
					TxType::PaymentTriggeringTransferToNonTrusted { ty: _ } => {
						let entry = internal_transfers
							.entry(PaymentId::Lightning(payment.id.0))
							.or_insert(InternalTransfer {
								lightning_receive_fee: None,
								trusted_send_fee: None,
								transaction: None,
							});
						debug_assert!(entry.transaction.is_none());
						debug_assert!(payment.direction == PaymentDirection::Outbound);

						entry.transaction = Some(Transaction {
							status: payment.status.into(),
							outbound: payment.direction == PaymentDirection::Outbound,
							amount: Amount::from_milli_sats(payment.amount_msat.unwrap_or(0)).ok(), // TODO: when can this be none https://github.com/lightningdevkit/ldk-node/issues/495
							fee: lightning_receive_fee,
							payment_type: (&payment).into(),
							time_since_epoch: tx_metadata.time,
						});
					},
					TxType::Payment { ty: _ } => {
						res.push(Transaction {
							status: payment.status.into(),
							outbound: payment.direction == PaymentDirection::Outbound,
							amount: Amount::from_milli_sats(payment.amount_msat.unwrap_or(0)).ok(), // TODO: when can this be none https://github.com/lightningdevkit/ldk-node/issues/495
							fee: lightning_receive_fee,
							payment_type: (&payment).into(),
							time_since_epoch: tx_metadata.time,
						})
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
					status,
					outbound: payment.direction == PaymentDirection::Outbound,
					amount: payment.amount_msat.map(|a| Amount::from_milli_sats(a).unwrap()),
					fee: lightning_receive_fee,
					payment_type: (&payment).into(),
					time_since_epoch: Duration::from_secs(payment.latest_update_timestamp),
				})
			}
		}

		for (_, tx_info) in internal_transfers {
			debug_assert!(tx_info.lightning_receive_fee.is_some());
			debug_assert!(tx_info.trusted_send_fee.is_some());
			debug_assert!(tx_info.transaction.is_some());
			if let Some(mut transaction) = tx_info.transaction {
				transaction.fee = Some(
					transaction
						.fee
						.unwrap_or(Amount::ZERO)
						.saturating_add(tx_info.lightning_receive_fee.unwrap_or(Amount::ZERO)),
				);
				transaction.fee = Some(
					transaction
						.fee
						.unwrap_or(Amount::ZERO)
						.saturating_add(tx_info.trusted_send_fee.unwrap_or(Amount::ZERO)),
				);
				res.push(transaction);
			}
		}

		res.sort_by_key(|e| e.time_since_epoch);
		Ok(res)
	}

	/// Gets our current total balance
	pub async fn get_balance(&self) -> Balances {
		let trusted_balance = self.inner.trusted.get_balance();
		let ln_balance = self.inner.ln_wallet.get_balance();
		log_debug!(
			self.inner.logger,
			"Have trusted balance of {trusted_balance:?}, lightning available balance of {:?}, and on chain balance of {:?}",
			ln_balance.lightning,
			ln_balance.onchain
		);
		Balances {
			available_balance: ln_balance.lightning.saturating_add(trusted_balance),
			pending_balance: ln_balance.onchain,
		}
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
		let (enable_onchain, bolt11) = if let Some(amt) = amount {
			let enable_onchain = amt >= self.inner.tunables.onchain_receive_threshold;
			// We always assume lighting balance is an overestimate by `rebalance_min`.
			let lightning_receivable = self
				.inner
				.ln_wallet
				.estimate_receivable_balance()
				.saturating_sub(self.inner.tunables.rebalance_min);
			let use_trusted =
				amt <= self.inner.tunables.trusted_balance_limit && amt >= lightning_receivable;

			let bolt11 = if use_trusted {
				self.inner.trusted.get_bolt11_invoice(amount).await?
			} else {
				self.inner.ln_wallet.get_bolt11_invoice(amount).await?
			};
			(enable_onchain, bolt11)
		} else {
			(
				self.inner.tunables.enable_amountless_receive_on_chain,
				self.inner.trusted.get_bolt11_invoice(amount).await?,
			)
		};
		if enable_onchain {
			let address = self.inner.ln_wallet.get_on_chain_address()?;
			Ok(SingleUseReceiveUri { address: Some(address), invoice: bolt11, amount })
		} else {
			Ok(SingleUseReceiveUri { address: None, invoice: bolt11, amount })
		}
	}

	/// Parses payment instructions from an arbitrary string provided by a user, scanned from a QR
	/// code, or read from a link which the user opened with this application.
	///
	/// See [`PaymentInstructions`] for the list of supported formats.
	///
	/// Note that a user might also be pasting or scanning a QR code containing a lightning BOLT 12
	/// refund, which allow us to *receive* funds, and must be parsed with
	/// [`Self::parse_claim_instructions`].
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
		todo!()
	}

	// returns true once the payment is pending
	pub async fn pay(&self, instructions: &PaymentInfo) -> Result<(), WalletError> {
		let trusted_balance = self.inner.trusted.get_balance();
		let ln_balance = self.inner.ln_wallet.get_balance();

		let mut last_trusted_err = None;
		let mut last_lightning_err = None;

		let mut pay_trusted = async |method, ty: &dyn Fn() -> PaymentType| {
			if instructions.0.1 <= trusted_balance {
				if let Ok(trusted_fee) =
					self.inner.trusted.estimate_fee(method, instructions.0.1).await
				{
					if trusted_fee.saturating_add(instructions.0.1) <= trusted_balance {
						let res = self.inner.trusted.pay(method, instructions.0.1).await;
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
								log_debug!(
									self.inner.logger,
									"Trusted payment failed with {:?}",
									e
								);
								last_trusted_err = Some(e.into())
							},
						}
					}
				}
			}
			Err(())
		};

		let mut pay_lightning = async |method, ty: &dyn Fn() -> PaymentType| {
			if instructions.0.1 <= ln_balance.lightning {
				if let Ok(lightning_fee) =
					self.inner.ln_wallet.estimate_fee(method, instructions.0.1).await
				{
					if lightning_fee.saturating_add(instructions.0.1) <= ln_balance.lightning {
						let res = self.inner.ln_wallet.pay(method, instructions.0.1).await;
						match res {
							Ok(id) => {
								// Note that the Payment Id can be repeated if we make a payment,
								// it fails, then we attempt to pay the same (BOLT 11) invoice
								// again.
								self.inner.tx_metadata.upsert(
									PaymentId::Lightning(id.0),
									TxMetadata {
										ty: TxType::Payment { ty: ty() },
										time: SystemTime::now()
											.duration_since(SystemTime::UNIX_EPOCH)
											.unwrap(),
									},
								);
								let inner_ref = Arc::clone(&self.inner);
								tokio::spawn(async move {
									Self::do_trusted_rebalance(
										&inner_ref,
										PaymentId::Lightning(id.0),
									)
									.await
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

		let methods = match &instructions.0.0 {
			PaymentInstructions::ConfigurableAmount(conf) => conf
				.methods()
				.map(|m| match m {
					PossiblyResolvedPaymentMethod::LNURLPay { .. } => {
						todo!("resolve lnurl to invoice")
					},
					PossiblyResolvedPaymentMethod::Resolved(resolved) => resolved.to_owned(),
				})
				.collect(),
			PaymentInstructions::FixedAmount(fixed) => fixed.methods().to_vec(),
		};

		// First try to pay via the trusted balance over lightning
		for method in &methods {
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
		for method in &methods {
			if let PaymentMethod::OnChain { .. } = method {
				if pay_trusted(method, &|| PaymentType::OutgoingOnChain {}).await.is_ok() {
					return Ok(());
				};
			}
		}

		// then pay on-chain out of the lightning wallet
		for method in &methods {
			if let PaymentMethod::OnChain { .. } = method {
				if pay_lightning(method, &|| PaymentType::OutgoingOnChain {}).await.is_ok() {
					return Ok(());
				};
			}
		}

		Err(last_lightning_err.unwrap_or(
			last_trusted_err.unwrap_or(WalletError::LdkNodeFailure(NodeError::InsufficientFunds)),
		))
	}
}

impl Drop for Wallet {
	fn drop(&mut self) {
		self.inner.ln_wallet.stop();
	}
}

#[cfg(test)]
mod tests {
	#[test]
	fn test_node_start() {
		// TODO
	}
}
