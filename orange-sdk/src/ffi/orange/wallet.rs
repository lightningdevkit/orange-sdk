use crate::Balances as OrangeBalances;

use crate::SingleUseReceiveUri as OrangeSingleUseReceiveUri;
use crate::Wallet as OrangeWallet;
use crate::WalletConfig as OrangeWalletConfig;
use crate::ffi::bitcoin_payment_instructions::{
	Amount, ParseError, PaymentInfo, PaymentInstructions,
};
use crate::ffi::ldk_node::ChannelDetails;
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

/// Represents a single-use Bitcoin URI for receiving payments.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Object)]
pub struct SingleUseReceiveUri(pub OrangeSingleUseReceiveUri);

#[uniffi::export]
impl SingleUseReceiveUri {
	pub fn address(&self) -> Option<String> {
		self.0.address.clone().map(|a| a.to_string())
	}

	pub fn invoice(&self) -> String {
		self.0.invoice.to_string()
	}

	pub fn amount(&self) -> Option<Arc<Amount>> {
		self.0.amount.map(|a| Arc::new(a.into()))
	}

	pub fn from_trusted(&self) -> bool {
		self.0.from_trusted
	}

	pub fn bip21_uri(&self) -> String {
		self.0.to_string()
	}

	pub fn payment_hash(&self) -> String {
		self.0.invoice.payment_hash().to_string()
	}
}

impl_from_core_type!(OrangeSingleUseReceiveUri, SingleUseReceiveUri);
impl_into_core_type!(SingleUseReceiveUri, OrangeSingleUseReceiveUri);

#[derive(Clone, uniffi::Object)]
pub struct Wallet {
	inner: Arc<OrangeWallet>,
}

#[uniffi::export(async_runtime = "tokio")]
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

	/// Parse payment instructions from a string (e.g., BOLT 11 invoice, BOLT 12 offer, on-chain address)
	///
	/// This method supports parsing various payment instruction formats including:
	/// - Lightning BOLT 11 invoices
	/// - Lightning BOLT 12 offers
	/// - On-chain Bitcoin addresses
	/// - BIP 21 URIs
	pub async fn parse_payment_instructions(
		&self, instructions: String,
	) -> Result<PaymentInstructions, ParseError> {
		let result = self.inner.parse_payment_instructions(&instructions).await?;
		Ok(result.into())
	}

	/// Initiate a payment using the provided PaymentInfo
	///
	/// This will attempt to pay from the trusted wallet if possible, otherwise it will pay
	/// from the lightning wallet. The function will also initiate automatic rebalancing
	/// from trusted to lightning wallet based on the resulting balance and configured tunables.
	///
	/// Returns once the payment is pending. This does not mean the payment has completed -
	/// it may still fail. Use event handlers or transaction listing to monitor payment status.
	pub async fn pay(&self, payment_info: Arc<PaymentInfo>) -> Result<(), WalletError> {
		self.inner.pay(&payment_info.0).await?;
		Ok(())
	}

	pub async fn stop(&self) {
		self.inner.stop().await
	}

	pub async fn get_single_use_receive_uri(
		&self, amount: Option<Arc<Amount>>,
	) -> Result<SingleUseReceiveUri, WalletError> {
		let uri = self.inner.get_single_use_receive_uri(amount.map(|a| a.0)).await?;
		Ok(uri.into())
	}

	/// List our current channels
	pub fn list_channels(&self) -> Vec<Arc<ChannelDetails>> {
		self.inner.channels().into_iter().map(|ch| Arc::new(ch.into())).collect()
	}
}
