use crate::bitcoin::Txid;
use crate::bitcoin::hashes::Hash;
use crate::bitcoin::hex::DisplayHex;
use crate::lightning_wallet::LightningWallet;
use crate::logging::Logger;
use crate::store::{PaymentId, TxMetadata, TxMetadataStore, TxStatus, TxType};
use crate::trusted_wallet::{DynTrustedWalletInterface, Payment};
use crate::{Event, EventQueue, PaymentType, Tunables};
use bitcoin_payment_instructions::amount::Amount;
use graduated_rebalancer::{EventHandler as _, RebalanceTrigger, RebalancerEvent, TriggerParams};
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::{log_error, log_info, log_trace, log_warn};
use ldk_node::payment::{
	ConfirmationStatus, PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus,
};
use std::cmp;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::Notify;

/// Combines pending requests while preserving a follow-up check for changes
/// observed during a running rebalance.
#[derive(Default)]
pub(crate) struct RebalanceScheduler {
	requested: Notify,
}

impl RebalanceScheduler {
	pub fn request(&self) {
		self.requested.notify_one();
	}

	pub async fn run<F, Fut>(&self, mut check: F)
	where
		F: FnMut() -> Fut,
		Fut: Future<Output = ()>,
	{
		loop {
			self.requested.notified().await;
			check().await;
		}
	}
}

pub(crate) struct OrangeTrigger {
	/// The main implementation of the wallet, containing both trusted and lightning wallet components.
	ln_wallet: Arc<LightningWallet>,
	/// The trusted wallet interface for managing small balances.
	trusted: Arc<Box<DynTrustedWalletInterface>>,
	/// Configuration parameters for when the wallet decides to use the lightning or trusted wallet.
	tunables: Tunables,
	/// Metadata store for tracking transactions.
	tx_metadata: TxMetadataStore,
	/// The event handler for processing wallet events.
	event_queue: Arc<EventQueue>,
	/// Time of the last on-chain sync, used to determine when to trigger rebalances.
	onchain_sync_time: AtomicU64,
	/// Total and spendable on-chain balances at the last payment history scan. An inbound
	/// transaction moves the total when it arrives and the spendable amount when it confirms,
	/// so a sync that changed neither has no new on-chain payments to scan for.
	scanned_onchain_balances: Mutex<Option<(u64, u64)>>,
	/// Logger for logging events and errors.
	logger: Arc<Logger>,
}

impl OrangeTrigger {
	/// Creates a new `OrangeTrigger` instance.
	pub(crate) fn new(
		ln_wallet: Arc<LightningWallet>, trusted: Arc<Box<DynTrustedWalletInterface>>,
		tunables: Tunables, tx_metadata: TxMetadataStore, event_queue: Arc<EventQueue>,
		logger: Arc<Logger>,
	) -> Self {
		let start =
			ln_wallet.inner.ldk_node.status().latest_onchain_wallet_sync_timestamp.unwrap_or(0);
		Self {
			ln_wallet,
			trusted,
			tunables,
			tx_metadata,
			event_queue,
			onchain_sync_time: AtomicU64::new(start),
			scanned_onchain_balances: Mutex::new(None),
			logger,
		}
	}
}

