use crate::logging::Logger;
use crate::store::{self, MppOutcome, PaymentId, RebalanceEnabledCache, TxMetadataStore, TxType};

use crate::dyn_store::DynStore;
use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::{OutPoint, Txid};
use ldk_node::lightning::events::{ClosureReason, PaymentFailureReason};
use ldk_node::lightning::ln::types::ChannelId;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::util::ser::{Readable, Writeable, Writer};
use ldk_node::lightning::{
	impl_writeable_tlv_based_enum, log_debug, log_error, log_info, log_warn,
};
use ldk_node::lightning_types::payment::{PaymentHash, PaymentPreimage};
use ldk_node::payment::{ConfirmationStatus, PaymentKind};
use ldk_node::{CustomTlvRecord, UserChannelId};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::SystemTime;
use tokio::sync::{Mutex, watch};

/// The event queue will be persisted under this key.
pub(crate) const EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE: &str = "";
pub(crate) const EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";
pub(crate) const EVENT_QUEUE_PERSISTENCE_KEY: &str = "orange_events";

/// An event emitted by [`Wallet`], which should be handled by the user.
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
	queue: Arc<Mutex<VecDeque<Event>>>,
	pending_mpp_events: Arc<Mutex<HashMap<PaymentHash, Vec<Event>>>>,
	waker: Arc<Mutex<Option<Waker>>>,
	kv_store: Arc<dyn DynStore>,
	rebalance_enabled: RebalanceEnabledCache,
	tx_metadata: TxMetadataStore,
	logger: Arc<Logger>,
}

impl EventQueue {
	/// Builds the queue over `kv_store`, starting from the events a previous run left unhandled.
	///
	/// The queue is persisted under [`EVENT_QUEUE_PERSISTENCE_KEY`] after every push and every
	/// [`EventQueue::event_handled`], so the stored copy is exactly the set of events the user has
	/// not acknowledged yet. Loading it means an event emitted shortly before a shutdown, or one
	/// the user never got to handle, is delivered on the next run instead of being dropped.
	///
	/// Delivery is at-least-once: an LDK event is persisted here before it is acknowledged to
	/// ldk-node, so a crash between the two delivers it again after a restart, once from this
	/// queue and once from ldk-node's own replay. Consumers must tolerate a repeat, as consumers
	/// of ldk-node's own queue already must: it is reloaded at build time and an event stays at
	/// its front until `event_handled`.
	///
	/// A missing key is a fresh wallet and starts empty. Any other read error, or a stored queue
	/// that does not decode, fails the build: the wallet must not silently start without events
	/// it owes the user.
	pub(crate) async fn new(
		kv_store: Arc<dyn DynStore>, tx_metadata: TxMetadataStore, logger: Arc<Logger>,
	) -> Result<Self, ldk_node::lightning::io::Error> {
		let persisted = match KVStore::read(
			kv_store.as_ref(),
			EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
			EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
			EVENT_QUEUE_PERSISTENCE_KEY,
		)
		.await
		{
			Ok(bytes) => {
				EventQueueDeserWrapper::read(&mut &bytes[..])
					.map_err(|e| {
						log_error!(logger.as_ref(), "Failed to deserialize event queue: {}", e);
						ldk_node::lightning::io::Error::new(
							ldk_node::lightning::io::ErrorKind::InvalidData,
							"Failed to deserialize EventQueue",
						)
					})?
					.0
			},
			Err(e) if e.kind() == ldk_node::lightning::io::ErrorKind::NotFound => VecDeque::new(),
			Err(e) => {
				log_error!(logger.as_ref(), "Failed to read event queue from store: {}", e);
				return Err(e);
			},
		};
		if !persisted.is_empty() {
			log_info!(
				logger.as_ref(),
				"Loaded {} unhandled event(s) from the store",
				persisted.len()
			);
		}
		let queue = Arc::new(Mutex::new(persisted));
		let pending_mpp_events = Arc::new(Mutex::new(HashMap::new()));
		let waker = Arc::new(Mutex::new(None));
		let rebalance_enabled = RebalanceEnabledCache::new(Arc::clone(&kv_store));
		Ok(Self {
			queue,
			pending_mpp_events,
			waker,
			kv_store,
			rebalance_enabled,
			tx_metadata,
			logger,
		})
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
	) -> Result<(), ldk_node::lightning::io::Error> {
		let pending_events = self.pending_mpp_events.lock().await.remove(&payment_hash);
		if let Some(events) = pending_events {
			for event in events {
				self.add_event(event).await?;
			}
		}
		Ok(())
	}

