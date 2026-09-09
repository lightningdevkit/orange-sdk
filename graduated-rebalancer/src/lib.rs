#![deny(missing_docs)]
#![allow(clippy::type_complexity)]

//! A library for managing graduated rebalancing between trusted and lightning wallets.
//!
//! This crate provides a `GraduatedRebalancer` that automatically manages the balance
//! between trusted wallets (for small amounts) and lightning wallets (for larger amounts)
//! based on configurable thresholds.

use bitcoin_payment_instructions::amount::Amount;
use bitcoin_payment_instructions::PaymentMethod;
use lightning::bitcoin::hex::DisplayHex;
use lightning::bitcoin::OutPoint;
use lightning::util::logger::Logger;
use lightning::{log_debug, log_error, log_info};
use lightning_invoice::Bolt11Invoice;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A registered rebalance result. Resolves after both payments succeed, or `None` on failure.
pub type RebalanceWait = Pin<Box<dyn Future<Output = Option<RebalanceReceipt>> + Send + 'static>>;

/// The payment receipts for a completed trusted-to-Lightning rebalance.
#[derive(Debug)]
pub struct RebalanceReceipt {
	/// The incoming Lightning payment.
	pub lightning: ReceivedLightningPayment,
	/// The outgoing trusted payment.
	pub trusted: ReceivedLightningPayment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Parameters for doing a rebalance
pub struct TriggerParams {
	/// ID for the rebalance operation, useful for tracking
	/// and logging purposes.
	pub id: [u8; 32],
	/// The amount to transfer in millisatoshis
	pub amount: Amount,
}

/// Create separate triggers for trusted -> LN and onchain -> LN on when to do a rebalance.
pub trait RebalanceTrigger: Send + Sync {
	/// If we need to do a Trusted -> LN rebalance, if the amount is None, no rebalance will be triggered.
	fn needs_trusted_rebalance(&self) -> impl Future<Output = Option<TriggerParams>> + Send;

	/// If we need to do an Onchain -> LN rebalance, if the amount is None, no rebalance will be triggered.
	fn needs_onchain_rebalance(&self) -> impl Future<Output = Option<TriggerParams>> + Send;
}

/// Configuration parameters for rebalancing decisions
#[derive(Debug, Clone, Copy)]
pub struct RebalanceTunables {
	/// The maximum balance that can be held in the trusted wallet.
	pub trusted_balance_limit: Amount,
	/// Trusted balances below this threshold will not be transferred to non-trusted balance
	/// even if we have capacity to do so without paying for a new channel.
	///
	/// This avoids unnecessary transfers and fees.
	pub rebalance_min: Amount,
}

impl Default for RebalanceTunables {
	fn default() -> Self {
		Self {
			trusted_balance_limit: Amount::from_sats(100_000).expect("valid amount"),
			rebalance_min: Amount::from_sats(5_000).expect("valid amount"),
		}
	}
}

/// Trait representing a trusted wallet backend
pub trait TrustedWallet: Send + Sync {
	/// Error type for trusted wallet operations
	type Error: Debug + Send + Sync + 'static;

	/// Get the current balance of the trusted wallet
	fn get_balance(&self)
		-> Pin<Box<dyn Future<Output = Result<Amount, Self::Error>> + Send + '_>>;

	/// Generate a BOLT11 invoice for the specified amount
	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> Pin<Box<dyn Future<Output = Result<Bolt11Invoice, Self::Error>> + Send + '_>>;

