use crate::Balances as OrangeBalances;

use crate::Wallet as OrangeWallet;
use crate::WalletConfig as OrangeWalletConfig;
use crate::ffi::bitcoin_payment_instructions::Amount;
use crate::ffi::orange::config::WalletConfig;
use crate::ffi::orange::error::{InitFailure, WalletError};
use crate::{impl_from_core_type, impl_into_core_type};
use std::sync::Arc;

/// Represents the balances of the wallet, including available and pending balances.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, uniffi::Object)]
pub struct Balances(pub OrangeBalances);

#[uniffi::export]
impl Balances {
	#[uniffi::constructor]
	pub fn new(lightning: Arc<Amount>, trusted: Arc<Amount>, pending_balance: Arc<Amount>) -> Self {
		Balances(OrangeBalances {
			lightning: lightning.0,
			trusted: trusted.0,
			pending_balance: pending_balance.0,
		})
	}

	/// Returns the lightning balance.
	pub fn lightning_balance(&self) -> Amount {
		self.0.lightning.into()
	}

	/// Returns the trusted balance.
	pub fn trusted_balance(&self) -> Amount {
		self.0.trusted.into()
	}

	/// Returns the pending balance.
	pub fn pending_balance(&self) -> Amount {
		self.0.pending_balance.into()
	}

	/// Returns the total available balance, which is the sum of the lightning and trusted balances.
	pub fn available_balance(&self) -> Amount {
		self.0.available_balance().into()
	}
}

impl_from_core_type!(OrangeBalances, Balances);
impl_into_core_type!(Balances, OrangeBalances);

#[derive(Clone, uniffi::Object)]
pub struct Wallet {
	inner: Arc<OrangeWallet>,
}

#[uniffi::export]
impl Wallet {
	#[uniffi::constructor]
	pub fn new(config: WalletConfig) -> Result<Self, InitFailure> {
		let rt = Arc::new(tokio::runtime::Builder::new_multi_thread().enable_all().build()?);

		let config: OrangeWalletConfig = config.try_into()?;

		let rt_clone = rt.clone();
		let inner =
			rt.block_on(async move { OrangeWallet::new_with_runtime(rt_clone, config).await })?;

		Ok(Wallet { inner: Arc::new(inner) })
	}

	pub fn node_id(&self) -> String {
		self.inner.node_id().to_string()
	}

	pub async fn get_balance(&self) -> Result<Balances, WalletError> {
		let balance = self.inner.get_balance().await?;
		Ok(balance.into())
	}

	pub async fn is_connected_to_lsp(&self) -> bool {
		self.inner.is_connected_to_lsp()
	}

	/// Sets whether the wallet should automatically rebalance from trusted/onchain to lightning.
	pub fn set_rebalance_enabled(&self, value: bool) {
		self.inner.set_rebalance_enabled(value)
	}

	/// Whether the wallet should automatically rebalance from trusted/onchain to lightning.
	pub fn get_rebalance_enabled(&self) -> bool {
		self.inner.get_rebalance_enabled()
	}

	pub async fn list_transactions(
		&self,
	) -> Result<Vec<std::sync::Arc<crate::ffi::orange::Transaction>>, WalletError> {
		let transactions = self.inner.list_transactions().await?;
		Ok(transactions.into_iter().map(|tx| std::sync::Arc::new(tx.into())).collect())
	}

	pub async fn stop(&self) {
		self.inner.stop().await
	}
}
