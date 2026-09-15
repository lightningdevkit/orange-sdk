//! Keeps submitted payments visible when a trusted backend has no transaction for them.

use super::{Payment, TrustedError};
use crate::dyn_store::{DynStore, read_keys_bounded};
use crate::logging::Logger;
use crate::store::{PaymentId, TxStatus};
use bitcoin_payment_instructions::amount::Amount;
use ldk_node::lightning::impl_writeable_tlv_based;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::util::ser::{Readable, Writeable};
use ldk_node::lightning::{log_error, log_warn};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const PRIMARY: &str = "orange_sdk";
const SECONDARY: &str = "trusted_payments";

#[derive(Clone)]
struct StoredPayment {
	id: [u8; 32],
	amount_msat: u64,
	status: TxStatus,
	time: u64,
	/// The backend's own identifier for the payment, when it differs from `id`.
	reference: Option<String>,
}

impl_writeable_tlv_based!(StoredPayment, {
	(0, id, required),
	(2, amount_msat, required),
	(4, status, required),
	(6, time, required),
	(7, reference, option)
});

impl StoredPayment {
	fn to_payment(&self) -> Payment {
		Payment {
			id: self.id,
			amount: Amount::from_milli_sats(self.amount_msat).expect("Stored valid amount"),
			fee: Amount::ZERO,
			status: self.status,
			outbound: true,
			time_since_epoch: Duration::from_secs(self.time),
		}
	}
}

pub(super) struct PaymentStore {
	store: Arc<dyn DynStore>,
	logger: Arc<Logger>,
	payments: Mutex<HashMap<[u8; 32], StoredPayment>>,
}

impl PaymentStore {
	pub async fn new(store: Arc<dyn DynStore>, logger: Arc<Logger>) -> Result<Self, TrustedError> {
		let keys = KVStore::list(store.as_ref(), PRIMARY, SECONDARY).await?;
		let records = read_keys_bounded(Arc::clone(&store), PRIMARY, SECONDARY, keys).await?;
		let mut payments = HashMap::with_capacity(records.len());
		for (key, bytes) in records {
			// History metadata is not worth refusing to start the wallet over.
			match StoredPayment::read(&mut &bytes[..]) {
				Ok(payment) => {
					payments.insert(payment.id, payment);
				},
				Err(e) => log_error!(logger, "Skipping invalid stored trusted payment {key}: {e}"),
			}
		}
		Ok(Self { store, logger, payments: Mutex::new(payments) })
	}

	/// Save before background work; return false if this ID is already submitted.
	/// `reference` is the backend's own identifier, kept so the payment can be looked up later.
	pub async fn insert_pending(
		&self, id: [u8; 32], amount: Amount, reference: Option<String>,
	) -> Result<bool, TrustedError> {
		let mut payments = self.payments.lock().await;
		if payments.get(&id).is_some_and(|p| p.status != TxStatus::Failed) {
			return Ok(false);
		}
		let payment = StoredPayment {
			id,
			amount_msat: amount.milli_sats(),
			status: TxStatus::Pending,
			time: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
			reference,
		};
		self.persist(&payment).await?;
		payments.insert(id, payment);
		Ok(true)
	}

	/// Only call for a confirmed failure. Transport errors leave the payment pending.
	/// Returns false if this outcome was already recorded.
	pub async fn mark_failed(&self, id: [u8; 32]) -> Result<bool, TrustedError> {
		self.mark(id, TxStatus::Failed).await
	}

	/// Record a confirmed success until the backend lists the payment itself.
	/// Returns false if this outcome was already recorded.
	pub async fn mark_completed(&self, id: [u8; 32]) -> Result<bool, TrustedError> {
		self.mark(id, TxStatus::Completed).await
	}