	pub(crate) async fn add_event(
		&self, event: Event,
	) -> Result<(), ldk_node::lightning::io::Error> {
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
	) -> Result<(), ldk_node::lightning::io::Error> {
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

	/// Appends an event to the queue and persists it, waking any pending consumer.
	async fn push_event(&self, event: Event) -> Result<(), ldk_node::lightning::io::Error> {
		{
			let mut locked_queue = self.queue.lock().await;
			locked_queue.push_back(event);
			self.persist_queue(&locked_queue).await?;
		}

		if let Some(waker) = self.waker.lock().await.take() {
			waker.wake();
		}
		Ok(())
	}

	pub(crate) async fn next_event(&self) -> Option<Event> {
		let locked_queue = self.queue.lock().await;
		locked_queue.front().cloned()
	}

	pub(crate) async fn next_event_async(&self) -> Event {
		EventFuture { event_queue: Arc::clone(&self.queue), waker: Arc::clone(&self.waker) }.await
	}

	pub(crate) async fn event_handled(&self) -> Result<(), ldk_node::lightning::io::Error> {
		{
			let mut locked_queue = self.queue.lock().await;
			locked_queue.pop_front();
			self.persist_queue(&locked_queue).await?;
		}

		if let Some(waker) = self.waker.lock().await.take() {
			waker.wake();
		}
		Ok(())
	}

	async fn persist_queue(
		&self, locked_queue: &VecDeque<Event>,
	) -> Result<(), ldk_node::lightning::io::Error> {
		let data = EventQueueSerWrapper(locked_queue).encode();
		KVStore::write(
			self.kv_store.as_ref(),
			EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
			EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
			EVENT_QUEUE_PERSISTENCE_KEY,
			data,
		)
		.await
		.map_err(|e| {
			log_error!(
				self.logger.as_ref(),
				"Write for key {}/{}/{} failed due to: {}",
				EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
				EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
				EVENT_QUEUE_PERSISTENCE_KEY,
				e
			);
			e
		})?;
		Ok(())
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
	use crate::logging::LoggerType;
	use crate::store::{PaymentId, PaymentType, TxMetadata, TxMetadataStore, TxType};
	use ldk_node::io::sqlite_store::SqliteStore;
	use ldk_node::lightning::io;
	use ldk_node::lightning::util::persist::{PageToken, PaginatedListResponse};
	use std::future::Future;
	use std::path::PathBuf;
	use std::pin::Pin;
	use std::time::{Duration, UNIX_EPOCH};

	fn temp_sqlite_store() -> (PathBuf, Arc<dyn DynStore>) {
		let path = std::env::temp_dir().join(format!(
			"orange-sdk-event-mpp-buffer-test-{}",
			SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
		));
		let store = SqliteStore::new(path.clone(), Some("orange.sqlite".to_string()), None)
			.expect("sqlite store");
		(path, Arc::new(store))
	}

	fn mpp_metadata(surface_id: PaymentId, lightning_leg: [u8; 32]) -> TxMetadata {
		TxMetadata {
			ty: TxType::MppPayment {
				surface_id,
				lightning_leg,
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

	#[tokio::test]
	async fn pending_mpp_setup_buffers_terminal_events_until_metadata_exists() {
		let (_path, store) = temp_sqlite_store();
		let tx_metadata = TxMetadataStore::new(Arc::clone(&store)).await;
		let queue = EventQueue::new(
			store,
			tx_metadata.clone(),
			Arc::new(Logger::new(&LoggerType::LogFacade).expect("logger")),
		)
		.await
		.expect("event queue");

		let payment_hash = PaymentHash([3u8; 32]);
		let surface_id = PaymentId::Trusted([7u8; 32]);
		let lightning_id = PaymentId::SelfCustodial(payment_hash.0);
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
		assert_eq!(queue.next_event().await, None);

		tx_metadata.insert(surface_id, mpp_metadata(surface_id, payment_hash.0)).await;
		tx_metadata.upsert(lightning_id, mpp_metadata(surface_id, payment_hash.0)).await;
		queue.finish_mpp_setup(payment_hash).await.expect("replay buffered events");
		assert_eq!(queue.next_event().await, None);

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
			queue.next_event().await,
			Some(Event::PaymentSuccessful {
				payment_id: surface_id,
				payment_hash,
				payment_preimage: preimage,
				fee_paid_msat: Some(3_000),
			})
		);
	}

	fn failed_payment(tag: u8) -> Event {
		Event::PaymentFailed {
			payment_id: PaymentId::SelfCustodial([tag; 32]),
			payment_hash: Some(PaymentHash([tag; 32])),
			reason: None,
		}
	}

	/// The queue is persisted after every push and every acknowledgement; a queue built over the
	/// same store must start from that copy, or every event still unhandled at a shutdown is lost.
	#[tokio::test]
	async fn unhandled_events_survive_a_restart() {
		let (_path, store) = temp_sqlite_store();
		let logger = Arc::new(Logger::new(&LoggerType::LogFacade).expect("logger"));
		let tx_metadata = TxMetadataStore::new(Arc::clone(&store)).await;

		let queue = EventQueue::new(Arc::clone(&store), tx_metadata.clone(), Arc::clone(&logger))
			.await
			.expect("event queue");
		queue.add_event(failed_payment(1)).await.expect("push");
		queue.add_event(failed_payment(2)).await.expect("push");
		drop(queue);

		let queue = EventQueue::new(Arc::clone(&store), tx_metadata.clone(), Arc::clone(&logger))
			.await
			.expect("event queue");
		assert_eq!(queue.next_event().await, Some(failed_payment(1)));
		queue.event_handled().await.expect("ack");
		assert_eq!(queue.next_event().await, Some(failed_payment(2)));
		drop(queue);

		// The acknowledgement was persisted too: only the second event comes back.
		let queue =
			EventQueue::new(Arc::clone(&store), tx_metadata, logger).await.expect("event queue");
		assert_eq!(queue.next_event().await, Some(failed_payment(2)));
		queue.event_handled().await.expect("ack");
		assert_eq!(queue.next_event().await, None);
	}

	/// A stored queue that does not decode fails the build instead of starting empty.
	#[tokio::test]
	async fn an_undecodable_stored_queue_fails_the_build() {
		let (_path, store) = temp_sqlite_store();
		// One event announced, then a variant id no `Event` has.
		KVStore::write(
			store.as_ref(),
			EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
			EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
			EVENT_QUEUE_PERSISTENCE_KEY,
			vec![0x00, 0x01, 0xff],
		)
		.await
		.expect("write");
		let logger = Arc::new(Logger::new(&LoggerType::LogFacade).expect("logger"));
		let tx_metadata = TxMetadataStore::new(Arc::clone(&store)).await;
		let err = EventQueue::new(store, tx_metadata, logger).await.err().expect("must fail");
		assert_eq!(err.kind(), io::ErrorKind::InvalidData);
	}

	/// A store that cannot be read at all; everything else goes to the real store.
	struct ReadFailsStore(Arc<dyn DynStore>);

	impl DynStore for ReadFailsStore {
		fn read_async(
			&self, _p: &str, _s: &str, _k: &str,
		) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, io::Error>> + Send + 'static>> {
			Box::pin(async { Err(io::Error::new(io::ErrorKind::Other, "store down")) })
		}

		fn write_async(
			&self, p: &str, s: &str, k: &str, buf: Vec<u8>,
		) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send + 'static>> {
			self.0.write_async(p, s, k, buf)
		}

		fn remove_async(
			&self, p: &str, s: &str, k: &str, lazy: bool,
		) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send + 'static>> {
			self.0.remove_async(p, s, k, lazy)
		}

		fn list_async(
			&self, p: &str, s: &str,
		) -> Pin<Box<dyn Future<Output = Result<Vec<String>, io::Error>> + Send + 'static>> {
			self.0.list_async(p, s)
		}

		fn list_paginated_async(
			&self, p: &str, s: &str, page_token: Option<PageToken>,
		) -> Pin<Box<dyn Future<Output = Result<PaginatedListResponse, io::Error>> + Send + 'static>>
		{
			self.0.list_paginated_async(p, s, page_token)
		}
	}

