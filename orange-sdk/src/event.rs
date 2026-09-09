use crate::dyn_store::DynStore;
use crate::lightning_wallet::{PaymentReceiptInbox, SplicePendingInbox};
use crate::logging::Logger;
use crate::runtime::Runtime;
use crate::store::{self, MppOutcome, PaymentId, RebalanceEnabledCache, TxMetadataStore, TxType};
use graduated_rebalancer::ReceivedLightningPayment;
use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::{OutPoint, Txid};
use ldk_node::lightning::events::{ClosureReason, PaymentFailureReason};
use ldk_node::lightning::io;
use ldk_node::lightning::ln::msgs::DecodeError;
use ldk_node::lightning::ln::types::ChannelId;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::util::ser::{Readable, Writeable};
use ldk_node::lightning::{impl_writeable_tlv_based_enum, log_debug, log_error, log_warn};
use ldk_node::lightning_types::payment::{PaymentHash, PaymentPreimage};
use ldk_node::payment::{ConfirmationStatus, PaymentKind};
use ldk_node::{CustomTlvRecord, UserChannelId};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;
use tokio::sync::{Mutex, OwnedMutexGuard, watch};

/// The event queue will be persisted under this key.
pub(crate) const EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE: &str = "";
pub(crate) const EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";
pub(crate) const EVENT_QUEUE_PERSISTENCE_KEY: &str = "orange_events";

/// An event emitted by [`Wallet`], which should be handled by the user.
///
/// Delivery across crashes is at least once. Handle payment events idempotently
/// using their variant and payment ID, including after an application acknowledgement.
///
/// [`Wallet`]: [`crate::Wallet`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
	/// An outgoing payment was successful.
	PaymentSuccessful {
		/// A local identifier used to track the payment.
		payment_id: PaymentId,
		/// The hash of the payment.
		payment_hash: PaymentHash,
		/// The preimage to the `payment_hash`.
		///
		/// Note that this serves as a payment receipt.
		payment_preimage: PaymentPreimage,
		/// The total fee which was spent at intermediate hops in this payment.
		fee_paid_msat: Option<u64>,
	},
	/// An outgoing payment has failed.
	PaymentFailed {
		/// A local identifier used to track the payment.
		payment_id: PaymentId,
		/// The hash of the payment.
		///
		/// This will be `None` if the payment failed before receiving an invoice when paying a
		/// BOLT12 [`Offer`].
		///
		/// [`Offer`]: ldk_node::lightning::offers::offer::Offer
		payment_hash: Option<PaymentHash>,
		/// The reason why the payment failed.
		///
		/// Will be `None` if the failure reason is not known.
		reason: Option<PaymentFailureReason>,
	},
	/// A payment has been received.
	PaymentReceived {
		/// A local identifier used to track the payment.
		payment_id: PaymentId,
		/// The hash of the payment.
		payment_hash: PaymentHash,
		/// The value, in msats, that has been received.
		amount_msat: u64,
		/// Custom TLV records received on the payment
		custom_records: Vec<CustomTlvRecord>,
		/// The value, in msats, that was skimmed off of this payment as an extra fee taken by LSP.
		/// Typically, this is only present for payments that result in opening a channel.
		lsp_fee_msats: Option<u64>,
	},
	/// A payment has been received.
	OnchainPaymentReceived {
		/// A local identifier used to track the payment.
		payment_id: PaymentId,
		/// The transaction ID.
		txid: Txid,
		/// The value, in sats, that has been received.
		amount_sat: u64,
		/// The confirmation status of this payment.
		status: ConfirmationStatus,
	},
	/// A channel is ready to be used.
	ChannelOpened {
		/// The `channel_id` of the channel.
		channel_id: ChannelId,
		/// The `user_channel_id` of the channel.
		user_channel_id: UserChannelId,
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The outpoint of the channel's funding transaction.
		funding_txo: OutPoint,
	},
	/// A channel has been closed.
	///
	/// When a channel is closed, we will disable automatic rebalancing
	/// so new channels will not be opened until it is explicitly enabled again.
	ChannelClosed {
		/// The `channel_id` of the channel.
		channel_id: ChannelId,
		/// The `user_channel_id` of the channel.
		user_channel_id: UserChannelId,
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// Why the channel was closed.
		///
		/// Will be `None` if the closure reason is not known.
		reason: Option<ClosureReason>,
	},
	/// A rebalance from our trusted wallet has been initiated.
	RebalanceInitiated {
		/// The `payment_id` of the transaction that triggered the rebalance.
		trigger_payment_id: PaymentId,
		/// The `payment_id` of the rebalance payment sent from the trusted wallet.
		trusted_rebalance_payment_id: [u8; 32],
		/// The amount, in msats, of the rebalance payment.
		amount_msat: u64,
	},
	/// A rebalance from our trusted wallet was successful.
	RebalanceSuccessful {
		/// The `payment_id` of the transaction that triggered the rebalance.
		trigger_payment_id: PaymentId,
		/// The `payment_id` of the rebalance payment sent from the trusted wallet.
		trusted_rebalance_payment_id: [u8; 32],
		/// The `payment_id` of the rebalance payment sent to the LN wallet.
		ln_rebalance_payment_id: [u8; 32],
		/// The amount, in msats, of the rebalance payment.
		amount_msat: u64,
		/// The fee paid, in msats, for the rebalance payment.
		fee_msat: u64,
	},
	/// We have initiated a splice and are waiting for it to confirm.
	SplicePending {
		/// The `channel_id` of the channel.
		channel_id: ChannelId,
		/// The `user_channel_id` of the channel.
		user_channel_id: UserChannelId,
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The outpoint of the channel's splice funding transaction.
		new_funding_txo: OutPoint,
	},
}

