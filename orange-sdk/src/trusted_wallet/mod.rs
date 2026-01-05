//! This module defines the `TrustedWalletInterface` trait and its associated types.

use crate::rebalance_monitor::RebalanceMonitorHolder;
use crate::store::TxStatus;

use ldk_node::lightning_invoice::Bolt11Invoice;

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "cashu")]
pub mod cashu;
#[cfg(feature = "_test-utils")]
pub mod dummy;
#[cfg(feature = "spark")]
pub mod spark;

/// Represents a payment with its associated details.
///
/// This struct contains information about a payment, including its unique ID,
/// amount, fee, status, and whether it is outbound or inbound.
#[derive(Debug, Clone)]
pub struct Payment {
	/// The unique identifier for the payment.
	pub id: [u8; 32],
	/// The amount of the payment.
	pub amount: Amount,
	/// The fee associated with the payment.
	pub fee: Amount,
	/// The current status of the payment (e.g., pending, completed, failed).
	pub status: TxStatus,
	/// Indicates whether the payment is outbound (`true`) or inbound (`false`).
	pub outbound: bool,
	/// The time the transaction was created
	pub time_since_epoch: Duration,
}

pub(crate) type DynTrustedWalletInterface = dyn TrustedWalletInterface + Send + Sync;

/// Represents a trait for a trusted wallet interface.
pub(crate) trait TrustedWalletInterface: Send + Sync + private::Sealed {
	/// Returns the current balance of the wallet.
	fn get_balance(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Amount, TrustedError>> + Send + '_>>;

	/// Generates a new reusable address for receiving payments.
	/// Generally, this should be a BOLT 12 offer.
	fn get_reusable_receive_uri(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<String, TrustedError>> + Send + '_>>;

	/// Generates a Bolt11 invoice for the specified amount.
	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> Pin<Box<dyn Future<Output = Result<Bolt11Invoice, TrustedError>> + Send + '_>>;

	/// Lists all payments made through the wallet.
	fn list_payments(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Vec<Payment>, TrustedError>> + Send + '_>>;

	/// Estimates the fee for a payment to the given payment method with the specified amount.
	fn estimate_fee(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<Amount, TrustedError>> + Send + '_>>;

	/// Pays to the given payment method with the specified amount. This should not await
	/// the result of the payment, it should only initiate it and return the payment ID.
	///
	/// This should later emit a [`PaymentSuccessful`] or [`PaymentFailed`] event
	/// when the payment is completed or failed.
	///
	/// [`PaymentSuccessful`]: `crate::event::Event::PaymentSuccessful`
	/// [`PaymentFailed`]: `crate::event::Event::PaymentFailed`
	fn pay(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<[u8; 32], TrustedError>> + Send + '_>>;

	/// Get the rebalance monitor holder for this wallet
	fn rebalance_monitor_holder(&self) -> RebalanceMonitorHolder;

	/// Stops the wallet, cleaning up any resources.
	/// This is typically used to gracefully shut down the wallet.
	fn stop(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

pub(crate) struct WalletTrusted<T: TrustedWalletInterface + ?Sized>(pub(crate) Arc<Box<T>>);

impl<T: ?Sized + TrustedWalletInterface> graduated_rebalancer::TrustedWallet for WalletTrusted<T> {
	type Error = TrustedError;

	fn get_balance(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Amount, Self::Error>> + Send + '_>> {
		Box::pin(async move { self.0.get_balance().await })
	}

	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> Pin<Box<dyn Future<Output = Result<Bolt11Invoice, Self::Error>> + Send + '_>> {
		Box::pin(async move { self.0.get_bolt11_invoice(amount).await })
	}

	fn pay(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<[u8; 32], Self::Error>> + Send + '_>> {
		Box::pin(async move { self.0.pay(method, amount).await })
	}
}

#[derive(Clone)]
/// Extra configuration needed for different types of wallets.
pub enum ExtraConfig {
	/// Configuration for Spark wallet.
	#[cfg(feature = "spark")]
	Spark(crate::SparkWalletConfig),
	/// Configuration for Cashu wallet.
	#[cfg(feature = "cashu")]
	Cashu(cashu::CashuConfig),
	/// Configuration for dummy wallet (test-only).
	#[cfg(feature = "_test-utils")]
	Dummy(dummy::DummyTrustedWalletExtraConfig),
}

/// Private module with a marker trait. This is to get around `private_bounds` errors while also
/// keeping the [`TrustedWalletInterface`] trait sealed. This is a common pattern in Rust to
/// prevent external code from implementing a trait. The `Sealed` trait is empty and is only used to
/// mark the types that are allowed to implement the [`TrustedWalletInterface`] trait.
mod private {
	pub trait Sealed {}

	// Only implement Sealed for types you want to allow
	#[cfg(feature = "spark")]
	impl Sealed for super::spark::Spark {}
	#[cfg(feature = "cashu")]
	impl Sealed for super::cashu::Cashu {}
	#[cfg(feature = "_test-utils")]
	impl Sealed for super::dummy::DummyTrustedWallet {}
}

/// An error type for the Spark wallet implementation.
#[derive(Debug)]
pub enum TrustedError {
	/// Not enough funds to complete the operation.
	InsufficientFunds,
	/// The wallet operation failed with a specific message.
	WalletOperationFailed(String),
	/// The provided network is invalid.
	InvalidNetwork,
	/// The spark wallet does not yet support the operation.
	UnsupportedOperation(String),
	/// Failed to convert an amount.
	AmountError,
	/// An I/O error occurred.
	IOError(ldk_node::lightning::io::Error),
	/// An unspecified error occurred.
	Other(String),
}

impl core::fmt::Display for TrustedError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
		write!(f, "{self:?}")
	}
}

impl From<ldk_node::lightning::io::Error> for TrustedError {
	fn from(e: ldk_node::lightning::io::Error) -> Self {
		TrustedError::IOError(e)
	}
}