	/// A store that cannot be read fails the build with the store's own error; the queue never
	/// starts empty over a store it could not read.
	#[tokio::test]
	async fn an_unreadable_store_fails_the_build() {
		let (_path, sqlite) = temp_sqlite_store();
		let tx_metadata = TxMetadataStore::new(Arc::clone(&sqlite)).await;
		let store: Arc<dyn DynStore> = Arc::new(ReadFailsStore(sqlite));
		let logger = Arc::new(Logger::new(&LoggerType::LogFacade).expect("logger"));
		let err = EventQueue::new(store, tx_metadata, logger).await.err().expect("must fail");
		assert_eq!(err.kind(), io::ErrorKind::Other);
	}
}

struct EventQueueSerWrapper<'a>(&'a VecDeque<Event>);

impl Writeable for EventQueueSerWrapper<'_> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ldk_node::lightning::io::Error> {
		(self.0.len() as u16).write(writer)?;
		for e in self.0.iter() {
			e.write(writer)?;
		}
		Ok(())
	}
}

struct EventQueueDeserWrapper(VecDeque<Event>);

impl Readable for EventQueueDeserWrapper {
	fn read<R: ldk_node::lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, ldk_node::lightning::ln::msgs::DecodeError> {
		let len: u16 = Readable::read(reader)?;
		let mut queue = VecDeque::with_capacity(len as usize);
		for _ in 0..len {
			queue.push_back(Readable::read(reader)?);
		}
		Ok(Self(queue))
	}
}