impl_writeable_tlv_based_enum!(Event,
	(0, PaymentSuccessful) => {
		(0, payment_id, required),
		(2, payment_hash, required),
		(4, payment_preimage, required),
		(5, fee_paid_msat, option),
	},
	(1, PaymentFailed) => {
		(0, payment_id, required),
		(1, payment_hash, option),
		(3, reason, upgradable_option),
	},
	(2, PaymentReceived) => {
		(0, payment_id, required),
		(2, payment_hash, required),
		(4, amount_msat, required),
		(5, custom_records, optional_vec),
		(7, lsp_fee_msats, option),
	},
	(3, OnchainPaymentReceived) => {
		(0, payment_id, required),
		(2, txid, required),
		(4, amount_sat, required),
		(6, status, required),
	},
	(4, ChannelOpened) => {
		(0, channel_id, required),
		(2, user_channel_id, required),
		(4, counterparty_node_id, required),
		(6, funding_txo, required),
	},
	(5, ChannelClosed) => {
		(0, channel_id, required),
		(2, user_channel_id, required),
		(4, counterparty_node_id, required),
		(5, reason, upgradable_option),
	},
	(6, RebalanceInitiated) => {
		(0, trigger_payment_id, required),
		(2, trusted_rebalance_payment_id, required),
		(4, amount_msat, required),
	},
	(7, RebalanceSuccessful) => {
		(0, trigger_payment_id, required),
		(2, trusted_rebalance_payment_id, required),
		(4, ln_rebalance_payment_id, required),
		(6, amount_msat, required),
		(8, fee_msat, required),
	},
	(8, SplicePending) => {
		(1, channel_id, required),
		(3, counterparty_node_id, required),
		(5, user_channel_id, required),
		(7, new_funding_txo, required),
	},
);

/// A queue for events emitted by the [`Wallet`].
///
/// [`Wallet`]: [`crate::Wallet`]
pub struct EventQueue {
	queue: Arc<StdMutex<QueueState>>,
	mutation_lock: Arc<Mutex<()>>,
	pending_mpp_events: Arc<Mutex<HashMap<PaymentHash, Vec<Event>>>>,
	changed: watch::Sender<()>,
	kv_store: Arc<dyn DynStore>,
	rebalance_enabled: RebalanceEnabledCache,
	tx_metadata: TxMetadataStore,
	logger: Arc<Logger>,
	runtime: Arc<Runtime>,
}