	/// Make a payment using the trusted wallet
	fn pay(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<[u8; 32], Self::Error>> + Send + '_>>;

	/// Estimate the fee for making a payment using the trusted wallet
	fn estimate_fee(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<Amount, Self::Error>> + Send + '_>>;
}

/// Trait representing a lightning wallet backend
pub trait LightningWallet: Send + Sync {
	/// Error type for lightning wallet operations
	type Error: Debug + Send + Sync + 'static;

	/// Get the current balance of the lightning wallet
	fn get_balance(&self) -> LightningBalance;

	/// Generate a BOLT11 invoice for the specified amount
	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> Pin<Box<dyn Future<Output = Result<Bolt11Invoice, Self::Error>> + Send + '_>>;

	/// Make a payment using the lightning wallet
	fn pay(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<[u8; 32], Self::Error>> + Send + '_>>;

	/// Watch both payments before the rebalance starts.
	/// Retain results that arrive before the future is polled. Resolve only after both
	/// payments succeed, or return `None` if either fails or tracking stops.
	fn watch_rebalance(&self, payment_hash: [u8; 32]) -> RebalanceWait;

	/// Check if we already have a channel with the LSP
	fn has_channel_with_lsp(&self) -> bool;

	/// Open a channel with the LSP using all available on-chain funds
	/// (minus fees and anchor reserves).
	fn open_channel_with_lsp(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<u128, Self::Error>> + Send + '_>>;

	/// Wait for a channel pending notification, returns the new channel's outpoint
	fn await_channel_pending(
		&self, channel_id: u128,
	) -> Pin<Box<dyn Future<Output = OutPoint> + Send + '_>>;

	/// Splice all available on-chain funds (minus fees and anchor reserves) into
	/// an existing channel with the LSP.
	fn splice_to_lsp_channel(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<u128, Self::Error>> + Send + '_>>;

	/// Wait for a splice pending notification, returns the splice outpoint
	fn await_splice_pending(
		&self, channel_id: u128,
	) -> Pin<Box<dyn Future<Output = OutPoint> + Send + '_>>;
}

/// Represents a payment from the lightning wallet
#[derive(Debug, Clone)]
pub struct ReceivedLightningPayment {
	/// Unique payment ID
	pub id: [u8; 32],
	/// Fee paid in millisatoshis
	pub fee_paid_msat: Option<u64>,
}

/// Lightning wallet balance information
#[derive(Debug, Clone, Copy)]
pub struct LightningBalance {
	/// Available lightning balance
	pub lightning: Amount,
	/// Available on-chain balance
	pub onchain: Amount,
}

/// Events that can be emitted by the rebalancer
#[derive(Debug, Clone)]
pub enum RebalancerEvent {
	/// Rebalance was initiated
	RebalanceInitiated {
		/// Optional trigger id given by the rebalance trigger
		trigger_id: [u8; 32],
		/// Trusted wallet payment ID for the rebalance
		trusted_rebalance_payment_id: [u8; 32],
		/// Hash of the invoice the trusted wallet pays, which identifies the Lightning receipt
		payment_hash: [u8; 32],
		/// Amount being rebalanced in millisatoshis
		amount_msat: u64,
	},
	/// Rebalance completed successfully
	RebalanceSuccessful {
		/// Trigger id given by the rebalance trigger
		trigger_id: [u8; 32],
		/// Trusted wallet payment ID for the rebalance
		trusted_rebalance_payment_id: [u8; 32],
		/// Lightning payment ID for the rebalance
		ln_rebalance_payment_id: [u8; 32],
		/// Amount rebalanced in millisatoshis
		amount_msat: u64,
		/// Total fee paid in millisatoshis
		fee_msat: u64,
	},
	/// We have initiated a lightning channel open
	OnChainRebalanceInitiated {
		/// Trigger id given by the rebalance trigger
		trigger_id: [u8; 32],
		/// User channel id set by the LN wallet
		user_channel_id: u128,
		/// New channel Outpoint
		channel_outpoint: OutPoint,
	},
}

/// Trait for handling rebalancer events
pub trait EventHandler: Send + Sync {
	/// Handle a rebalancer event
	fn handle_event(&self, event: RebalancerEvent)
		-> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// A no-op event handler that discards all events
#[derive(Debug, Copy, Clone, Default)]
pub struct IgnoringEventHandler;

impl EventHandler for IgnoringEventHandler {
	fn handle_event(
		&self, _event: RebalancerEvent,
	) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		Box::pin(async move {})
		// Do nothing
	}
}

/// The main graduated rebalancer that manages balance between trusted and lightning wallets
pub struct GraduatedRebalancer<
	T: TrustedWallet,
	L: LightningWallet,
	R: RebalanceTrigger,
	E: EventHandler,
	O: Logger,
> {
	trusted: Arc<T>,
	ln_wallet: Arc<L>,
	trigger: Arc<R>,
	event_handler: Arc<E>,
	logger: Arc<O>,

	/// Mutex to ensure thread-safe balance operations.
	balance_mutex: tokio::sync::Mutex<()>,
}

impl<T, LN, R, E, L> GraduatedRebalancer<T, LN, R, E, L>
where
	T: TrustedWallet,
	LN: LightningWallet,
	R: RebalanceTrigger,
	E: EventHandler,
	L: Logger,
{
	/// Create a new graduated rebalancer
	pub fn new(
		trusted: Arc<T>, ln_wallet: Arc<LN>, trigger: Arc<R>, event_handler: Arc<E>, logger: Arc<L>,
	) -> Self {
		Self {
			trusted,
			ln_wallet,
			trigger,
			event_handler,
			logger,
			balance_mutex: tokio::sync::Mutex::new(()),
		}
	}

	/// Does any rebalance if it meets the conditions of the tunables
	pub async fn do_rebalance_if_needed(&self) {
		self.do_trusted_rebalance_if_needed().await;

		self.do_onchain_rebalance_if_needed().await;
	}

	/// Does a trusted to lightning rebalance if needed
	pub async fn do_trusted_rebalance_if_needed(&self) {
		let _lock = self.balance_mutex.lock().await;
		if let Some(params) = self.trigger.needs_trusted_rebalance().await {
			self.do_trusted_rebalance_locked(params).await;
		}
	}

	/// Does an on-chain to lightning rebalance if needed
	pub async fn do_onchain_rebalance_if_needed(&self) {
		let _lock = self.balance_mutex.lock().await;
		if let Some(params) = self.trigger.needs_onchain_rebalance().await {
			self.do_onchain_rebalance_locked(params).await;
		}
	}

	/// Perform a rebalance from trusted to lightning wallet
	async fn do_trusted_rebalance_locked(&self, params: TriggerParams) {
		let mut transfer_amt = params.amount;
		log_info!(self.logger, "Initiating rebalance");

		if let Ok(mut inv) = self.ln_wallet.get_bolt11_invoice(Some(transfer_amt)).await {
			if let Ok(fee) = self
				.trusted
				.estimate_fee(PaymentMethod::LightningBolt11(inv.clone()), transfer_amt)
				.await
			{
				if fee >= transfer_amt {
					log_error!(
						self.logger,
						"Rebalance trusted transaction fee {fee:?} exceeds amount {transfer_amt:?}",
					);
					return;
				}

				if transfer_amt.saturating_add(fee) > params.amount {
					transfer_amt = params.amount.saturating_sub(fee);
					match self.ln_wallet.get_bolt11_invoice(Some(transfer_amt)).await {
						Ok(reduced_inv) => inv = reduced_inv,
						Err(e) => {
							log_error!(
								self.logger,
								"Failed to create fee-adjusted rebalance invoice: {e:?}",
							);
							return;
						},
					}
				}
			}

			log_debug!(
				self.logger,
				"Attempting to pay invoice {inv} to rebalance for {transfer_amt:?}",
			);
			let expected_hash = inv.payment_hash();
			let result = self.ln_wallet.watch_rebalance(expected_hash.0);
			match self.trusted.pay(PaymentMethod::LightningBolt11(inv), transfer_amt).await {
				Ok(rebalance_id) => {
					log_debug!(
						self.logger,
						"Rebalance trusted transaction initiated, id {}. Waiting for LN payment.",
						rebalance_id.as_hex()
					);

					self.event_handler
						.handle_event(RebalancerEvent::RebalanceInitiated {
							trigger_id: params.id,
							trusted_rebalance_payment_id: rebalance_id,
							payment_hash: expected_hash.0,
							amount_msat: transfer_amt.milli_sats(),
						})
						.await;

					let Some(receipt) = result.await else {
						log_error!(self.logger, "Failed to complete rebalance payment!");
						return;
					};

					log_info!(
						self.logger,
						"Rebalance succeeded. Sent trusted tx {} to lightning tx {}",
						rebalance_id.as_hex(),
						receipt.lightning.id.as_hex(),
					);

					self.event_handler
						.handle_event(RebalancerEvent::RebalanceSuccessful {
							trigger_id: params.id,
							trusted_rebalance_payment_id: rebalance_id,
							ln_rebalance_payment_id: receipt.lightning.id,
							amount_msat: transfer_amt.milli_sats(),
							fee_msat: receipt.lightning.fee_paid_msat.unwrap_or_default()
								+ receipt.trusted.fee_paid_msat.unwrap_or_default(),
						})
						.await;
				},
				Err(e) => {
					log_info!(self.logger, "Rebalance trusted transaction failed with {e:?}",);
				},
			}
		}
	}

	/// Perform on-chain to lightning rebalance by opening a channel or splicing into an existing one
	async fn do_onchain_rebalance_locked(&self, params: TriggerParams) {
		let (channel_outpoint, user_channel_id) = if self.ln_wallet.has_channel_with_lsp() {
			log_info!(self.logger, "Splicing into channel with LSP with on-chain funds");

			let user_chan_id = match self.ln_wallet.splice_to_lsp_channel().await {
				Ok(chan_id) => chan_id,
				Err(e) => {
					log_error!(self.logger, "Failed to open channel with LSP: {e:?}");
					return;
				},
			};

			log_info!(self.logger, "Initiated splice opened with LSP");

			let channel_outpoint = self.ln_wallet.await_splice_pending(user_chan_id).await;

			log_info!(self.logger, "Splice initiated at: {channel_outpoint}");

			(channel_outpoint, user_chan_id)
		} else {
			log_info!(self.logger, "Opening channel with LSP with on-chain funds");

			let user_chan_id = match self.ln_wallet.open_channel_with_lsp().await {
				Ok(chan_id) => chan_id,
				Err(e) => {
					log_error!(self.logger, "Failed to open channel with LSP: {e:?}");
					return;
				},
			};

			log_info!(self.logger, "Initiated channel opened with LSP");

			let channel_outpoint = self.ln_wallet.await_channel_pending(user_chan_id).await;

			log_info!(self.logger, "Channel open succeeded at: {channel_outpoint}");

			(channel_outpoint, user_chan_id)
		};

		self.event_handler
			.handle_event(RebalancerEvent::OnChainRebalanceInitiated {
				trigger_id: params.id,
				user_channel_id,
				channel_outpoint,
			})
			.await;
	}

	/// Stops the rebalancer, waits for any active rebalances to complete
	pub async fn stop(&self) {
		log_debug!(self.logger, "Waiting for balance mutex...");
		let _ = self.balance_mutex.lock().await;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use lightning::bitcoin::secp256k1::{Secp256k1, SecretKey};
	use lightning::util::logger::Record;
	use lightning_invoice::{Currency, InvoiceBuilder, PaymentHash, PaymentSecret};
	use std::future::ready;
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::time::Duration;
	use tokio::sync::{mpsc, watch};

	const HASH: [u8; 32] = [3; 32];
	const TRUSTED_ID: [u8; 32] = [1; 32];
	const LN_ID: [u8; 32] = [2; 32];
	const TRIGGER_ID: [u8; 32] = [9; 32];

	struct NoopLogger;
	impl Logger for NoopLogger {
		fn log(&self, _record: Record) {}
	}

	fn invoice(amount: Amount) -> Bolt11Invoice {
		let key = SecretKey::from_slice(&[0xcd; 32]).unwrap();
		InvoiceBuilder::new(Currency::Regtest)
			.description("rebalance".into())
			.payment_hash(PaymentHash(HASH))
			.payment_secret(PaymentSecret([0; 32]))
			.duration_since_epoch(Duration::from_secs(1_700_000_000))
			.min_final_cltv_expiry_delta(144)
			.amount_milli_satoshis(amount.milli_sats())
			.build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &key))
			.unwrap()
	}

	/// Trusted leg whose outcome the test decides after `pay` returned.
	struct Trusted {
		outcome: watch::Sender<Option<bool>>,
		outcome_on_pay: Option<bool>,
	}

	impl TrustedWallet for Trusted {
		type Error = String;
		fn get_balance(
			&self,
		) -> Pin<Box<dyn Future<Output = Result<Amount, Self::Error>> + Send + '_>> {
			Box::pin(ready(Ok(Amount::from_sats(1_000_000).unwrap())))
		}
		fn get_bolt11_invoice(
			&self, _amount: Option<Amount>,
		) -> Pin<Box<dyn Future<Output = Result<Bolt11Invoice, Self::Error>> + Send + '_>> {
			Box::pin(ready(Err("unused".into())))
		}
		fn pay(
			&self, _method: PaymentMethod, _amount: Amount,
		) -> Pin<Box<dyn Future<Output = Result<[u8; 32], Self::Error>> + Send + '_>> {
			assert!(self.outcome.receiver_count() > 0, "register the result before sending");
			if let Some(outcome) = self.outcome_on_pay {
				self.outcome.send_replace(Some(outcome));
			}
			Box::pin(ready(Ok(TRUSTED_ID)))
		}
		fn estimate_fee(
			&self, _method: PaymentMethod, _amount: Amount,
		) -> Pin<Box<dyn Future<Output = Result<Amount, Self::Error>> + Send + '_>> {
			Box::pin(ready(Ok(Amount::from_sats(1).unwrap())))
		}
	}