struct EventFuture {
	event_queue: Arc<Mutex<VecDeque<Event>>>,
	waker: Arc<Mutex<Option<Waker>>>,
}

impl Future for EventFuture {
	type Output = Event;

	fn poll(
		self: core::pin::Pin<&mut Self>, cx: &mut core::task::Context<'_>,
	) -> Poll<Self::Output> {
		if let Some(event) = self.event_queue.try_lock().ok().and_then(|q| q.front().cloned()) {
			Poll::Ready(event)
		} else {
			if let Ok(mut waker) = self.waker.try_lock() {
				*waker = Some(cx.waker().clone());
			}
			Poll::Pending
		}
	}
}

#[derive(Clone)]
pub(crate) struct LdkEventHandler {
	pub(crate) event_queue: Arc<EventQueue>,
	pub(crate) ldk_node: Arc<ldk_node::Node>,
	pub(crate) tx_metadata: store::TxMetadataStore,
	pub(crate) payment_receipt_sender: watch::Sender<()>,
	pub(crate) channel_pending_sender: watch::Sender<u128>,
	pub(crate) splice_pending_inbox: Arc<crate::lightning_wallet::SplicePendingInbox>,
	pub(crate) logger: Arc<Logger>,
}

impl LdkEventHandler {
	pub(crate) async fn handle_ldk_node_event(&self, event: ldk_node::Event) {
		match event {
			ldk_node::Event::PaymentSuccessful {
				payment_id,
				payment_hash,
				payment_preimage,
				fee_paid_msat,
				bolt12_invoice: _,
			} => {
				let preimage = payment_preimage.unwrap(); // safe
				let payment_id = PaymentId::SelfCustodial(payment_id.unwrap().0); // safe

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
					return;
				}
			},
			ldk_node::Event::PaymentFailed { payment_id, payment_hash, reason } => {
				if let Err(e) = self
					.event_queue
					.add_event(Event::PaymentFailed {
						payment_id: PaymentId::SelfCustodial(payment_id.unwrap().0), // safe
						payment_hash,
						reason,
					})
					.await
				{
					log_error!(self.logger, "Failed to add PaymentFailed event: {e:?}");
					return;
				}
			},
			ldk_node::Event::PaymentReceived {
				payment_id,
				payment_hash,
				amount_msat,
				custom_records,
			} => {
				let payment_id = payment_id.expect("this is safe");
				let lsp_fee_msats = self.ldk_node.payment(&payment_id).and_then(|p| {
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
				}
				let _ = self.payment_receipt_sender.send(());
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
					return;
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
					return;
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
					return;
				}
			},
			ldk_node::Event::SpliceNegotiationFailed { .. } => {
				log_warn!(self.logger, "Received SpliceNegotiationFailed event: {event:?}");
			},
		}

		if let Err(e) = self.ldk_node.event_handled() {
			log_error!(self.logger, "Failed to handle event: {e:?}");
		}
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
