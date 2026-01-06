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

/// Represents the state of an in-progress on-chain rebalance
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnChainRebalanceState {
	/// ID from the rebalance trigger (the triggering on-chain txid)
	pub trigger_id: [u8; 32],
	/// User channel ID assigned by LDK (set after operation completes)
	pub user_channel_id: Option<u128>,
	/// Amount being rebalanced in satoshis
	pub amount_sats: u64,
	/// Whether this is a splice (true) or channel open (false)
	pub is_splice: bool,
	/// Whether the channel/splice pending event has been received
	pub pending_confirmed: bool,
	/// The channel outpoint (set when pending_confirmed is true)
	pub channel_outpoint: Option<OutPoint>,
}

impl_writeable_tlv_based!(OnChainRebalanceState, {
	(0, trigger_id, required),
	(2, user_channel_id, option),
	(4, amount_sats, required),
	(6, is_splice, required),
	(8, pending_confirmed, required),
	(10, channel_outpoint, option),
});

/// Trait for persisting rebalance state across restarts
pub trait RebalancePersistence: Send + Sync {
	/// Insert a new trusted rebalance state
	fn insert_trusted_rebalance_state(
		&self, state: RebalanceState,
	) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

	/// Update an existing trusted rebalance state
	fn update_trusted_rebalance_state(
		&self, state: RebalanceState,
	) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		self.insert_trusted_rebalance_state(state)
	}

	/// Remove a trusted rebalance state
	fn remove_trusted_rebalance_state(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

	/// Gets the current trusted rebalance state
	fn get_trusted_rebalance(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Option<RebalanceState>, ()>> + Send + '_>>;

	/// Insert a new on-chain rebalance state
	fn insert_onchain_rebalance_state(
		&self, state: OnChainRebalanceState,
	) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

	/// Update an existing on-chain rebalance state
	fn update_onchain_rebalance_state(
		&self, state: OnChainRebalanceState,
	) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		self.insert_onchain_rebalance_state(state)
	}

	/// Remove an on-chain rebalance state
	fn remove_onchain_rebalance_state(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

	/// Gets the current on-chain rebalance state
	fn get_onchain_rebalance(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Option<OnChainRebalanceState>, ()>> + Send + '_>>;
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

	/// Splice funds from on-chain to an existing channel with the LSP
	fn splice_to_lsp_channel(
		&self, amt: Amount,
	) -> Pin<Box<dyn Future<Output = Result<u128, Self::Error>> + Send + '_>>;

	/// Get the funding outpoint for a channel by user_channel_id, if it exists
	fn get_channel_outpoint(&self, user_channel_id: u128) -> Option<OutPoint>;
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

	/// In-memory cache of active rebalance (only one trusted rebalance allowed at a time)
	active_trusted_rebalance: Arc<tokio::sync::RwLock<Option<RebalanceState>>>,

	/// In-memory cache of active on-chain rebalance (only one on-chain rebalance allowed at a time)
	active_onchain_rebalance: Arc<tokio::sync::RwLock<Option<OnChainRebalanceState>>>,
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
			active_trusted_rebalance: Arc::new(tokio::sync::RwLock::new(None)),
			active_onchain_rebalance: Arc::new(tokio::sync::RwLock::new(None)),
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
		let mut rebalance = self.active_trusted_rebalance.write().await;
		if rebalance.is_some() {
			log_info!(self.logger, "Skipping rebalance trigger - already have an active rebalance");
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
			self.persistence.insert_trusted_rebalance_state(state).await;

			match self.trusted.pay(PaymentMethod::LightningBolt11(inv), transfer_amt).await {
				Ok(trusted_payment_id) => {
					log_debug!(
						self.logger,
						"Rebalance trusted transaction initiated, id {}. Will complete via observer callbacks.",
						trusted_payment_id.as_hex()
					);

					// persist trusted payment id
					state.trusted_payment_id = Some(trusted_payment_id);
					self.persistence.update_trusted_rebalance_state(state).await;

					// Set active rebalance
					*rebalance = Some(state);

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
					self.persistence.remove_trusted_rebalance_state().await;

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
		let mut onchain_rebalance = self.active_onchain_rebalance.write().await;

		// Check if there's already an active on-chain rebalance
		if onchain_rebalance.is_some() {
			log_info!(
				self.logger,
				"Skipping on-chain rebalance trigger - already have an active on-chain rebalance"
			);
			return;
		}

		let is_splice = self.ln_wallet.has_channel_with_lsp();

		if is_splice {
			log_info!(self.logger, "Splicing into channel with LSP with on-chain funds");
		} else {
			log_info!(self.logger, "Opening channel with LSP with on-chain funds");
		}

		// Persist state BEFORE initiating the operation to ensure we don't lose track
		let mut state = OnChainRebalanceState {
			trigger_id: params.id,
			user_channel_id: None,
			amount_sats: params.amount.sats_rounding_up(),
			is_splice,
			pending_confirmed: false,
			channel_outpoint: None,
		};
		self.persistence.insert_onchain_rebalance_state(state).await;
		*onchain_rebalance = Some(state);

		// Now initiate the actual operation
		let user_channel_id = if is_splice {
			match self.ln_wallet.splice_to_lsp_channel(params.amount).await {
				Ok(chan_id) => chan_id,
				Err(e) => {
					log_error!(self.logger, "Failed to splice to LSP channel: {e:?}");
					// Clean up the state if we failed
					let _ = onchain_rebalance.take();
					self.persistence.remove_onchain_rebalance_state().await;
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
			}
		} else {
			match self.ln_wallet.open_channel_with_lsp(params.amount).await {
				Ok(chan_id) => chan_id,
				Err(e) => {
					log_error!(self.logger, "Failed to open channel with LSP: {e:?}");
					// Clean up the state if we failed
					let _ = onchain_rebalance.take();
					self.persistence.remove_onchain_rebalance_state().await;
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
			}
		};

		// Update state with the user_channel_id
		state.user_channel_id = Some(user_channel_id);
		*onchain_rebalance = Some(state);
		self.persistence.update_onchain_rebalance_state(state).await;

		log_info!(
			self.logger,
			"On-chain rebalance initiated for user_channel_id {user_channel_id}. Will complete via observer callback."
		);
	}

	/// Called when the trusted wallet confirms sending a payment
	/// This is part of the observer pattern to handle rebalance completion across restarts
	pub async fn on_trusted_payment_sent(&self, payment_hash: [u8; 32], fee_msat: Option<u64>) {
		let mut rebalance = self.active_trusted_rebalance.write().await;

		if let Some(state) = rebalance.as_mut() {
			if state.expected_payment_hash == payment_hash {
				state.trusted_payment_sent = true;
				state.trusted_fee_msat = fee_msat;

				log_debug!(
					self.logger,
					"Trusted payment sent for rebalance {}",
					payment_hash.as_hex()
				);

				// Check if rebalance is complete, otherwise persist state
				if state.ln_payment_received {
					self.complete_rebalance(*state, &mut rebalance).await;
				} else {
					self.persistence.update_trusted_rebalance_state(*state).await;
				}
			}
		}
	}

	/// Called when the lightning wallet receives a payment
	/// This is part of the observer pattern to handle rebalance completion across restarts
	pub async fn on_ln_payment_received(
		&self, payment_hash: [u8; 32], payment_id: [u8; 32], fee_msat: Option<u64>,
	) {
		let mut rebalance = self.active_trusted_rebalance.write().await;

		if let Some(state) = rebalance.as_mut() {
			if state.expected_payment_hash == payment_hash {
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
					self.complete_rebalance(*state, &mut rebalance).await;
				} else {
					self.persistence.update_trusted_rebalance_state(*state).await;
				}
			}
		}
	}

	/// Called when a trusted wallet payment fails
	/// This is part of the observer pattern to handle rebalance failures
	pub async fn on_trusted_payment_failed(&self, payment_hash: [u8; 32], reason: String) {
		let mut rebalance = self.active_trusted_rebalance.write().await;

		if let Some(state) = rebalance.as_ref() {
			if state.expected_payment_hash == payment_hash {
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
				let _ = rebalance.take();
				self.persistence.remove_trusted_rebalance_state().await;
			}
		}
	}

	/// Called when a channel or splice becomes pending
	/// This is part of the observer pattern to handle on-chain rebalance completion across restarts
	pub async fn on_channel_splice_pending(
		&self, user_channel_id: u128, channel_outpoint: OutPoint,
	) {
		let mut onchain_rebalance = self.active_onchain_rebalance.write().await;

		// Check if there's an active on-chain rebalance matching this user_channel_id
		if let Some(state) = onchain_rebalance.as_mut() {
			if state.user_channel_id == Some(user_channel_id) {
				state.pending_confirmed = true;
				state.channel_outpoint = Some(channel_outpoint);

				log_info!(
					self.logger,
					"On-chain rebalance pending confirmed for user_channel_id {} at outpoint {}",
					user_channel_id,
					channel_outpoint
				);

				self.complete_onchain_rebalance(*state, &mut onchain_rebalance).await;
			}
		}
	}

	/// Complete an on-chain rebalance by posting the event and cleaning up
	async fn complete_onchain_rebalance(
		&self, state: OnChainRebalanceState,
		onchain_rebalance: &mut tokio::sync::RwLockWriteGuard<'_, Option<OnChainRebalanceState>>,
	) {
		log_info!(
			self.logger,
			"On-chain rebalance completed for user_channel_id {:?} at {}",
			state.user_channel_id,
			state.channel_outpoint.expect("channel_outpoint must be set")
		);

		self.event_handler
			.handle_event(RebalancerEvent::OnChainRebalanceInitiated {
				trigger_id: state.trigger_id,
				user_channel_id: state.user_channel_id.expect("user_channel_id must be set"),
				channel_outpoint: state.channel_outpoint.expect("channel_outpoint must be set"),
			})
			.await;

		// Clean up
		let _ = onchain_rebalance.take();
		self.persistence.remove_onchain_rebalance_state().await;
	}

	/// Complete a rebalance by posting the success event and cleaning up
	async fn complete_rebalance(
		&self, state: RebalanceState,
		rebalance: &mut tokio::sync::RwLockWriteGuard<'_, Option<RebalanceState>>,
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
		let _ = rebalance.take();
		self.persistence.remove_trusted_rebalance_state().await;
	}

	/// Recover incomplete rebalances from persistence on startup
	/// This should be called during wallet initialization
	pub async fn recover_incomplete_rebalances(&self) -> Result<(), ()> {
		let state_opt = self.persistence.get_trusted_rebalance().await?;

		if let Some(state) = state_opt {
			let mut rebalance = self.active_trusted_rebalance.write().await;
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

				// Temporarily set to complete
				*rebalance = Some(state);
				self.complete_rebalance(state, &mut rebalance).await;
			} else {
				// Still waiting for one or both sides
				// Note: We only recover the first incomplete rebalance since we only allow one at a time
				if rebalance.is_none() {
					*rebalance = Some(state);
				} else {
					debug_assert!(
						false,
						"Called recover_incomplete_rebalances with multiple times"
					);
				}
			}
		}

		Ok(())
	}

	/// Recover incomplete on-chain rebalances from persistence on startup
	/// This should be called during wallet initialization
	pub async fn recover_incomplete_onchain_rebalances(&self) -> Result<(), ()> {
		let state_opt = self.persistence.get_onchain_rebalance().await?;

		if let Some(mut state) = state_opt {
			let mut onchain_rebalance = self.active_onchain_rebalance.write().await;
			log_debug!(
				self.logger,
				"Recovering on-chain rebalance for user_channel_id {:?} (pending_confirmed: {})",
				state.user_channel_id,
				state.pending_confirmed
			);

			// Check if the channel is already ready by querying LDK (only if user_channel_id is set)
			if let Some(user_channel_id) = state.user_channel_id {
				if let Some(outpoint) = self.ln_wallet.get_channel_outpoint(user_channel_id) {
					// Channel is already pending/ready, complete now
					log_info!(
						self.logger,
						"On-chain rebalance for user_channel_id {user_channel_id} was already pending, finalizing"
					);

					state.pending_confirmed = true;
					state.channel_outpoint = Some(outpoint);

					*onchain_rebalance = Some(state);
					self.complete_onchain_rebalance(state, &mut onchain_rebalance).await;
				} else {
					// Still waiting for channel/splice pending
					if onchain_rebalance.is_none() {
						*onchain_rebalance = Some(state);
					} else {
						debug_assert!(
							false,
							"Called recover_incomplete_onchain_rebalances multiple times"
						);
					}
				}
			} else {
				// user_channel_id not set yet, still waiting for operation to complete
				if onchain_rebalance.is_none() {
					*onchain_rebalance = Some(state);
				} else {
					debug_assert!(
						false,
						"Called recover_incomplete_onchain_rebalances multiple times"
					);
				}
			}
		}

		Ok(())
	}

	/// Stops the rebalancer, waits for any active rebalances to complete
	pub async fn stop(&self) {
		log_debug!(self.logger, "Waiting for balance mutex...");
		let _ = self.active_trusted_rebalance.write().await;
	}
}