	struct Lightning {
		outcome: watch::Sender<Option<bool>>,
	}

	impl LightningWallet for Lightning {
		type Error = String;
		fn get_balance(&self) -> LightningBalance {
			LightningBalance { lightning: Amount::ZERO, onchain: Amount::ZERO }
		}
		fn get_bolt11_invoice(
			&self, amount: Option<Amount>,
		) -> Pin<Box<dyn Future<Output = Result<Bolt11Invoice, Self::Error>> + Send + '_>> {
			Box::pin(ready(Ok(invoice(amount.unwrap()))))
		}
		fn pay(
			&self, _method: PaymentMethod, _amount: Amount,
		) -> Pin<Box<dyn Future<Output = Result<[u8; 32], Self::Error>> + Send + '_>> {
			Box::pin(ready(Err("unused".into())))
		}
		fn watch_rebalance(&self, payment_hash: [u8; 32]) -> RebalanceWait {
			assert_eq!(payment_hash, HASH);
			let mut outcome = self.outcome.subscribe();
			Box::pin(async move {
				let succeeded = *outcome.wait_for(|o| o.is_some()).await.unwrap();
				succeeded.unwrap().then_some(RebalanceReceipt {
					lightning: ReceivedLightningPayment { id: LN_ID, fee_paid_msat: Some(2) },
					trusted: ReceivedLightningPayment { id: TRUSTED_ID, fee_paid_msat: Some(1) },
				})
			})
		}
		fn has_channel_with_lsp(&self) -> bool {
			false
		}
		fn open_channel_with_lsp(
			&self,
		) -> Pin<Box<dyn Future<Output = Result<u128, Self::Error>> + Send + '_>> {
			Box::pin(ready(Err("unused".into())))
		}
		fn await_channel_pending(
			&self, _channel_id: u128,
		) -> Pin<Box<dyn Future<Output = OutPoint> + Send + '_>> {
			Box::pin(std::future::pending())
		}
		fn splice_to_lsp_channel(
			&self,
		) -> Pin<Box<dyn Future<Output = Result<u128, Self::Error>> + Send + '_>> {
			Box::pin(ready(Err("unused".into())))
		}
		fn await_splice_pending(
			&self, _channel_id: u128,
		) -> Pin<Box<dyn Future<Output = OutPoint> + Send + '_>> {
			Box::pin(std::future::pending())
		}
	}

	struct OneTrustedRebalance {
		triggered: AtomicBool,
	}

	impl RebalanceTrigger for OneTrustedRebalance {
		fn needs_trusted_rebalance(&self) -> impl Future<Output = Option<TriggerParams>> + Send {
			let first = !self.triggered.swap(true, Ordering::SeqCst);
			ready(first.then_some(TriggerParams {
				id: TRIGGER_ID,
				amount: Amount::from_sats(10_000).unwrap(),
			}))
		}
		fn needs_onchain_rebalance(&self) -> impl Future<Output = Option<TriggerParams>> + Send {
			ready(None)
		}
	}

	struct Events(mpsc::UnboundedSender<RebalancerEvent>);

	impl EventHandler for Events {
		fn handle_event(
			&self, event: RebalancerEvent,
		) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
			self.0.send(event).unwrap();
			Box::pin(ready(()))
		}
	}

	type TestRebalancer =
		GraduatedRebalancer<Trusted, Lightning, OneTrustedRebalance, Events, NoopLogger>;

	fn rebalancer(
		trusted_outcome: watch::Sender<Option<bool>>, outcome_on_pay: Option<bool>,
	) -> (Arc<TestRebalancer>, mpsc::UnboundedReceiver<RebalancerEvent>) {
		let (events, received) = mpsc::unbounded_channel();
		let rebalancer = GraduatedRebalancer::new(
			Arc::new(Trusted { outcome: trusted_outcome.clone(), outcome_on_pay }),
			Arc::new(Lightning { outcome: trusted_outcome }),
			Arc::new(OneTrustedRebalance { triggered: AtomicBool::new(false) }),
			Arc::new(Events(events)),
			Arc::new(NoopLogger),
		);
		(Arc::new(rebalancer), received)
	}

	async fn next_event(events: &mut mpsc::UnboundedReceiver<RebalancerEvent>) -> RebalancerEvent {
		tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().unwrap()
	}

	#[tokio::test]
	async fn combined_result_is_registered_before_pay_and_reports_success() {
		for immediate in [false, true] {
			let (outcome, _) = watch::channel(None);
			let (rebalancer, mut events) = rebalancer(outcome.clone(), immediate.then_some(true));
			let rb = Arc::clone(&rebalancer);
			let task = tokio::spawn(async move { rb.do_trusted_rebalance_if_needed().await });
			assert!(matches!(
				next_event(&mut events).await,
				RebalancerEvent::RebalanceInitiated { .. }
			));
			if !immediate {
				assert!(rebalancer.balance_mutex.try_lock().is_err());
				assert!(!task.is_finished());
				outcome.send_replace(Some(true));
			}
			tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
			assert!(rebalancer.balance_mutex.try_lock().is_ok());
			match next_event(&mut events).await {
				RebalancerEvent::RebalanceSuccessful {
					trigger_id,
					trusted_rebalance_payment_id,
					ln_rebalance_payment_id,
					fee_msat,
					..
				} => {
					assert_eq!(trigger_id, TRIGGER_ID);
					assert_eq!(trusted_rebalance_payment_id, TRUSTED_ID);
					assert_eq!(ln_rebalance_payment_id, LN_ID);
					assert_eq!(fee_msat, 3);
				},
				other => panic!("unexpected event {other:?}"),
			}
			assert!(events.try_recv().is_err());
			assert_eq!(outcome.receiver_count(), 0);
		}
	}

	#[tokio::test]
	async fn combined_failure_releases_the_rebalance_lock() {
		for immediate in [false, true] {
			let (outcome, _) = watch::channel(None);
			let (rebalancer, mut events) = rebalancer(outcome.clone(), immediate.then_some(false));
			let rb = Arc::clone(&rebalancer);
			let task = tokio::spawn(async move { rb.do_trusted_rebalance_if_needed().await });
			assert!(matches!(
				next_event(&mut events).await,
				RebalancerEvent::RebalanceInitiated { .. }
			));
			if !immediate {
				outcome.send_replace(Some(false));
			}
			tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
			assert!(rebalancer.balance_mutex.try_lock().is_ok());
			assert!(events.try_recv().is_err());
			assert_eq!(outcome.receiver_count(), 0);
		}
	}
}