impl RebalanceTrigger for OrangeTrigger {
	fn needs_trusted_rebalance(&self) -> impl Future<Output = Option<TriggerParams>> + Send {
		async move {
			let rebalance_enabled = self.event_queue.get_rebalance_enabled().await;
			if !rebalance_enabled {
				return None;
			}

			// we need to add metadata for any potential payments that will cause a rebalance
			// to happen, so we can track them.
			if let Ok(trusted_payments) = self.trusted.list_payments().await {
				let mut new_txn = Vec::new();
				let mut latest_tx: Option<(Duration, _)> = None;
				for payment in trusted_payments.iter() {
					if payment.outbound {
						// Assume it'll be tracked by the sending task.
						// TODO: Maybe use this to backfill stuff we lost on crash?
						continue;
					}
					let payment_id = PaymentId::Trusted(payment.id);
					let have_metadata = if let Some(metadata) =
						self.tx_metadata.read().get(&payment_id)
					{
						if let TxType::Payment { .. } = &metadata.ty {
							if latest_tx.is_none() || latest_tx.as_ref().unwrap().0 < metadata.time
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
							self.logger,
							"Received new trusted payment with id {}",
							payment.id.as_hex()
						);
						new_txn.push((payment.amount, &payment.id));
						self.tx_metadata
							.insert(
								payment_id,
								TxMetadata {
									ty: TxType::Payment { ty: PaymentType::IncomingLightning {} },
									// Backend settle time, not detection time, so a receive that
									// settled while offline keeps its real time. Preserved through
									// promotion for the graduated-transfer display.
									time: payment.time_since_epoch,
								},
							)
							.await;
					}
				}

				// We always assume lighting balance is an overestimate by `rebalance_min`.
				let lightning_receivable = self
					.ln_wallet
					.estimate_receivable_balance()
					.saturating_sub(self.tunables.rebalance_min);
				let trusted_bal = self.trusted.get_balance().await.ok()?;
				let mut transfer_amt = cmp::min(lightning_receivable, trusted_bal);
				if trusted_bal.saturating_sub(transfer_amt) > self.tunables.trusted_balance_limit {
					// We need to just get a new channel, there's too much that we need to get to lightning
					transfer_amt = trusted_bal;
				}
				if transfer_amt > self.tunables.rebalance_min {
					new_txn.sort_unstable();
					let victim_id = new_txn.first().map(|(_, id)| *id).or_else(|| {
						// Should only happen due to races settling balance, pick the latest.
						latest_tx.map(|l| l.1)
					});

					victim_id.map(|id| TriggerParams { amount: transfer_amt, id: *id })
				} else {
					None
				}
			} else {
				None
			}
		}
	}

