use graduated_rebalancer::{RebalanceReceipt, RebalanceWait, ReceivedLightningPayment};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::oneshot;

struct PendingRebalance {
	lightning: Option<ReceivedLightningPayment>,
	trusted: Option<ReceivedLightningPayment>,
	sender: oneshot::Sender<Option<RebalanceReceipt>>,
}

/// One watcher per rebalance, shared by the Lightning and trusted event handlers.
#[derive(Default)]
pub(crate) struct RebalanceWatchers {
	pending: Mutex<HashMap<[u8; 32], PendingRebalance>>,
}

impl RebalanceWatchers {
	/// Register before sending so neither leg can complete before its watcher exists.
	/// A second registration for an active hash fails without replacing the first.
	pub(crate) fn register(&self, payment_hash: [u8; 32]) -> RebalanceWait {
		let mut pending = self.pending.lock().unwrap();
		pending.retain(|_, rebalance| !rebalance.sender.is_closed());
		if pending.contains_key(&payment_hash) {
			return Box::pin(async { None });
		}
		let (sender, receiver) = oneshot::channel();
		pending.insert(payment_hash, PendingRebalance { lightning: None, trusted: None, sender });
		Box::pin(async move { receiver.await.ok().flatten() })
	}

	pub(crate) fn received(
		&self, payment_hash: [u8; 32], receipt: Option<ReceivedLightningPayment>,
	) {
		self.update(payment_hash, receipt, |rebalance| &mut rebalance.lightning);
	}

	pub(crate) fn sent(&self, payment_hash: [u8; 32], receipt: Option<ReceivedLightningPayment>) {
		self.update(payment_hash, receipt, |rebalance| &mut rebalance.trusted);
	}

	fn update(
		&self, payment_hash: [u8; 32], receipt: Option<ReceivedLightningPayment>,
		leg: impl FnOnce(&mut PendingRebalance) -> &mut Option<ReceivedLightningPayment>,
	) {
		let mut pending = self.pending.lock().unwrap();
		let Some(rebalance) = pending.get_mut(&payment_hash) else { return };
		let payment = leg(rebalance);
		// A replay of one leg must not complete the other leg or change its result.
		if payment.is_some() {
			return;
		}
		if let Some(receipt) = receipt {
			*payment = Some(receipt);
			if rebalance.lightning.is_none() || rebalance.trusted.is_none() {
				return;
			}
		}
		let rebalance = pending.remove(&payment_hash).unwrap();
		let result = rebalance
			.lightning
			.zip(rebalance.trusted)
			.map(|(lightning, trusted)| RebalanceReceipt { lightning, trusted });
		let _ = rebalance.sender.send(result);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::future::poll_fn;
	use std::task::Poll;

	fn receipt(id: u8) -> Option<ReceivedLightningPayment> {
		Some(ReceivedLightningPayment { id: [id; 32], fee_paid_msat: Some(id.into()) })
	}

	#[tokio::test]
	async fn both_legs_are_required_in_either_order_and_before_polling() {
		for receive_first in [false, true] {
			let watchers = RebalanceWatchers::default();
			let mut wait = watchers.register([1; 32]);
			if receive_first {
				watchers.received([1; 32], receipt(11));
				watchers.received([1; 32], receipt(99));
			} else {
				watchers.sent([1; 32], receipt(22));
				watchers.sent([1; 32], receipt(99));
			}
			assert!(poll_fn(|cx| Poll::Ready(wait.as_mut().poll(cx))).await.is_pending());
			if receive_first {
				watchers.sent([1; 32], receipt(22));
			} else {
				watchers.received([1; 32], receipt(11));
			}
			let result = wait.await.unwrap();
			assert_eq!(result.lightning.id, [11; 32]);
			assert_eq!(result.lightning.fee_paid_msat, Some(11));
			assert_eq!(result.trusted.id, [22; 32]);
			assert_eq!(result.trusted.fee_paid_msat, Some(22));
			assert!(watchers.pending.lock().unwrap().is_empty());
		}
	}

	#[tokio::test]
	async fn either_failure_finishes_without_affecting_other_rebalances() {
		for receive_fails in [false, true] {
			for other_succeeded in [false, true] {
				let watchers = RebalanceWatchers::default();
				let failed = watchers.register([1; 32]);
				let success = watchers.register([2; 32]);
				watchers.sent([2; 32], receipt(22));
				if receive_fails {
					if other_succeeded {
						watchers.sent([1; 32], receipt(33));
					}
					watchers.received([1; 32], None);
				} else {
					if other_succeeded {
						watchers.received([1; 32], receipt(44));
					}
					watchers.sent([1; 32], None);
				}
				assert!(failed.await.is_none());
				watchers.received([2; 32], receipt(11));
				assert_eq!(success.await.unwrap().trusted.id, [22; 32]);
				assert!(watchers.pending.lock().unwrap().is_empty());
			}
		}
	}

	#[tokio::test]
	async fn duplicate_registration_preserves_the_active_watcher() {
		let watchers = RebalanceWatchers::default();
		let wait = watchers.register([1; 32]);
		assert!(watchers.register([1; 32]).await.is_none());
		watchers.sent([1; 32], receipt(22));
		watchers.received([1; 32], receipt(11));
		assert!(wait.await.is_some());
	}

	#[test]
	fn cancelled_and_unrequested_results_are_discarded() {
		let watchers = RebalanceWatchers::default();
		drop(watchers.register([1; 32]));
		let _active = watchers.register([2; 32]);
		watchers.received([3; 32], receipt(33));
		let pending = watchers.pending.lock().unwrap();
		assert_eq!(pending.len(), 1);
		assert!(pending.contains_key(&[2; 32]));
	}
}
