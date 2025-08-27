#![deny(missing_docs)]
#![allow(clippy::type_complexity)]

//! A library for managing graduated rebalancing between trusted and lightning wallets.
//!
//! This crate provides a `GraduatedRebalancer` that automatically manages the balance
//! between trusted wallets (for small amounts) and lightning wallets (for larger amounts)
//! based on configurable thresholds.

use bitcoin_payment_instructions::amount::Amount;
use bitcoin_payment_instructions::PaymentMethod;
use lightning::bitcoin::hashes::Hash;
use lightning::bitcoin::hex::DisplayHex;
use lightning::bitcoin::OutPoint;
use lightning::util::logger::Logger;
use lightning::{log_debug, log_error, log_info};
use lightning_invoice::Bolt11Invoice;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

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

	/// Get transaction fee paid
	fn get_tx_fee(&self, id: [u8; 32])
		-> Pin<Box<dyn Future<Output = Option<Amount>> + Send + '_>>;
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

	/// Wait for a payment receipt notification
	fn await_payment_receipt(
		&self, payment_hash: [u8; 32],
	) -> Pin<Box<dyn Future<Output = Option<ReceivedLightningPayment>> + Send + '_>>;

	/// Open a channel with the LSP using on-chain funds
	fn open_channel_with_lsp(
		&self, amt: Amount,
	) -> Pin<Box<dyn Future<Output = Result<u128, Self::Error>> + Send + '_>>;

	/// Wait for a channel pending notification, returns the new channel's outpoint
	fn await_channel_pending(
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
	fn handle_event(&self, event: RebalancerEvent);
}

/// A no-op event handler that discards all events
#[derive(Debug, Copy, Clone, Default)]
pub struct IgnoringEventHandler;

impl EventHandler for IgnoringEventHandler {
	fn handle_event(&self, _event: RebalancerEvent) {
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
		if let Some(params) = self.trigger.needs_trusted_rebalance().await {
			self.do_trusted_rebalance(params).await;
		}

		if let Some(params) = self.trigger.needs_onchain_rebalance().await {
			self.do_onchain_rebalance(params).await;
		}
	}

	/// Perform a rebalance from trusted to lightning wallet
	async fn do_trusted_rebalance(&self, params: TriggerParams) {
		let transfer_amt = params.amount;
		let _lock = self.balance_mutex.lock().await;
		log_info!(self.logger, "Initiating rebalance");

		if let Ok(inv) = self.ln_wallet.get_bolt11_invoice(Some(transfer_amt)).await {
			log_debug!(
				self.logger,
				"Attempting to pay invoice {inv} to rebalance for {transfer_amt:?}",
			);
			let expected_hash = *inv.payment_hash();
			match self.trusted.pay(PaymentMethod::LightningBolt11(inv), transfer_amt).await {
				Ok(rebalance_id) => {
					log_debug!(
						self.logger,
						"Rebalance trusted transaction initiated, id {}. Waiting for LN payment.",
						rebalance_id.as_hex()
					);

					self.event_handler.handle_event(RebalancerEvent::RebalanceInitiated {
						trigger_id: params.id,
						trusted_rebalance_payment_id: rebalance_id,
						amount_msat: transfer_amt.milli_sats(),
					});

					// get the fee of the rebalance transaction
					let rebalance_tx_fee =
						self.trusted.get_tx_fee(rebalance_id).await.unwrap_or(Amount::ZERO);

					let ln_payment = match self
						.ln_wallet
						.await_payment_receipt(expected_hash.to_byte_array())
						.await
					{
						Some(receipt) => receipt,
						None => {
							log_error!(self.logger, "Failed to receive rebalance payment!");
							return;
						},
					};

					log_info!(
						self.logger,
						"Rebalance succeeded. Sent trusted tx {} to lightning tx {}",
						rebalance_id.as_hex(),
						ln_payment.id.as_hex(),
					);

					self.event_handler.handle_event(RebalancerEvent::RebalanceSuccessful {
						trigger_id: params.id,
						trusted_rebalance_payment_id: rebalance_id,
						ln_rebalance_payment_id: ln_payment.id,
						amount_msat: transfer_amt.milli_sats(),
						fee_msat: ln_payment.fee_paid_msat.unwrap_or_default()
							+ rebalance_tx_fee.milli_sats(),
					});
				},
				Err(e) => {
					log_info!(self.logger, "Rebalance trusted transaction failed with {e:?}",);
				},
			}
		}
	}

	/// Perform on-chain to lightning rebalance by opening a channel
	async fn do_onchain_rebalance(&self, params: TriggerParams) {
		// This should open a channel with the LSP using available on-chain funds

		let _ = self.balance_mutex.lock().await;

		log_info!(self.logger, "Opening channel with LSP with on-chain funds");

		// todo for now we can only open a channel, eventually move to splicing
		let user_chan_id = match self.ln_wallet.open_channel_with_lsp(params.amount).await {
			Ok(chan_id) => chan_id,
			Err(e) => {
				log_error!(self.logger, "Failed to open channel with LSP: {e:?}");
				return;
			},
		};

		log_info!(self.logger, "Initiated channel opened with LSP");

		let channel_outpoint = self.ln_wallet.await_channel_pending(user_chan_id).await;

		log_info!(self.logger, "Channel open succeeded at: {channel_outpoint}",);

		self.event_handler.handle_event(RebalancerEvent::OnChainRebalanceInitiated {
			trigger_id: params.id,
			user_channel_id: user_chan_id,
			channel_outpoint,
		});
	}

	/// Stops the rebalancer, waits for any active rebalances to complete
	pub async fn stop(&self) {
		log_debug!(self.logger, "Waiting for balance mutex...");
		let _ = self.balance_mutex.lock().await;
	}
}