	fn needs_onchain_rebalance(&self) -> impl Future<Output = Option<TriggerParams>> + Send {
		async move {
			let rebalance_enabled = self.event_queue.get_rebalance_enabled().await;
			if !rebalance_enabled {
				return None;
			}

			// detect if onchain was synced, if so, check if we need to rebalance
			let new_onchain_sync_time =
				self.ln_wallet.inner.ldk_node.status().latest_onchain_wallet_sync_timestamp;
			let onchain_sync_time = self.onchain_sync_time.load(Ordering::Relaxed);
			if let Some(new_onchain_sync_time) = new_onchain_sync_time
				&& onchain_sync_time != new_onchain_sync_time
			{
				let balances = self.ln_wallet.inner.ldk_node.list_balances();
				let onchain_balances =
					(balances.total_onchain_balance_sats, balances.spendable_onchain_balance_sats);
				// Reading payment history can hit a remote store, so skip it on idle syncs.
				if *self.scanned_onchain_balances.lock().unwrap() == Some(onchain_balances) {
					return None;
				}
				// find all new confirmed inbound onchain payments since last sync
				let payments = match self.ln_wallet.list_payments() {
					Ok(payments) => payments,
					Err(e) => {
						log_error!(self.logger, "Failed to list onchain payments: {e}");
						return None;
					},
				};
				let new_recvs = payments.iter().filter(|p| {
					p.direction == PaymentDirection::Inbound
						&& p.status == PaymentStatus::Succeeded
						&& p.latest_update_timestamp > onchain_sync_time
						&& matches!(
							p.kind,
							PaymentKind::Onchain {
								status: ConfirmationStatus::Confirmed { .. },
								..
							}
						)
				});

				// now create events for these payments
				for payment in new_recvs {
					let payment_id = PaymentId::SelfCustodial(payment.id.0);
					let (txid, status) = match payment.kind {
						PaymentKind::Onchain { txid, status, .. } => (txid, status),
						_ => continue,
					};
					let event = Event::OnchainPaymentReceived {
						payment_id,
						txid,
						amount_sat: payment.amount_msat.expect("must have amount") / 1_000,
						status,
					};

					log_trace!(self.logger, "Generated OnchainPaymentReceived event: {event:?}");
					if let Err(e) = self.event_queue.add_event(event).await {
						log_error!(
							self.logger,
							"Failed to add OnchainPaymentReceived event: {e:?}"
						);
						return None;
					}
				}

				self.onchain_sync_time.store(new_onchain_sync_time, Ordering::Relaxed);
				*self.scanned_onchain_balances.lock().unwrap() = Some(onchain_balances);

				// check if we have funds that aren't anchor reserve && greater than rebalance_min
				let spendable = balances.spendable_onchain_balance_sats;

				if spendable > self.tunables.rebalance_min.sats_rounding_up() {
					// find the new onchain receives since last sync
					// if we have multiple, select the largest one as the one to mark as triggering the rebalance
					let new = payments
						.into_iter()
						.filter_map(|t| {
							if t.status != PaymentStatus::Succeeded
								|| t.direction != PaymentDirection::Inbound
								|| t.latest_update_timestamp <= onchain_sync_time
							{
								return None;
							}
							let PaymentKind::Onchain { txid, .. } = t.kind else {
								return None;
							};
							let trigger = PaymentId::SelfCustodial(txid.to_byte_array());
							// Only payments can be promoted into rebalance triggers. If this
							// metadata was already promoted by a previous rebalance, selecting it
							// again would make the event handler reject the duplicate promotion.
							let can_mark_as_trigger =
								self.tx_metadata.read().get(&trigger).is_none_or(|metadata| {
									matches!(metadata.ty, TxType::Payment { .. })
								});
							if can_mark_as_trigger { Some((t, txid, trigger)) } else { None }
						})
						.max_by_key(|(t, _, _)| t.amount_msat);
					match new {
						Some((payment, txid, trigger)) => {
							// make sure we have a metadata entry for the triggering transaction
							if self.tx_metadata.read().get(&trigger).is_none() {
								self.tx_metadata
									.insert(
										trigger,
										TxMetadata {
											ty: TxType::Payment {
												ty: PaymentType::IncomingOnChain {
													txid: Some(txid),
												},
											},
											// ldk-node's confirmation time, not detection time, so a
											// receive that confirmed while offline keeps its real
											// time. Preserved through promotion for display.
											time: Duration::from_secs(
												payment.latest_update_timestamp,
											),
										},
									)
									.await;
							}

							Some(TriggerParams {
								amount: Amount::from_sats(spendable).expect("valid amount"),
								id: txid.to_byte_array(),
							})
						},
						None => {
							log_warn!(
								self.logger,
								"Detected onchain sync with balance updates, but no new onchain payments found"
							);
							None
						},
					}
				} else {
					None
				}
			} else {
				// no new onchain sync, so no need to rebalance
				None
			}
		}
	}
}

pub(crate) struct OrangeRebalanceEventHandler {
	/// Metadata store for tracking transactions.
	tx_metadata: TxMetadataStore,
	/// The event handler for processing wallet events.
	event_queue: Arc<EventQueue>,
	/// Logger for logging events and errors.
	logger: Arc<Logger>,
}

impl OrangeRebalanceEventHandler {
	/// Creates a new `OrangeRebalanceEventHandler` instance.
	pub(crate) fn new(
		tx_metadata: TxMetadataStore, event_queue: Arc<EventQueue>, logger: Arc<Logger>,
	) -> Self {
		Self { tx_metadata, event_queue, logger }
	}

