use crate::bitcoin::Txid;
use crate::bitcoin::hashes::Hash;
use crate::bitcoin::hex::DisplayHex;
use crate::lightning_wallet::LightningWallet;
use crate::logging::Logger;
use crate::store::{PaymentId, TxMetadata, TxMetadataStore, TxType};
use crate::trusted_wallet::DynTrustedWalletInterface;
use crate::{Event, EventQueue, PaymentType, Tunables, store};
use bitcoin_payment_instructions::amount::Amount;
use graduated_rebalancer::{RebalanceTrigger, RebalancerEvent, TriggerParams};
use ldk_node::DynStore;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::{log_error, log_info, log_trace, log_warn};
use ldk_node::payment::{ConfirmationStatus, PaymentDirection, PaymentKind, PaymentStatus};
use std::cmp;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

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
	/// Key-value store for persistent storage.
	store: Arc<DynStore>,
	/// Time of the last on-chain sync, used to determine when to trigger rebalances.
	onchain_sync_time: AtomicU64,
	/// Logger for logging events and errors.
	logger: Arc<Logger>,
}

impl OrangeTrigger {
	/// Creates a new `OrangeTrigger` instance.
	pub(crate) fn new(
		ln_wallet: Arc<LightningWallet>, trusted: Arc<Box<DynTrustedWalletInterface>>,
		tunables: Tunables, tx_metadata: TxMetadataStore, event_queue: Arc<EventQueue>,
		store: Arc<DynStore>, logger: Arc<Logger>,
	) -> Self {
		let start =
			ln_wallet.inner.ldk_node.status().latest_onchain_wallet_sync_timestamp.unwrap_or(0);
		Self {
			ln_wallet,
			trusted,
			tunables,
			tx_metadata,
			event_queue,
			store,
			onchain_sync_time: AtomicU64::new(start),
			logger,
		}
	}
}

impl RebalanceTrigger for OrangeTrigger {
	fn needs_trusted_rebalance(&self) -> impl Future<Output = Option<TriggerParams>> + Send {
		async move {
			let rebalance_enabled = store::get_rebalance_enabled(self.store.as_ref());
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
									time: SystemTime::now()
										.duration_since(SystemTime::UNIX_EPOCH)
										.unwrap(),
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
			let rebalance_enabled = store::get_rebalance_enabled(self.store.as_ref());
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
				// find all new confirmed inbound onchain payments since last sync
				let new_recvs = self.ln_wallet.inner.ldk_node.list_payments_with_filter(|p| {
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

				self.onchain_sync_time.swap(new_onchain_sync_time, Ordering::Relaxed);

				// now create events for these payments
				for payment in new_recvs {
					let payment_id = PaymentId::SelfCustodial(payment.id.0);
					let (txid, status) = match payment.kind {
						PaymentKind::Onchain { txid, status } => (txid, status),
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
					}
				}

				// check if we have funds that aren't anchor reserve && greater than rebalance_min
				let spendable =
					self.ln_wallet.inner.ldk_node.list_balances().spendable_onchain_balance_sats;

				if spendable > self.tunables.rebalance_min.sats_rounding_up() {
					// find the new onchain receives since last sync
					// if we have multiple, select the largest one as the one to mark as triggering the rebalance
					let txs = self.ln_wallet.list_payments();
					let new = txs
						.into_iter()
						.filter(|t| {
							t.status == PaymentStatus::Succeeded
								&& t.direction == PaymentDirection::Inbound
								&& matches!(t.kind, PaymentKind::Onchain { .. })
								&& t.latest_update_timestamp >= onchain_sync_time
						})
						.max_by_key(|t| t.amount_msat);
					match new {
						Some(new) => {
							if let PaymentKind::Onchain { txid, .. } = new.kind {
								// make sure we have a metadata entry for the triggering transaction
								let trigger = PaymentId::SelfCustodial(txid.to_byte_array());
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
												time: SystemTime::now()
													.duration_since(SystemTime::UNIX_EPOCH)
													.unwrap(),
											},
										)
										.await;
								}

								Some(TriggerParams {
									amount: Amount::from_sats(spendable).expect("valid amount"),
									id: txid.to_byte_array(),
								})
							} else {
								debug_assert!(
									false,
									"PaymentKind::Onchain should always be present for onchain payments"
								);
								None
							}
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
					amount_msat,
				} => {
					let metadata = TxMetadata {
						ty: TxType::PendingRebalance {},
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
					self.tx_metadata
						.set_tx_caused_rebalance(&trigger_id)
						.await
						.expect("Failed to write metadata for onchain rebalance transaction");
					let metadata = TxMetadata {
						ty: TxType::OnchainToLightning { channel_txid: chan_txid, triggering_txid },
						time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap(),
					};
					self.tx_metadata
						.insert(PaymentId::SelfCustodial(chan_txid.to_byte_array()), metadata)
						.await;
				},
			}
		})
	}
}
