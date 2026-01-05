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
use lightning::impl_writeable_tlv_based;
use lightning::util::logger::Logger;
use lightning::{log_debug, log_error, log_info};
use lightning_invoice::Bolt11Invoice;
use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Represents the state of an in-progress rebalance from trusted to lightning wallet
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebalanceState {
	/// ID from the rebalance trigger
	pub trigger_id: [u8; 32],
	/// Expected payment hash for the lightning invoice
	pub expected_payment_hash: [u8; 32],
	/// Amount being rebalanced in millisatoshis
	pub amount_msat: u64,
	/// Whether the lightning wallet has received the payment
	pub ln_payment_received: bool,
	/// Whether the trusted wallet has confirmed sending the payment
	pub trusted_payment_sent: bool,
	/// Payment ID from the trusted wallet
	pub trusted_payment_id: Option<[u8; 32]>,
	/// Lightning payment ID (set when received)
	pub ln_payment_id: Option<[u8; 32]>,
	/// Fee paid by lightning wallet in millisatoshis
	pub ln_fee_msat: Option<u64>,
	/// Fee paid by trusted wallet in millisatoshis
	pub trusted_fee_msat: Option<u64>,
}

impl_writeable_tlv_based!(RebalanceState, {
	(0, trigger_id, required),
	(2, expected_payment_hash, required),
	(4, amount_msat, required),
	(6, ln_payment_received, required),
	(8, trusted_payment_sent, required),
	(10, trusted_payment_id, option),
	(12, ln_payment_id, option),
	(14, ln_fee_msat, option),
	(16, trusted_fee_msat, option),
});

/// Trait for persisting rebalance state across restarts
pub trait RebalancePersistence: Send + Sync {
	/// Insert a new rebalance state
	fn insert_rebalance_state(
		&self, state: RebalanceState,
	) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