	/// Finishes trusted rebalances whose Lightning receipt arrived but was never matched,
	/// for example after a crash or shutdown while a slow trusted payment was settling.
	/// Without this, the trusted send stays hidden and the receipt shows as ordinary income.
	pub(crate) async fn reconcile_pending_rebalances(
		&self, trusted: &DynTrustedWalletInterface, ln_wallet: &LightningWallet,
	) {
		let pending: Vec<_> = {
			let metadata = self.tx_metadata.read();
			metadata
				.iter()
				.filter_map(|(id, entry)| match (id, entry.ty) {
					(
						PaymentId::Trusted(trusted_id),
						TxType::PendingRebalance {
							payment_hash: Some(payment_hash),
							trigger: Some(trigger),
							amount_msat: Some(amount_msat),
						},
					) => {
						// `RebalanceSuccessful` promotes the trigger, which must be a payment.
						let trigger_is_payment = matches!(
							metadata.get(&PaymentId::Trusted(trigger)).map(|t| t.ty),
							Some(
								TxType::Payment { .. }
									| TxType::PaymentTriggeringTransferLightning { .. }
							)
						);
						trigger_is_payment.then_some(PendingTrustedRebalance {
							trusted_id: *trusted_id,
							payment_hash,
							trigger,
							amount_msat,
						})
					},
					_ => None,
				})
				.collect()
		};
		if pending.is_empty() {
			return;
		}

		let trusted_payments = match trusted.list_payments().await {
			Ok(payments) => payments,
			Err(e) => {
				log_error!(self.logger, "Failed to list trusted payments for reconciliation: {e}");
				return;
			},
		};
		let lightning_payments = match ln_wallet.list_payments() {
			Ok(payments) => payments,
			Err(e) => {
				log_error!(self.logger, "Failed to list LN payments for reconciliation: {e}");
				return;
			},
		};

		for (trusted_id, event) in
			completed_pending_rebalances(&pending, &trusted_payments, &lightning_payments)
		{
			log_info!(
				self.logger,
				"Finishing rebalance {} that completed before the last shutdown",
				trusted_id.as_hex()
			);
			self.handle_event(event).await;
		}
	}
}

/// A trusted rebalance whose receipt has not been matched, with what is needed to finish it.
struct PendingTrustedRebalance {
	trusted_id: [u8; 32],
	payment_hash: [u8; 32],
	trigger: [u8; 32],
	amount_msat: u64,
}

/// Pending rebalances whose trusted leg completed and whose Lightning receipt is recorded,
/// as the trusted payment ID and the `RebalanceSuccessful` event that finishes each one.
fn completed_pending_rebalances(
	pending: &[PendingTrustedRebalance], trusted: &[Payment], lightning: &[PaymentDetails],
) -> Vec<([u8; 32], RebalancerEvent)> {
	pending
		.iter()
		.filter_map(|rebalance| {
			let trusted_payment = trusted
				.iter()
				.find(|payment| payment.outbound && payment.id == rebalance.trusted_id)?;
			if trusted_payment.status != TxStatus::Completed {
				return None;
			}
			let (ln_id, lsp_fee_msat) =
				lightning.iter().find_map(|payment| match payment.kind {
					PaymentKind::Bolt11 { hash, counterparty_skimmed_fee_msat, .. }
						if hash.0 == rebalance.payment_hash
							&& payment.direction == PaymentDirection::Inbound
							&& payment.status == PaymentStatus::Succeeded =>
					{
						Some((payment.id.0, counterparty_skimmed_fee_msat))
					},
					_ => None,
				})?;
			let event = RebalancerEvent::RebalanceSuccessful {
				trigger_id: rebalance.trigger,
				trusted_rebalance_payment_id: rebalance.trusted_id,
				ln_rebalance_payment_id: ln_id,
				amount_msat: rebalance.amount_msat,
				fee_msat: lsp_fee_msat.unwrap_or_default() + trusted_payment.fee.milli_sats(),
			};
			Some((rebalance.trusted_id, event))
		})
		.collect()
}