impl EventQueue {
	/// Reads the persisted queue. A store without one yields an empty queue.
	pub(crate) async fn load_events(kv_store: &dyn DynStore) -> Result<VecDeque<Event>, io::Error> {
		match KVStore::read(
			kv_store,
			EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
			EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
			EVENT_QUEUE_PERSISTENCE_KEY,
		)
		.await
		{
			Ok(data) => {
				let mut reader = &data[..];
				let decoded = EventQueueDeserWrapper::read(&mut reader).map_err(|e| {
					io::Error::new(
						io::ErrorKind::InvalidData,
						format!("Invalid event queue: {e:?}"),
					)
				})?;
				if !reader.is_empty() {
					return Err(io::Error::new(
						io::ErrorKind::InvalidData,
						"Trailing event queue data",
					));
				}
				Ok(decoded.0)
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(VecDeque::new()),
			Err(e) => Err(e),
		}
	}

	/// `restored` is the queue returned by [`EventQueue::load_events`] for `kv_store`.
	pub(crate) fn new(
		kv_store: Arc<dyn DynStore>, restored: VecDeque<Event>, tx_metadata: TxMetadataStore,
		logger: Arc<Logger>, runtime: Arc<Runtime>,
	) -> Self {
		let (changed, _) = watch::channel(());
		let rebalance_enabled = RebalanceEnabledCache::new(Arc::clone(&kv_store));
		let queue = QueueState { restored: restored.len(), events: restored };
		Self {
			queue: Arc::new(StdMutex::new(queue)),
			mutation_lock: Arc::new(Mutex::new(())),
			pending_mpp_events: Arc::new(Mutex::new(HashMap::new())),
			changed,
			kv_store,
			rebalance_enabled,
			tx_metadata,
			logger,
			runtime,
		}
	}

	pub(crate) async fn get_rebalance_enabled(&self) -> bool {
		self.rebalance_enabled.get().await
	}

	pub(crate) async fn set_rebalance_enabled(&self, enabled: bool) {
		self.rebalance_enabled.set(enabled).await;
	}

	/// Starts buffering terminal events for a multi-path payment while its leg metadata is being
	/// registered.
	pub(crate) async fn begin_mpp_setup(&self, payment_hash: PaymentHash) {
		self.pending_mpp_events.lock().await.entry(payment_hash).or_default();
	}

	/// Stops buffering terminal events for a multi-path payment and re-processes any events that
	/// arrived before the leg metadata was registered.
	pub(crate) async fn finish_mpp_setup(
		&self, payment_hash: PaymentHash,
	) -> Result<(), io::Error> {
		let pending_events = self.pending_mpp_events.lock().await.remove(&payment_hash);
		if let Some(events) = pending_events {
			for event in events {
				self.add_event(event).await?;
			}
		}
		Ok(())
	}

	pub(crate) async fn add_event(&self, event: Event) -> Result<(), io::Error> {
		// Outgoing payments split across the trusted and lightning wallets emit a terminal event per
		// leg. Record each leg's result onto the shared, persisted metadata; the leg that completes
		// the payment yields the single combined event we surface instead of the per-leg ones.
		match &event {
			Event::PaymentSuccessful { payment_id, payment_preimage, fee_paid_msat, .. }
				if self.is_mpp_leg(payment_id) =>
			{
				let combined = self
					.tx_metadata
					.record_mpp_leg(
						*payment_id,
						Some((fee_paid_msat.unwrap_or(0), payment_preimage.0)),
					)
					.await;
				return self.push_combined_mpp(combined).await;
			},
			Event::PaymentFailed { payment_id, .. } if self.is_mpp_leg(payment_id) => {
				let combined = self.tx_metadata.record_mpp_leg(*payment_id, None).await;
				return self.push_combined_mpp(combined).await;
			},
			_ => {},
		}

		if let Some(payment_hash) = terminal_payment_hash(&event) {
			let mut pending_mpp_events = self.pending_mpp_events.lock().await;
			if let Some(events) = pending_mpp_events.get_mut(&payment_hash) {
				events.push(event);
				return Ok(());
			}
		}

		self.push_event(event).await
	}

	/// Whether `id` identifies a leg of a multi-path payment.
	fn is_mpp_leg(&self, id: &PaymentId) -> bool {
		matches!(self.tx_metadata.read().get(id).map(|m| m.ty), Some(TxType::MppPayment { .. }))
	}

	/// Surfaces the single combined event for a multi-path payment, or nothing if the payment is
	/// still waiting on its other leg (or already produced its combined event).
	async fn push_combined_mpp(
		&self, combined: Option<(PaymentId, MppOutcome)>,
	) -> Result<(), io::Error> {
		match combined {
			Some((surface_id, MppOutcome::Succeeded { payment_hash, preimage, fee_msat })) => {
				self.push_event(Event::PaymentSuccessful {
					payment_id: surface_id,
					payment_hash: PaymentHash(payment_hash),
					payment_preimage: PaymentPreimage(preimage),
					fee_paid_msat: Some(fee_msat),
				})
				.await
			},
			Some((surface_id, MppOutcome::Failed { payment_hash })) => {
				self.push_event(Event::PaymentFailed {
					payment_id: surface_id,
					payment_hash: Some(PaymentHash(payment_hash)),
					reason: None,
				})
				.await
			},
			None => Ok(()),
		}
	}

	/// Persists each mutation in order without blocking readers of committed events.
	async fn push_event(&self, event: Event) -> Result<(), io::Error> {
		let writer = Arc::clone(&self.mutation_lock).lock_owned().await;
		let data = {
			let queue = self.queue.lock().unwrap();
			// Only events restored from storage can be a replay of an event whose source
			// acknowledgement was lost in a crash. Later duplicates are new occurrences,
			// such as a retried BOLT 11 payment that fails again under the same ID.
			let mut restored = queue.events.iter().take(queue.restored);
			if restored.any(|queued| same_event(queued, &event)) {
				return Ok(());
			}
			if queue.events.len() == u16::MAX as usize {
				return Err(io::Error::new(io::ErrorKind::Other, "Event queue is full"));
			}
			encode_events(queue.events.len() + 1, queue.events.iter().chain(Some(&event)))
		};
		self.commit_queue(data, writer, move |queue| queue.events.push_back(event)).await
	}

	pub(crate) fn next_event(&self) -> Option<Event> {
		self.queue.lock().unwrap().events.front().cloned()
	}

	pub(crate) async fn next_event_async(&self) -> Event {
		// Subscribe before inspecting the queue so an append cannot lose a wake-up.
		let mut changed = self.changed.subscribe();
		loop {
			if let Some(event) = self.next_event() {
				return event;
			}
			changed.changed().await.expect("queue owns the sender");
		}
	}

	pub(crate) async fn event_handled(&self) -> Result<(), io::Error> {
		let writer = Arc::clone(&self.mutation_lock).lock_owned().await;
		let data = {
			let queue = self.queue.lock().unwrap();
			if queue.events.is_empty() {
				return Ok(());
			}
			encode_events(queue.events.len() - 1, queue.events.iter().skip(1))
		};
		self.commit_queue(data, writer, |queue| {
			queue.events.pop_front();
			queue.restored = queue.restored.saturating_sub(1);
		})
		.await
	}

	/// Writes the encoded queue, then applies the same change to the committed queue.
	/// The writer lock serializes mutations, so the queue cannot change in between.
	async fn commit_queue(
		&self, data: Vec<u8>, writer: OwnedMutexGuard<()>,
		apply: impl FnOnce(&mut QueueState) + Send + 'static,
	) -> Result<(), io::Error> {
		let store = Arc::clone(&self.kv_store);
		let queue = Arc::clone(&self.queue);
		let logger = Arc::clone(&self.logger);
		let changed = self.changed.clone();
		// Keep persistence and its in-memory commit together if a caller cancels
		// its wait. Otherwise storage could commit after we released the writer
		// lock, leaving the queue stale or allowing another mutation to race it.
		self.runtime
			.persist(writer, async move {
				KVStore::write(
					store.as_ref(),
					EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
					EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
					EVENT_QUEUE_PERSISTENCE_KEY,
					data,
				)
				.await
				.map_err(|e| {
					log_error!(logger, "Failed to persist Orange event queue: {e}");
					e
				})?;
				apply(&mut queue.lock().unwrap());
				changed.send_replace(());
				Ok::<(), io::Error>(())
			})
			.await
	}
}

/// Committed events plus how many at the head were restored from storage at startup.
struct QueueState {
	events: VecDeque<Event>,
	/// Restored events may be replayed by their source before their acknowledgement was
	/// saved. The count shrinks as they are handled, so only that prefix is deduplicated.
	restored: usize,
}

// A persisted event may be replayed by LDK before its own acknowledgement was saved.
// Compare payment identity, not optional enrichment such as a fee lookup.
fn same_event(left: &Event, right: &Event) -> bool {
	match (left, right) {
		(
			Event::PaymentReceived { payment_id: a, .. },
			Event::PaymentReceived { payment_id: b, .. },
		)
		| (
			Event::PaymentSuccessful { payment_id: a, .. },
			Event::PaymentSuccessful { payment_id: b, .. },
		)
		| (
			Event::PaymentFailed { payment_id: a, .. },
			Event::PaymentFailed { payment_id: b, .. },
		) => a == b,
		_ => left == right,
	}
}

fn terminal_payment_hash(event: &Event) -> Option<PaymentHash> {
	match event {
		Event::PaymentSuccessful { payment_hash, .. } => Some(*payment_hash),
		Event::PaymentFailed { payment_hash, .. } => *payment_hash,
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::store::{PaymentId, PaymentType, TxMetadata, TxMetadataStore, TxType};
	use crate::test_store::{TestStore, test_event_queue, test_runtime};
	use std::sync::atomic::Ordering;
	use std::time::Duration;

	fn mpp_metadata(
		surface_id: PaymentId, lightning_leg: [u8; 32], payment_hash: [u8; 32],
	) -> TxMetadata {
		TxMetadata {
			ty: TxType::MppPayment {
				surface_id,
				lightning_leg,
				payment_hash: Some(payment_hash),
				total_amount_msat: 200_000,
				ty: PaymentType::OutgoingLightningBolt11 { payment_preimage: None },
				trusted_fee_msat: None,
				lightning_fee_msat: None,
				preimage: None,
				failed: false,
				finalized: false,
			},
			time: Duration::from_secs(1),
		}
	}

	async fn test_queue(store: &TestStore) -> Arc<EventQueue> {
		let metadata = TxMetadataStore::new(store.shared()).await;
		test_event_queue(store, metadata, test_runtime()).await
	}

	fn received(id: u8) -> Event {
		Event::PaymentReceived {
			payment_id: PaymentId::SelfCustodial([id; 32]),
			payment_hash: PaymentHash([id; 32]),
			amount_msat: 4_200_000,
			custom_records: Vec::new(),
			lsp_fee_msats: None,
		}
	}

	#[tokio::test]
	async fn restored_payment_event_is_not_appended_again_on_replay() {
		let store = TestStore::default();
		let queue = test_queue(&store).await;
		queue.add_event(received(1)).await.unwrap();
		queue.add_event(received(2)).await.unwrap();
		let restored = test_queue(&store).await;
		let writes = store.writes.load(Ordering::SeqCst);
		let mut replay = received(1);
		if let Event::PaymentReceived { lsp_fee_msats, .. } = &mut replay {
			*lsp_fee_msats = Some(42);
		}
		restored.add_event(replay).await.unwrap();
		assert_eq!(store.writes.load(Ordering::SeqCst), writes);
		assert_eq!(restored.next_event(), Some(received(1)));
		restored.event_handled().await.unwrap();
		assert_eq!(restored.next_event(), Some(received(2)));
		restored.event_handled().await.unwrap();
		assert_eq!(restored.next_event(), None);
	}

	fn failed(id: u8) -> Event {
		Event::PaymentFailed {
			payment_id: PaymentId::SelfCustodial([id; 32]),
			payment_hash: Some(PaymentHash([id; 32])),
			reason: None,
		}
	}

	#[tokio::test]
	async fn repeated_events_outside_the_restored_prefix_are_queued() {
		// A retried BOLT 11 payment reuses its ID, so a second failure is a new event.
		let store = TestStore::default();
		let queue = test_queue(&store).await;
		queue.add_event(failed(1)).await.unwrap();
		queue.add_event(failed(1)).await.unwrap();
		assert_eq!(queue.next_event(), Some(failed(1)));
		queue.event_handled().await.unwrap();
		assert_eq!(queue.next_event(), Some(failed(1)));
		queue.event_handled().await.unwrap();
		assert_eq!(queue.next_event(), None);

		// After a restart the restored copy absorbs one replay, and only while it is
		// still queued. Once handled, the same failure enqueues again.
		queue.add_event(failed(2)).await.unwrap();
		let restored = test_queue(&store).await;
		restored.add_event(failed(2)).await.unwrap();
		assert_eq!(restored.next_event(), Some(failed(2)));
		restored.event_handled().await.unwrap();
		assert_eq!(restored.next_event(), None);
		restored.add_event(failed(2)).await.unwrap();
		assert_eq!(restored.next_event(), Some(failed(2)));
		// An event appended after restore is not part of the restored prefix.
		restored.add_event(failed(3)).await.unwrap();
		restored.add_event(failed(3)).await.unwrap();
		restored.event_handled().await.unwrap();
		assert_eq!(restored.next_event(), Some(failed(3)));
		restored.event_handled().await.unwrap();
		assert_eq!(restored.next_event(), Some(failed(3)));
	}

	#[test]
	fn queue_writes_use_owned_runtime_and_shutdown_drains_cancelled_waits() {
		let store = TestStore::default();
		let runtime = test_runtime();
		let metadata = runtime.block_on(TxMetadataStore::new(store.shared()));
		let queue = runtime.block_on(test_event_queue(&store, metadata, Arc::clone(&runtime)));
		assert!(tokio::runtime::Handle::try_current().is_err());
		let mut append = Box::pin(queue.add_event(received(1)));
		let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
		let _ = append.as_mut().poll(&mut cx);
		drop(append);
		runtime.wait_on_background_tasks();
		assert_eq!(queue.next_event(), Some(received(1)));
		let mut ack = Box::pin(queue.event_handled());
		let _ = ack.as_mut().poll(&mut cx);
		drop(ack);
		runtime.wait_on_background_tasks();
		assert_eq!(queue.next_event(), None);
	}

	#[tokio::test]
	async fn queue_restores_events_and_failed_ack_is_retryable() {
		let store = TestStore::default();
		let queue = test_queue(&store).await;
		queue.add_event(received(1)).await.unwrap();
		queue.add_event(received(2)).await.unwrap();
		drop(queue);
		let queue = test_queue(&store).await;
		assert_eq!(queue.next_event_async().await, received(1));
		store.fail_next_write.store(true, Ordering::SeqCst);
		assert!(queue.event_handled().await.is_err());
		assert_eq!(queue.next_event(), Some(received(1)));
		queue.event_handled().await.unwrap();
		assert_eq!(queue.next_event(), Some(received(2)));
		drop(queue);
		let queue = test_queue(&store).await;
		assert_eq!(queue.next_event(), Some(received(2)));
		queue.event_handled().await.unwrap();
		let writes = store.writes.load(Ordering::SeqCst);
		queue.event_handled().await.unwrap();
		assert_eq!(store.writes.load(Ordering::SeqCst), writes);
		assert_eq!(test_queue(&store).await.next_event(), None);
	}

	#[tokio::test]
	async fn pending_append_does_not_block_committed_events() {
		let store = TestStore::default();
		let queue = test_queue(&store).await;
		queue.add_event(received(1)).await.unwrap();
		let mut writes = store.control_writes();
		let writer = Arc::clone(&queue);
		let append = tokio::spawn(async move { writer.add_event(received(2)).await });
		let finish = writes.recv().await.unwrap();
		let event =
			tokio::time::timeout(Duration::from_secs(1), queue.next_event_async()).await.unwrap();
		assert_eq!(event, received(1));
		finish.send(()).unwrap();
		append.await.unwrap().unwrap();
		assert_eq!(test_queue(&store).await.next_event(), Some(received(1)));
	}

	#[tokio::test]
	async fn failed_append_stays_invisible_and_retry_wakes_consumer() {
		let store = TestStore::default();
		let queue = test_queue(&store).await;
		let consumer = Arc::clone(&queue);
		let next = tokio::spawn(async move { consumer.next_event_async().await });
		store.fail_next_write.store(true, Ordering::SeqCst);
		assert!(queue.add_event(received(1)).await.is_err());
		assert_eq!(queue.next_event(), None);
		assert_eq!(test_queue(&store).await.next_event(), None);
		queue.add_event(received(1)).await.unwrap();
		assert_eq!(
			tokio::time::timeout(Duration::from_secs(1), next).await.unwrap().unwrap(),
			received(1)
		);
	}

	#[tokio::test]
	async fn cancelling_append_wait_does_not_split_store_and_queue() {
		let store = TestStore::default();
		let queue = test_queue(&store).await;
		let mut writes = store.control_writes();
		let writer = Arc::clone(&queue);
		let append = tokio::spawn(async move { writer.add_event(received(1)).await });
		let finish = writes.recv().await.unwrap();
		assert_eq!(queue.next_event(), None);
		append.abort();
		assert!(append.await.unwrap_err().is_cancelled());
		finish.send(()).unwrap();
		assert_eq!(
			tokio::time::timeout(Duration::from_secs(1), queue.next_event_async()).await.unwrap(),
			received(1)
		);
		assert_eq!(test_queue(&store).await.next_event(), Some(received(1)));
	}

	#[tokio::test]
	async fn invalid_or_unreadable_queue_fails_initialization() {
		let store = TestStore::default();
		store.fail_next_read.store(true, Ordering::SeqCst);
		assert!(EventQueue::load_events(&store).await.is_err());
		for data in [vec![], vec![0, 1], vec![0, 0, 1]] {
			KVStore::write(&store, "", "", EVENT_QUEUE_PERSISTENCE_KEY, data).await.unwrap();
			assert!(EventQueue::load_events(&store).await.is_err());
		}
	}

	#[tokio::test]
	async fn pending_mpp_setup_buffers_terminal_events_until_metadata_exists() {
		let store = TestStore::default();
		let tx_metadata = TxMetadataStore::new(store.shared()).await;
		let queue = test_event_queue(&store, tx_metadata.clone(), test_runtime()).await;

		let payment_hash = PaymentHash([3u8; 32]);
		let surface_id = PaymentId::Trusted([7u8; 32]);
		let lightning_leg = [4; 32];
		let lightning_id = PaymentId::SelfCustodial(lightning_leg);
		let preimage = PaymentPreimage([1u8; 32]);

		queue.begin_mpp_setup(payment_hash).await;
		queue
			.add_event(Event::PaymentSuccessful {
				payment_id: surface_id,
				payment_hash,
				payment_preimage: preimage,
				fee_paid_msat: Some(1_000),
			})
			.await
			.expect("buffer event");
		assert_eq!(queue.next_event(), None);

		tx_metadata
			.insert(surface_id, mpp_metadata(surface_id, lightning_leg, payment_hash.0))
			.await;
		tx_metadata
			.upsert(lightning_id, mpp_metadata(surface_id, lightning_leg, payment_hash.0))
			.await;
		queue.finish_mpp_setup(payment_hash).await.expect("replay buffered events");
		assert_eq!(queue.next_event(), None);

		queue
			.add_event(Event::PaymentSuccessful {
				payment_id: lightning_id,
				payment_hash,
				payment_preimage: preimage,
				fee_paid_msat: Some(2_000),
			})
			.await
			.expect("complete mpp");

		assert_eq!(
			queue.next_event(),
			Some(Event::PaymentSuccessful {
				payment_id: surface_id,
				payment_hash,
				payment_preimage: preimage,
				fee_paid_msat: Some(3_000),
			})
		);
	}
}

struct EventQueueDeserWrapper(VecDeque<Event>);

impl Readable for EventQueueDeserWrapper {
	fn read<R: io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let len: u16 = Readable::read(reader)?;
		let mut queue = VecDeque::new();
		for _ in 0..len {
			queue.push_back(Readable::read(reader)?);
		}
		Ok(Self(queue))
	}
}

/// Encodes `len` events in the format [`EventQueueDeserWrapper`] reads.
fn encode_events<'a>(len: usize, events: impl Iterator<Item = &'a Event>) -> Vec<u8> {
	let mut data = Vec::new();
	(len as u16).write(&mut data).expect("in-memory writes cannot fail");
	for event in events {
		event.write(&mut data).expect("in-memory writes cannot fail");
	}
	data
}