	async fn mark(&self, id: [u8; 32], status: TxStatus) -> Result<bool, TrustedError> {
		let mut payments = self.payments.lock().await;
		let Some(mut payment) = payments.get(&id).cloned() else {
			// Nothing recorded, so nothing has been reported for it either.
			return Ok(true);
		};
		if payment.status == status {
			return Ok(false);
		}
		payment.status = status;
		self.persist(&payment).await?;
		payments.insert(id, payment);
		Ok(true)
	}

	/// Submitted payments whose outcome is not yet known, with their backend references.
	pub async fn pending(&self) -> Vec<([u8; 32], Option<String>)> {
		self.payments
			.lock()
			.await
			.values()
			.filter(|p| p.status == TxStatus::Pending)
			.map(|p| (p.id, p.reference.clone()))
			.collect()
	}

	async fn persist(&self, payment: &StoredPayment) -> Result<(), TrustedError> {
		KVStore::write(
			self.store.as_ref(),
			PRIMARY,
			SECONDARY,
			&PaymentId::Trusted(payment.id).to_string(),
			payment.encode(),
		)
		.await?;
		Ok(())
	}

	/// Backend terminal records take precedence and supply settled amounts and fees. Local
	/// records they supersede are dropped so the store does not grow with every send.
	///
	/// This runs on every history listing and rebalance check, so the backend list passes
	/// through untouched when there is nothing local to merge.
	pub async fn merge(&self, mut backend: Vec<Payment>) -> Vec<Payment> {
		let mut local = self.payments.lock().await;
		if local.is_empty() {
			return backend;
		}
		let mut covered = HashSet::with_capacity(local.len());
		let mut pruned = Vec::new();
		backend.retain(|payment| {
			let Some(record) = local.get(&payment.id) else { return true };
			if payment.status == TxStatus::Pending {
				// A confirmed local failure outranks a backend record that never settled.
				if record.status == TxStatus::Failed {
					return false;
				}
			} else {
				pruned.push(payment.id);
			}
			covered.insert(payment.id);
			true
		});
		backend.extend(local.values().filter(|p| !covered.contains(&p.id)).map(|p| p.to_payment()));
		for id in pruned {
			local.remove(&id);
			let key = PaymentId::Trusted(id).to_string();
			if let Err(e) =
				KVStore::remove(self.store.as_ref(), PRIMARY, SECONDARY, &key, false).await
			{
				log_warn!(self.logger, "Failed to prune trusted payment record {key}: {e}");
			}
		}
		backend
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::logging::LoggerType;
	use ldk_node::io::sqlite_store::SqliteStore;

	fn store() -> Arc<dyn DynStore> {
		let path = std::env::temp_dir().join(format!(
			"orange-trusted-payments-{}",
			SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
		));
		Arc::new(SqliteStore::new(path, Some("payments.sqlite".to_owned()), None).unwrap())
	}

	fn logger() -> Arc<Logger> {
		Arc::new(Logger::new(&LoggerType::LogFacade).expect("logger"))
	}

	#[tokio::test]
	async fn submitted_and_failed_payments_survive_restart() {
		let store = store();
		let payments = PaymentStore::new(Arc::clone(&store), logger()).await.unwrap();
		let amount = Amount::from_sats(100).unwrap();
		payments.insert_pending([1; 32], amount, Some("quote-1".to_owned())).await.unwrap();
		payments.insert_pending([2; 32], amount, None).await.unwrap();
		assert!(payments.mark_failed([1; 32]).await.unwrap());
		assert!(!payments.mark_failed([1; 32]).await.unwrap());
		drop(payments);

		let payments = PaymentStore::new(store, logger()).await.unwrap();
		let history = payments.merge(vec![]).await;
		assert_eq!(history.len(), 2);
		assert!(history.iter().all(|p| p.outbound && p.amount == amount));
		assert_eq!(history.iter().find(|p| p.id == [1; 32]).unwrap().status, TxStatus::Failed);
		assert_eq!(history.iter().find(|p| p.id == [2; 32]).unwrap().status, TxStatus::Pending);
		assert_eq!(payments.pending().await, vec![([2; 32], None)]);
		assert!(!payments.insert_pending([2; 32], amount, None).await.unwrap());
		// A confirmed failure can be retried with the same backend ID.
		payments.insert_pending([1; 32], amount, Some("quote-1".to_owned())).await.unwrap();
		assert_eq!(payments.pending().await.len(), 2);
		assert!(payments.pending().await.contains(&([1; 32], Some("quote-1".to_owned()))));
		assert_eq!(payments.merge(vec![]).await.len(), 2);
	}

	#[tokio::test]
	async fn backend_completion_replaces_local_record_without_duplicates() {
		let store = store();
		let payments = PaymentStore::new(Arc::clone(&store), logger()).await.unwrap();
		let amount = Amount::from_sats(100).unwrap();
		payments.insert_pending([1; 32], amount, None).await.unwrap();
		payments.mark_failed([1; 32]).await.unwrap();
		let mut backend = Payment {
			id: [1; 32],
			amount,
			fee: Amount::from_sats(2).unwrap(),
			status: TxStatus::Pending,
			outbound: true,
			time_since_epoch: Duration::from_secs(1),
		};
		assert_eq!(payments.merge(vec![backend.clone()]).await[0].status, TxStatus::Failed);
		backend.status = TxStatus::Completed;
		let history = payments.merge(vec![backend.clone()]).await;
		assert_eq!(history.len(), 1);
		assert_eq!(history[0].status, TxStatus::Completed);
		assert_eq!(history[0].fee, backend.fee);
		assert_eq!(history[0].time_since_epoch, backend.time_since_epoch);

		// The backend now owns the record, so the local copy is pruned, also on disk.
		assert!(payments.merge(vec![]).await.is_empty());
		drop(payments);
		let payments = PaymentStore::new(store, logger()).await.unwrap();
		assert!(payments.merge(vec![]).await.is_empty());
	}

	#[tokio::test]
	async fn confirmed_success_is_listed_before_backend_catches_up() {
		let payments = PaymentStore::new(store(), logger()).await.unwrap();
		let amount = Amount::from_sats(100).unwrap();
		payments.insert_pending([1; 32], amount, None).await.unwrap();
		assert!(payments.mark_completed([1; 32]).await.unwrap());
		let history = payments.merge(vec![]).await;
		assert_eq!(history.len(), 1);
		assert_eq!(history[0].status, TxStatus::Completed);
		assert!(payments.pending().await.is_empty());
		// An outcome for a payment this store never saw still counts as new.
		assert!(payments.mark_completed([9; 32]).await.unwrap());
	}

	#[tokio::test]
	async fn merge_passes_backend_through_when_nothing_is_local() {
		let payments = PaymentStore::new(store(), logger()).await.unwrap();
		let amount = Amount::from_sats(100).unwrap();
		let backend = vec![Payment {
			id: [1; 32],
			amount,
			fee: Amount::ZERO,
			status: TxStatus::Pending,
			outbound: false,
			time_since_epoch: Duration::from_secs(1),
		}];
		let merged = payments.merge(backend.clone()).await;
		assert_eq!(merged.len(), 1);
		assert_eq!(merged[0].id, backend[0].id);
		assert!(!merged[0].outbound);
	}

	#[tokio::test]
	async fn invalid_record_is_skipped_on_load() {
		let store = store();
		KVStore::write(store.as_ref(), PRIMARY, SECONDARY, "garbage", vec![1, 2, 3]).await.unwrap();
		let payments = PaymentStore::new(Arc::clone(&store), logger()).await.unwrap();
		payments.insert_pending([1; 32], Amount::from_sats(1).unwrap(), None).await.unwrap();
		drop(payments);
		let payments = PaymentStore::new(store, logger()).await.unwrap();
		assert_eq!(payments.merge(vec![]).await.len(), 1);
	}
}