	/// Update an existing rebalance state
	fn update_rebalance_state(
		&self, state: RebalanceState,
	) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		self.insert_rebalance_state(state)
	}

	/// Remove a rebalance state
	fn remove_rebalance_state(
		&self, payment_hash: [u8; 32],
	) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

	/// List all incomplete rebalance states
	fn list_incomplete_rebalances(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Vec<RebalanceState>, ()>> + Send + '_>>;
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

	/// Check if we already have a channel with the LSP
	fn has_channel_with_lsp(&self) -> bool;

	/// Open a channel with the LSP using on-chain funds
	fn open_channel_with_lsp(
		&self, amt: Amount,
	) -> Pin<Box<dyn Future<Output = Result<u128, Self::Error>> + Send + '_>>;

	/// Wait for a channel pending notification, returns the new channel's outpoint
	fn await_channel_pending(
		&self, channel_id: u128,
	) -> Pin<Box<dyn Future<Output = OutPoint> + Send + '_>>;

	/// Splice funds from on-chain to an existing channel with the LSP
	fn splice_to_lsp_channel(
		&self, amt: Amount,
	) -> Pin<Box<dyn Future<Output = Result<u128, Self::Error>> + Send + '_>>;

	/// Wait for a splice pending notification, returns the splice outpoint
	fn await_splice_pending(
		&self, channel_id: u128,
	) -> Pin<Box<dyn Future<Output = OutPoint> + Send + '_>>;
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
	/// Rebalance failed
	RebalanceFailed {
		/// Trigger id given by the rebalance trigger
		trigger_id: [u8; 32],
		/// Trusted wallet payment ID for the rebalance
		trusted_rebalance_payment_id: Option<[u8; 32]>,
		/// Amount that was being rebalanced in millisatoshis
		amount_msat: u64,
		/// Reason for failure
		reason: String,
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
	P: RebalancePersistence,
	O: Logger,
> {
	trusted: Arc<T>,
	ln_wallet: Arc<L>,
	trigger: Arc<R>,
	event_handler: Arc<E>,
	persistence: Arc<P>,
	logger: Arc<O>,

	/// In-memory cache of active rebalances indexed by payment hash
	active_rebalances: Arc<tokio::sync::RwLock<HashMap<[u8; 32], RebalanceState>>>,
}

impl<T, LN, R, E, P, L> GraduatedRebalancer<T, LN, R, E, P, L>
where
	T: TrustedWallet,
	LN: LightningWallet,
	R: RebalanceTrigger,
	E: EventHandler,
	P: RebalancePersistence,
	L: Logger,
{
	/// Create a new graduated rebalancer
	pub fn new(
		trusted: Arc<T>, ln_wallet: Arc<LN>, trigger: Arc<R>, event_handler: Arc<E>,
		persistence: Arc<P>, logger: Arc<L>,
	) -> Self {
		Self {
			trusted,
			ln_wallet,
			trigger,
			event_handler,
			persistence,
			logger,
			active_rebalances: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
		}
	}

	/// Does any rebalance if it meets the conditions of the tunables
	pub async fn do_rebalance_if_needed(&self) {
		self.do_trusted_rebalance_if_needed().await;

		self.do_onchain_rebalance_if_needed().await;
	}

	/// Does a trusted to lightning rebalance if needed
	pub async fn do_trusted_rebalance_if_needed(&self) {
		if let Some(params) = self.trigger.needs_trusted_rebalance().await {
			self.do_trusted_rebalance(params).await;
		}
	}

	/// Does an on-chain to lightning rebalance if needed
	pub async fn do_onchain_rebalance_if_needed(&self) {
		if let Some(params) = self.trigger.needs_onchain_rebalance().await {
			self.do_onchain_rebalance(params).await;
		}
	}

	/// Perform a rebalance from trusted to lightning wallet
	/// This method initiates the rebalance and returns immediately.
	/// Completion is handled via the observer pattern (on_trusted_payment_sent, on_ln_payment_received).
	async fn do_trusted_rebalance(&self, params: TriggerParams) {
		// Check if there's already an active rebalance
		let mut rebalances = self.active_rebalances.write().await;
		if !rebalances.is_empty() {
			log_info!(
				self.logger,
				"Skipping rebalance trigger - already have {} active rebalance(s)",
				rebalances.len()
			);
			return;
		}

		log_info!(self.logger, "Initiating rebalance");

		let transfer_amt = params.amount;
		if let Ok(inv) = self.ln_wallet.get_bolt11_invoice(Some(transfer_amt)).await {
			log_debug!(
				self.logger,
				"Attempting to pay invoice {inv} to rebalance for {transfer_amt:?}",
			);

			let expected_hash = inv.payment_hash().to_byte_array();

			// Create and persist rebalance state
			let mut state = RebalanceState {
				trigger_id: params.id,
				expected_payment_hash: expected_hash,
				amount_msat: transfer_amt.milli_sats(),
				ln_payment_received: false,
				trusted_payment_sent: false,
				trusted_payment_id: None,
				ln_payment_id: None,
				ln_fee_msat: None,
				trusted_fee_msat: None,
			};
			self.persistence.insert_rebalance_state(state).await;

			match self.trusted.pay(PaymentMethod::LightningBolt11(inv), transfer_amt).await {
				Ok(trusted_payment_id) => {
					log_debug!(
						self.logger,
						"Rebalance trusted transaction initiated, id {}. Will complete via observer callbacks.",
						trusted_payment_id.as_hex()
					);

					// persist trusted payment id
					state.trusted_payment_id = Some(trusted_payment_id);
					self.persistence.update_rebalance_state(state).await;

					// Add to active rebalances cache
					rebalances.insert(expected_hash, state);

					// Post initiated event
					self.event_handler
						.handle_event(RebalancerEvent::RebalanceInitiated {
							trigger_id: params.id,
							trusted_rebalance_payment_id: trusted_payment_id,
							amount_msat: transfer_amt.milli_sats(),
						})
						.await;
				},
				Err(e) => {
					log_info!(self.logger, "Rebalance trusted transaction failed with {e:?}",);

					// Clean up persisted state
					self.persistence.remove_rebalance_state(expected_hash).await;

					// Post failure event
					self.event_handler
						.handle_event(RebalancerEvent::RebalanceFailed {
							trigger_id: params.id,
							trusted_rebalance_payment_id: None,
							amount_msat: transfer_amt.milli_sats(),
							reason: format!("Failed to initiate trusted payment: {e:?}"),
						})
						.await;
				},
			}
		}
	}

	/// Perform on-chain to lightning rebalance by opening a channel or splicing into an existing one
	async fn do_onchain_rebalance(&self, params: TriggerParams) {
		let _ = self.active_rebalances.write().await; // todo persist onchain rebalances

		let (channel_outpoint, user_channel_id) = if self.ln_wallet.has_channel_with_lsp() {
			log_info!(self.logger, "Splicing into channel with LSP with on-chain funds");

			let user_chan_id = match self.ln_wallet.splice_to_lsp_channel(params.amount).await {
				Ok(chan_id) => chan_id,
				Err(e) => {
					log_error!(self.logger, "Failed to splice to LSP channel: {e:?}");
					self.event_handler
						.handle_event(RebalancerEvent::RebalanceFailed {
							trigger_id: params.id,
							trusted_rebalance_payment_id: None,
							amount_msat: params.amount.milli_sats(),
							reason: format!("Failed to splice to LSP channel: {e:?}"),
						})
						.await;
					return;
				},
			};

			log_info!(self.logger, "Initiated splice opened with LSP");

			let channel_outpoint = self.ln_wallet.await_splice_pending(user_chan_id).await;

			log_info!(self.logger, "Splice initiated at: {channel_outpoint}");

			(channel_outpoint, user_chan_id)
		} else {
			log_info!(self.logger, "Opening channel with LSP with on-chain funds");

			let user_chan_id = match self.ln_wallet.open_channel_with_lsp(params.amount).await {
				Ok(chan_id) => chan_id,
				Err(e) => {
					log_error!(self.logger, "Failed to open channel with LSP: {e:?}");
					self.event_handler
						.handle_event(RebalancerEvent::RebalanceFailed {
							trigger_id: params.id,
							trusted_rebalance_payment_id: None,
							amount_msat: params.amount.milli_sats(),
							reason: format!("Failed to open channel with LSP: {e:?}"),
						})
						.await;
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

	/// Called when the trusted wallet confirms sending a payment
	/// This is part of the observer pattern to handle rebalance completion across restarts
	pub async fn on_trusted_payment_sent(&self, payment_hash: [u8; 32], fee_msat: Option<u64>) {
		let mut rebalances = self.active_rebalances.write().await;

		if let Some(state) = rebalances.get_mut(&payment_hash) {
			state.trusted_payment_sent = true;
			state.trusted_fee_msat = fee_msat;

			log_debug!(self.logger, "Trusted payment sent for rebalance {}", payment_hash.as_hex());

			// Check if rebalance is complete, otherwise persist state
			if state.ln_payment_received {
				self.complete_rebalance(*state, &mut rebalances).await;
			} else {
				self.persistence.update_rebalance_state(*state).await;
			}
		}
	}

	/// Called when the lightning wallet receives a payment
	/// This is part of the observer pattern to handle rebalance completion across restarts
	pub async fn on_ln_payment_received(
		&self, payment_hash: [u8; 32], payment_id: [u8; 32], fee_msat: Option<u64>,
	) {
		let mut rebalances = self.active_rebalances.write().await;

		if let Some(state) = rebalances.get_mut(&payment_hash) {
			state.ln_payment_received = true;
			state.ln_payment_id = Some(payment_id);
			state.ln_fee_msat = fee_msat;

			log_debug!(
				self.logger,
				"Lightning payment received for rebalance {}",
				payment_hash.as_hex()
			);

			// Check if rebalance is complete, otherwise persist state
			if state.trusted_payment_sent {
				self.complete_rebalance(*state, &mut rebalances).await;
			} else {
				self.persistence.update_rebalance_state(*state).await;
			}
		}
	}

	/// Called when a trusted wallet payment fails
	/// This is part of the observer pattern to handle rebalance failures
	pub async fn on_trusted_payment_failed(&self, payment_hash: [u8; 32], reason: String) {
		let mut rebalances = self.active_rebalances.write().await;

		if let Some(state) = rebalances.get(&payment_hash) {
			log_info!(
				self.logger,
				"Trusted payment failed for rebalance {}: {}",
				payment_hash.as_hex(),
				reason
			);

			// Post failure event
			self.event_handler
				.handle_event(RebalancerEvent::RebalanceFailed {
					trigger_id: state.trigger_id,
					trusted_rebalance_payment_id: state.trusted_payment_id,
					amount_msat: state.amount_msat,
					reason,
				})
				.await;

			// Clean up
			rebalances.remove(&payment_hash);
			self.persistence.remove_rebalance_state(payment_hash).await;
		}
	}

	/// Complete a rebalance by posting the success event and cleaning up
	async fn complete_rebalance(
		&self, state: RebalanceState,
		rebalances: &mut tokio::sync::RwLockWriteGuard<'_, HashMap<[u8; 32], RebalanceState>>,
	) {
		log_info!(
			self.logger,
			"Rebalance succeeded. Sent trusted tx {} to lightning tx {}",
			state.trusted_payment_id.expect("trusted_payment_id must be set").as_hex(),
			state.ln_payment_id.expect("ln_payment_id must be set").as_hex(),
		);

		self.event_handler
			.handle_event(RebalancerEvent::RebalanceSuccessful {
				trigger_id: state.trigger_id,
				trusted_rebalance_payment_id: state
					.trusted_payment_id
					.expect("trusted_payment_id must be set"),
				ln_rebalance_payment_id: state.ln_payment_id.expect("ln_payment_id must be set"),
				amount_msat: state.amount_msat,
				fee_msat: state.ln_fee_msat.unwrap_or_default()
					+ state.trusted_fee_msat.unwrap_or_default(),
			})
			.await;

		// Clean up
		rebalances.remove(&state.expected_payment_hash);
		self.persistence.remove_rebalance_state(state.expected_payment_hash).await;
	}

	/// Recover incomplete rebalances from persistence on startup
	/// This should be called during wallet initialization
	pub async fn recover_incomplete_rebalances(&self) -> Result<(), ()> {
		let incomplete = self.persistence.list_incomplete_rebalances().await?;

		if !incomplete.is_empty() {
			log_info!(
				self.logger,
				"Found {} incomplete rebalances, loading into memory",
				incomplete.len()
			);
		}

		let mut rebalances = self.active_rebalances.write().await;
		for state in incomplete {
			log_debug!(
				self.logger,
				"Recovering rebalance {} (ln_received: {}, trusted_sent: {})",
				state.expected_payment_hash.as_hex(),
				state.ln_payment_received,
				state.trusted_payment_sent
			);

			// If both sides already completed, finalize now
			if state.ln_payment_received && state.trusted_payment_sent {
				log_info!(
					self.logger,
					"Rebalance {} was already complete, finalizing",
					state.expected_payment_hash.as_hex()
				);

				// We need to temporarily insert to use complete_rebalance
				rebalances.insert(state.expected_payment_hash, state);
				self.complete_rebalance(state, &mut rebalances).await;
			} else {
				// Still waiting for one or both sides
				rebalances.insert(state.expected_payment_hash, state);
			}
		}

		Ok(())
	}

	/// Stops the rebalancer, waits for any active rebalances to complete
	pub async fn stop(&self) {
		log_debug!(self.logger, "Waiting for balance mutex...");
		let _ = self.active_rebalances.write().await;
	}
}
