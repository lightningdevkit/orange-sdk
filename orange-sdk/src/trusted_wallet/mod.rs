//! Trusted wallet backends for the graduated custody model.
//!
//! This module defines the [`TrustedWalletInterface`] trait and provides concrete
//! implementations for different custodial backends:
//!
//! - **`spark`** (feature `spark`, enabled by default) – Uses the [Breez Spark SDK](https://breez.technology)
//!   for custodial Lightning payments with low fees and instant settlement.
//! - **`cashu`** (feature `cashu`) – Uses the [Cashu Development Kit (CDK)](https://docs.rs/cdk)
//!   for ecash-based custody via a Cashu mint. Supports [npub.cash](https://npub.cash) for
//!   Lightning address integration.
//! - **`dummy`** (feature `_test-utils`) – A test-only in-memory implementation.
//!
//! The trusted wallet holds small balances for instant, low-fee payments while the
//! [`Wallet`](crate::Wallet) automatically moves larger amounts into self-custodial
//! Lightning channels via the rebalancer.

use crate::store::TxStatus;

use ldk_node::lightning_invoice::Bolt11Invoice;

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use graduated_rebalancer::ReceivedLightningPayment;

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

/// The interface that all trusted wallet backends must implement.
///
/// This trait is **sealed** — it cannot be implemented outside of this crate. The available
/// implementations are `Spark` (feature `spark`), `Cashu` (feature `cashu`), and
/// `DummyTrustedWallet` (feature `_test-utils`, test-only).
///
/// Users don't interact with this trait directly. Instead, choose a backend via
/// [`ExtraConfig`] when building a [`WalletConfig`](crate::WalletConfig), and the
/// [`Wallet`](crate::Wallet) handles dispatching internally.
pub trait TrustedWalletInterface: Send + Sync + private::Sealed {
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

	/// Waits for a payment with the given payment hash to succeed.
	/// Returns the `ReceivedLightningPayment` if successful, or `None` if it fails or times out.
	fn await_payment_success(
		&self, payment_hash: [u8; 32],
	) -> Pin<Box<dyn Future<Output = Option<ReceivedLightningPayment>> + Send + '_>>;

	/// Gets the lightning address for this wallet, if one is set.
	fn get_lightning_address(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Option<String>, TrustedError>> + Send + '_>>;

	/// Attempts to register the lightning address for this wallet.
	fn register_lightning_address(
		&self, name: String,
	) -> Pin<Box<dyn Future<Output = Result<(), TrustedError>> + Send + '_>>;

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

	fn await_payment_success(
		&self, payment_hash: [u8; 32],
	) -> Pin<Box<dyn Future<Output = Option<ReceivedLightningPayment>> + Send + '_>> {
		Box::pin(async move { self.0.await_payment_success(payment_hash).await })
	}
}

/// Selects and configures the trusted wallet backend.
///
/// Pass one of these variants in [`WalletConfig::extra_config`](crate::WalletConfig::extra_config)
/// to choose which custodial backend the wallet uses:
///
/// - `Spark` – Breez Spark SDK (requires feature `spark`, enabled by default)
/// - `Cashu` – Cashu ecash via CDK (requires feature `cashu`)
/// - `Dummy` – in-memory test backend (requires feature `_test-utils`)
#[derive(Clone)]
pub enum ExtraConfig {
	/// Use the Spark backend. See [`SparkWalletConfig`](crate::SparkWalletConfig) for options.
	#[cfg(feature = "spark")]
	Spark(crate::SparkWalletConfig),
	/// Use the Cashu ecash backend. See [`CashuConfig`](cashu::CashuConfig) for options.
	#[cfg(feature = "cashu")]
	Cashu(cashu::CashuConfig),
	/// Use the in-memory dummy backend (test-only).
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

/// Errors from trusted wallet backend operations.
///
/// Any of the trusted backends (Spark, Cashu, Dummy) may return these errors. They are
/// surfaced to the caller as [`WalletError::TrustedFailure`](crate::WalletError::TrustedFailure)
/// or [`InitFailure::TrustedFailure`](crate::InitFailure::TrustedFailure).
#[derive(Debug)]
pub enum TrustedError {
	/// The wallet does not have enough funds to complete the operation.
	InsufficientFunds,
	/// A backend-specific operation failed.
	WalletOperationFailed(String),
	/// The configured Bitcoin network does not match the backend's network.
	InvalidNetwork,
	/// The requested operation is not supported by this backend.
	UnsupportedOperation(String),
	/// An amount conversion error (e.g. overflow or invalid unit).
	AmountError,
	/// An I/O error occurred during a storage or network operation.
	IOError(ldk_node::lightning::io::Error),
	/// An unspecified error with a descriptive message.
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