impl graduated_rebalancer::EventHandler for OrangeRebalanceEventHandler {
	fn handle_event(
		&self, event: RebalancerEvent,
	) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		Box::pin(async move {
			match event {
				RebalancerEvent::RebalanceInitiated {
					trigger_id,
					trusted_rebalance_payment_id,
					payment_hash,
					amount_msat,
				} => {
					let metadata = TxMetadata {
						ty: TxType::PendingRebalance {
							payment_hash: Some(payment_hash),
							trigger: Some(trigger_id),
							amount_msat: Some(amount_msat),
						},
						time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap(),
					};
					self.tx_metadata
						.insert(PaymentId::Trusted(trusted_rebalance_payment_id), metadata)
						.await;
					if let Err(e) = self
						.event_queue
						.add_event(Event::RebalanceInitiated {
							trigger_payment_id: PaymentId::Trusted(trigger_id),
							trusted_rebalance_payment_id,
							amount_msat,
						})
						.await
					{
						log_error!(self.logger, "Failed to add RebalanceSuccessful event: {e:?}");
					}
				},
				RebalancerEvent::RebalanceSuccessful {
					trigger_id,
					trusted_rebalance_payment_id: rebalance_id,
					ln_rebalance_payment_id: lightning_id,
					amount_msat,
					fee_msat,
				} => {
					let triggering_transaction_id = PaymentId::Trusted(trigger_id);
					self.tx_metadata
						.set_tx_caused_rebalance(&triggering_transaction_id)
						.await
						.expect("Failed to write metadata for rebalance transaction");
					let metadata = TxMetadata {
						ty: TxType::TrustedToLightning {
							trusted_payment: rebalance_id,
							lightning_payment: lightning_id,
							payment_triggering_transfer: triggering_transaction_id,
						},
						time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap(),
					};
					self.tx_metadata.upsert(PaymentId::Trusted(rebalance_id), metadata).await;
					self.tx_metadata.insert(PaymentId::SelfCustodial(lightning_id), metadata).await;

					let event_queue = Arc::clone(&self.event_queue);
					let logger = Arc::clone(&self.logger);
					tokio::spawn(async move {
						if let Err(e) = event_queue
							.add_event(Event::RebalanceSuccessful {
								trigger_payment_id: triggering_transaction_id,
								trusted_rebalance_payment_id: rebalance_id,
								ln_rebalance_payment_id: lightning_id,
								amount_msat,
								fee_msat,
							})
							.await
						{
							log_error!(logger, "Failed to add RebalanceSuccessful event: {e:?}");
						}
					});
				},
				RebalancerEvent::OnChainRebalanceInitiated {
					trigger_id,
					channel_outpoint,
					user_channel_id: _,
				} => {
					let chan_txid = channel_outpoint.txid;
					let triggering_txid = Txid::from_byte_array(trigger_id);
					let trigger_id = PaymentId::SelfCustodial(triggering_txid.to_byte_array());
					let metadata = TxMetadata {
						ty: TxType::OnchainToLightning { channel_txid: chan_txid, triggering_txid },
						time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap(),
					};
					self.tx_metadata
						.set_tx_caused_rebalance_with_splice(
							&trigger_id,
							PaymentId::SelfCustodial(chan_txid.to_byte_array()),
							metadata,
						)
						.await
						.expect("Failed to write metadata for onchain rebalance transaction");
				},
			}
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use ldk_node::lightning::ln::channelmanager::PaymentId as LdkPaymentId;
	use ldk_node::lightning_types::payment::PaymentHash;
	use tokio::sync::{Semaphore, mpsc};

	const TRUSTED_ID: [u8; 32] = [1; 32];
	const HASH: [u8; 32] = [2; 32];
	const TRIGGER: [u8; 32] = [3; 32];
	const LN_ID: [u8; 32] = [4; 32];

	fn pending() -> PendingTrustedRebalance {
		PendingTrustedRebalance {
			trusted_id: TRUSTED_ID,
			payment_hash: HASH,
			trigger: TRIGGER,
			amount_msat: 50_000_000,
		}
	}

	fn trusted(status: TxStatus) -> Payment {
		Payment {
			id: TRUSTED_ID,
			amount: Amount::from_milli_sats(50_000_000).unwrap(),
			fee: Amount::from_milli_sats(1_000).unwrap(),
			status,
			outbound: true,
			time_since_epoch: Duration::from_secs(1),
		}
	}

	fn receipt(hash: [u8; 32], status: PaymentStatus) -> PaymentDetails {
		PaymentDetails {
			id: LdkPaymentId(LN_ID),
			kind: PaymentKind::Bolt11 {
				hash: PaymentHash(hash),
				preimage: None,
				secret: None,
				counterparty_skimmed_fee_msat: Some(2_000),
			},
			amount_msat: Some(50_000_000),
			fee_paid_msat: None,
			direction: PaymentDirection::Inbound,
			status,
			latest_update_timestamp: 1,
		}
	}

	#[test]
	fn reconciliation_finishes_only_settled_and_received_rebalances() {
		let completed = trusted(TxStatus::Completed);
		let received = receipt(HASH, PaymentStatus::Succeeded);

		let events =
			completed_pending_rebalances(&[pending()], &[completed.clone()], &[received.clone()]);
		assert_eq!(events.len(), 1);
		assert_eq!(events[0].0, TRUSTED_ID);
		match &events[0].1 {
			RebalancerEvent::RebalanceSuccessful {
				trigger_id,
				trusted_rebalance_payment_id,
				ln_rebalance_payment_id,
				amount_msat,
				fee_msat,
			} => {
				assert_eq!(*trigger_id, TRIGGER);
				assert_eq!(*trusted_rebalance_payment_id, TRUSTED_ID);
				assert_eq!(*ln_rebalance_payment_id, LN_ID);
				assert_eq!(*amount_msat, 50_000_000);
				assert_eq!(*fee_msat, 3_000);
			},
			other => panic!("unexpected event {other:?}"),
		}

		// Anything unsettled or unmatched is left alone for a later run.
		let unsettled = [
			(trusted(TxStatus::Pending), received.clone()),
			(trusted(TxStatus::Failed), received.clone()),
			(completed.clone(), receipt(HASH, PaymentStatus::Pending)),
			(completed.clone(), receipt([9; 32], PaymentStatus::Succeeded)),
		];
		for (trusted_payment, ln_payment) in unsettled {
			assert!(
				completed_pending_rebalances(&[pending()], &[trusted_payment], &[ln_payment])
					.is_empty()
			);
		}
		assert!(completed_pending_rebalances(&[pending()], &[], &[received]).is_empty());
	}

	#[tokio::test]
	async fn requests_are_combined_without_losing_changes_during_a_check() {
		let scheduler = Arc::new(RebalanceScheduler::default());
		for _ in 0..8 {
			scheduler.request();
		}
		let (started, mut checks) = mpsc::unbounded_channel();
		let release = Arc::new(Semaphore::new(0));
		let worker_scheduler = Arc::clone(&scheduler);
		let worker_release = Arc::clone(&release);
		let worker = tokio::spawn(async move {
			worker_scheduler
				.run(|| {
					let started = started.clone();
					let release = Arc::clone(&worker_release);
					async move {
						started.send(()).unwrap();
						release.acquire().await.unwrap().forget();
					}
				})
				.await;
		});
		tokio::time::timeout(Duration::from_secs(1), checks.recv()).await.unwrap().unwrap();
		for _ in 0..8 {
			scheduler.request();
		}
		release.add_permits(1);
		tokio::time::timeout(Duration::from_secs(1), checks.recv()).await.unwrap().unwrap();
		release.add_permits(1);
		assert!(tokio::time::timeout(Duration::from_millis(30), checks.recv()).await.is_err());
		// A fresh request after the worker becomes idle must still wake it.
		scheduler.request();
		tokio::time::timeout(Duration::from_secs(1), checks.recv()).await.unwrap().unwrap();
		worker.abort();
		assert!(worker.await.unwrap_err().is_cancelled());
	}
}
