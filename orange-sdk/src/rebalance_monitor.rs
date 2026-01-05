use crate::Rebalancer;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Monitor that bridges payment events to the rebalancer
///
/// This struct breaks the circular dependency by allowing wallets to notify
/// about payment events without needing a direct reference to the rebalancer.
/// The rebalancer will check internally if the payment is part of an active
/// rebalance and handle it accordingly.
#[derive(Clone)]
pub(crate) struct RebalanceMonitor {
	rebalancer: Arc<Rebalancer>,
}

/// A mutable holder for the RebalanceMonitor that can be set after initialization
pub(crate) type RebalanceMonitorHolder = Arc<Mutex<Option<RebalanceMonitor>>>;

impl RebalanceMonitor {
	pub(crate) fn new(rebalancer: Arc<Rebalancer>) -> Self {
		Self { rebalancer }
	}

	/// Notify that a trusted wallet payment has been sent
	pub(crate) async fn notify_trusted_payment_sent(
		&self, payment_hash: [u8; 32], fee_msat: Option<u64>,
	) {
		self.rebalancer.on_trusted_payment_sent(payment_hash, fee_msat).await;
	}

	/// Notify that a lightning wallet payment has been received
	pub(crate) async fn notify_ln_payment_received(
		&self, payment_hash: [u8; 32], payment_id: [u8; 32], fee_msat: Option<u64>,
	) {
		self.rebalancer.on_ln_payment_received(payment_hash, payment_id, fee_msat).await;
	}

	/// Notify that a trusted wallet payment has failed
	pub(crate) async fn notify_trusted_payment_failed(
		&self, payment_hash: [u8; 32], reason: String,
	) {
		self.rebalancer.on_trusted_payment_failed(payment_hash, reason).await;
	}
}