#[derive(Clone)]
pub(crate) struct LdkEventHandler {
	pub(crate) event_queue: Arc<EventQueue>,
	pub(crate) ldk_node: Arc<ldk_node::Node>,
	pub(crate) tx_metadata: store::TxMetadataStore,
	pub(crate) payment_receipt_inbox: Arc<PaymentReceiptInbox>,
	pub(crate) channel_pending_sender: watch::Sender<u128>,
	pub(crate) splice_pending_inbox: Arc<SplicePendingInbox>,
	pub(crate) logger: Arc<Logger>,
}

impl LdkEventHandler {
	pub(crate) async fn handle_ldk_node_event(&self, event: ldk_node::Event) -> bool {
		match event {
			ldk_node::Event::PaymentSuccessful {
				payment_id,
				payment_hash,
				payment_preimage,
				fee_paid_msat,
				bolt12_invoice: _,
			} => {
				let preimage = payment_preimage.unwrap(); // safe
				let payment_id = PaymentId::SelfCustodial(payment_id.0);

				if self.tx_metadata.set_preimage(payment_id, preimage.0).await.is_err() {
					log_error!(self.logger, "Failed to set preimage for payment {payment_id:?}");
				}

				if let Err(e) = self
					.event_queue
					.add_event(Event::PaymentSuccessful {
						payment_id,
						payment_hash,
						payment_preimage: preimage,
						fee_paid_msat,
					})
					.await
				{
					log_error!(self.logger, "Failed to add PaymentSuccessful event: {e:?}");
					return false;
				}
			},
			ldk_node::Event::PaymentFailed { payment_id, payment_hash, reason } => {
				if let Err(e) = self
					.event_queue
					.add_event(Event::PaymentFailed {
						payment_id: PaymentId::SelfCustodial(payment_id.0),
						payment_hash,
						reason,
					})
					.await
				{
					log_error!(self.logger, "Failed to add PaymentFailed event: {e:?}");
					return false;
				}
			},
			ldk_node::Event::PaymentReceived {
				payment_id,
				payment_hash,
				amount_msat,
				custom_records,
			} => {
				let payment = self.ldk_node.payment(&payment_id).unwrap_or_else(|e| {
					log_error!(self.logger, "Failed to read received payment fee: {e}");
					None
				});
				let lsp_fee_msats = payment.and_then(|p| {
					if let PaymentKind::Bolt11 { counterparty_skimmed_fee_msat, .. } = p.kind {
						counterparty_skimmed_fee_msat
					} else {
						None
					}
				});

				if let Err(e) = self
					.event_queue
					.add_event(Event::PaymentReceived {
						payment_id: PaymentId::SelfCustodial(payment_id.0),
						payment_hash,
						amount_msat,
						custom_records,
						lsp_fee_msats,
					})
					.await
				{
					log_error!(self.logger, "Failed to add PaymentReceived event: {e:?}");
					return false;
				}
				self.payment_receipt_inbox.deliver(
					payment_hash.0,
					ReceivedLightningPayment { id: payment_id.0, fee_paid_msat: lsp_fee_msats },
				);
			},
			ldk_node::Event::PaymentForwarded { .. } => {},
			ldk_node::Event::PaymentClaimable { .. } => {
				log_warn!(
					self.logger,
					"Unexpected PaymentClaimable event received. This is likely due to a bug in the LDK Node implementation."
				);
			},
			ldk_node::Event::ChannelPending { funding_txo, .. } => {
				log_debug!(self.logger, "Received ChannelPending event");
				// The funding tx is already in `ldk_node.list_payments()`; populate our
				// metadata before any concurrent `list_transactions` call observes the
				// outbound payment.
				self.reserve_rebalance_slot_for_funding_tx(funding_txo.txid).await;
			},
			ldk_node::Event::ChannelReady {
				channel_id,
				user_channel_id,
				counterparty_node_id,
				funding_txo,
			} => {
				let funding_txo = funding_txo.unwrap(); // safe

				if let Err(e) = self
					.event_queue
					.add_event(Event::ChannelOpened {
						channel_id,
						user_channel_id,
						counterparty_node_id: counterparty_node_id.unwrap(), // safe
						funding_txo,
					})
					.await
				{
					log_error!(self.logger, "Failed to add ChannelOpened event: {e:?}");
					return false;
				}
				let _ = self.channel_pending_sender.send(user_channel_id.0);
			},
			ldk_node::Event::ChannelClosed {
				channel_id,
				user_channel_id,
				counterparty_node_id,
				reason,
			} => {
				// We experienced a channel close, we disable rebalancing so we don't automatically
				// try to reopen the channel.
				self.event_queue.set_rebalance_enabled(false).await;

				if let Err(e) = self
					.event_queue
					.add_event(Event::ChannelClosed {
						channel_id,
						user_channel_id,
						counterparty_node_id,
						reason,
					})
					.await
				{
					log_error!(self.logger, "Failed to add ChannelClosed event: {e:?}");
					return false;
				}
			},
			ldk_node::Event::SpliceNegotiated {
				channel_id,
				user_channel_id,
				counterparty_node_id,
				new_funding_txo,
			} => {
				log_debug!(self.logger, "Received SpliceNegotiated event {event:?}");
				// Reserve the metadata slot before delivering so any task waking on the
				// inbox (the rebalancer's `OnChainRebalanceInitiated` for splice-in,
				// `pay_lightning` for splice-out) sees an entry to upsert.
				self.reserve_rebalance_slot_for_funding_tx(new_funding_txo.txid).await;
				self.splice_pending_inbox.deliver(user_channel_id.0, new_funding_txo);

				if let Err(e) = self
					.event_queue
					.add_event(Event::SplicePending {
						channel_id,
						user_channel_id,
						counterparty_node_id,
						new_funding_txo,
					})
					.await
				{
					log_error!(self.logger, "Failed to add SplicePending event: {e:?}");
					return false;
				}
			},
			ldk_node::Event::SpliceNegotiationFailed { .. } => {
				log_warn!(self.logger, "Received SpliceNegotiationFailed event: {event:?}");
			},
		}

		if let Err(e) = self.ldk_node.event_handled() {
			log_error!(self.logger, "Failed to handle event: {e:?}");
			return false;
		}
		true
	}

	/// Reserve a `PendingRebalance` metadata slot for a freshly broadcast channel or splice
	/// funding tx. The matching outbound on-chain payment is already visible in
	/// `ldk_node.list_payments()` by the time we're called, so without this entry
	/// `list_transactions` would trip its `debug_assert_ne!`. `PendingRebalance` is used as the
	/// placeholder because `list_transactions` already skips it.
	async fn reserve_rebalance_slot_for_funding_tx(&self, txid: Txid) {
		let payment_id = PaymentId::SelfCustodial(txid.to_byte_array());
		if self.tx_metadata.read().get(&payment_id).is_some() {
			return;
		}
		self.tx_metadata
			.upsert(
				payment_id,
				store::TxMetadata {
					ty: store::TxType::PendingRebalance {},
					time: SystemTime::now()
						.duration_since(SystemTime::UNIX_EPOCH)
						.unwrap_or_default(),
				},
			)
			.await;
	}
}
