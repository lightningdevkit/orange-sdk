use crate::Balances as OrangeBalances;
use crate::SingleUseReceiveUri as OrangeSingleUseReceiveUri;
use crate::Wallet as OrangeWallet;
use crate::WalletConfig as OrangeWalletConfig;
use crate::ffi::bitcoin_payment_instructions::{
	Amount, ParseError, PaymentInfo, PaymentInstructions,
};
use crate::ffi::ldk_node::ChannelDetails;
use crate::ffi::orange::Event;
use crate::ffi::orange::config::{Tunables, WalletConfig};
use crate::ffi::orange::error::{InitFailure, WalletError};
use crate::{impl_from_core_type, impl_into_core_type};

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

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

// This is basically the `async-compat` utility that UniFFI uses under
// the hood for its `async_runtime = "tokio"` attribute, except it lets
// us use our own runtime to avoid the redundant runtimes implicit in
// the `async-compat` single-threaded runtime.
pin_project_lite::pin_project! {
	struct RTPoller<T> {
		#[pin]
		inner: T,
		rt: Arc<tokio::runtime::Runtime>,
	}
}

impl<T> RTPoller<T> {
	fn new(inner: T, rt: Arc<tokio::runtime::Runtime>) -> Self {
		Self { inner, rt }
	}
}

impl<T: Future> Future for RTPoller<T> {
	type Output = T::Output;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let _guard = self.rt.enter();
		self.project().inner.poll(cx)
	}
}

#[derive(Clone, uniffi::Object)]
pub struct Wallet {
	inner: Arc<OrangeWallet>,
	rt: Arc<tokio::runtime::Runtime>,
}

#[uniffi::export]
impl Wallet {
	#[uniffi::constructor]
	pub async fn new(config: WalletConfig) -> Result<Self, InitFailure> {
		let rt = Arc::new(tokio::runtime::Builder::new_multi_thread().enable_all().build()?);

		let config: OrangeWalletConfig = config.try_into()?;

		let inner = RTPoller::new(OrangeWallet::new(config), Arc::clone(&rt)).await?;

		Ok(Wallet { inner: Arc::new(inner), rt })
	}

	pub fn node_id(&self) -> String {
		self.inner.node_id().to_string()
	}

	pub async fn get_balance(&self) -> Result<Balances, WalletError> {
		let balance = RTPoller::new(self.inner.get_balance(), Arc::clone(&self.rt)).await?;
		Ok(balance.into())
	}

	pub fn is_connected_to_lsp(&self) -> bool {
		self.inner.is_connected_to_lsp()
	}

	/// Sets whether the wallet should automatically rebalance from trusted/onchain to lightning.
	pub async fn set_rebalance_enabled(&self, value: bool) {
		RTPoller::new(self.inner.set_rebalance_enabled(value), Arc::clone(&self.rt)).await
	}

	/// Whether the wallet should automatically rebalance from trusted/onchain to lightning.
	pub async fn get_rebalance_enabled(&self) -> bool {
		RTPoller::new(self.inner.get_rebalance_enabled(), Arc::clone(&self.rt)).await
	}

	pub async fn list_transactions(
		&self,
	) -> Result<Vec<std::sync::Arc<crate::ffi::orange::Transaction>>, WalletError> {
		let transactions =
			RTPoller::new(self.inner.list_transactions(), Arc::clone(&self.rt)).await?;
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
		let result = RTPoller::new(
			self.inner.parse_payment_instructions(&instructions),
			Arc::clone(&self.rt),
		)
		.await?;
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
	pub async fn pay(
		&self, payment_info: Arc<PaymentInfo>,
	) -> Result<super::PaymentId, WalletError> {
		let id = RTPoller::new(self.inner.pay(&payment_info.0), Arc::clone(&self.rt)).await?;
		Ok(id.into())
	}

	/// Estimates the fees required to pay using the provided PaymentInfo
	///
	/// This will estimate fees for the optimal payment method based on the current wallet state,
	/// including whether the payment would be routed through the trusted wallet or lightning wallet.
	pub async fn estimate_fee(
		&self, payment_info: Arc<PaymentInfo>,
	) -> Result<Arc<Amount>, WalletError> {
		let fee = RTPoller::new(
			self.inner.estimate_fee(&payment_info.0.instructions),
			Arc::clone(&self.rt),
		)
		.await;
		Ok(Arc::new(fee.into()))
	}

	pub async fn stop(&self) {
		RTPoller::new(self.inner.stop(), Arc::clone(&self.rt)).await
	}

	pub async fn get_single_use_receive_uri(
		&self, amount: Option<Arc<Amount>>,
	) -> Result<SingleUseReceiveUri, WalletError> {
		let uri = RTPoller::new(
			self.inner.get_single_use_receive_uri(amount.map(|a| a.0)),
			Arc::clone(&self.rt),
		)
		.await?;
		Ok(uri.into())
	}

	/// List our current channels
	pub fn list_channels(&self) -> Vec<Arc<ChannelDetails>> {
		self.inner.channels().into_iter().map(|ch| Arc::new(ch.into())).collect()
	}

	/// List our current channels
	pub async fn close_channels(&self) -> Result<(), WalletError> {
		RTPoller::new(self.inner.close_channels(), Arc::clone(&self.rt)).await?;
		Ok(())
	}

	/// Authenticates the user via [LNURL-auth] for the given LNURL string.
	///
	/// [LNURL-auth]: https://github.com/lnurl/luds/blob/luds/04.md
	// pub fn lnurl_auth(&self, lnurl: &str) -> Result<(), WalletError> {
	// 	self.inner.lnurl_auth(lnurl)?;
	// 	Ok(())
	// }

	/// Returns the wallet's configured tunables.
	pub fn get_tunables(&self) -> Arc<Tunables> {
		Arc::new(self.inner.get_tunables().into())
	}

	/// Returns the next event in the event queue, if currently available.
	///
	/// Will return `Some(...)` if an event is available and `None` otherwise.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`crate::Wallet::event_handled`].
	///
	/// **Caution:** Users must handle events as quickly as possible to prevent a large event backlog,
	/// which can increase the memory footprint of [`crate::Wallet`].
	pub fn next_event(&self) -> Option<Event> {
		self.inner.next_event().map(|e| e.into())
	}

	/// Returns the next event in the event queue.
	///
	/// Will asynchronously poll the event queue until the next event is ready.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`crate::Wallet::event_handled`].
	///
	/// **Caution:** Users must handle events as quickly as possible to prevent a large event backlog,
	/// which can increase the memory footprint of [`crate::Wallet`].
	pub async fn next_event_async(&self) -> Event {
		RTPoller::new(self.inner.next_event_async(), Arc::clone(&self.rt)).await.into()
	}

	/// Returns the next event in the event queue.
	///
	/// Will block the current thread until the next event is available.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`crate::Wallet::event_handled`].
	///
	/// **Caution:** Users must handle events as quickly as possible to prevent a large event backlog,
	/// which can increase the memory footprint of [`crate::Wallet`].
	pub fn wait_next_event(&self) -> Event {
		self.inner.wait_next_event().into()
	}

	/// Confirm the last retrieved event handled. Returns true if successful.
	///
	/// **Note:** This **MUST** be called after each event has been handled.
	pub fn event_handled(&self) -> bool {
		self.inner.event_handled().is_ok()
	}
}
