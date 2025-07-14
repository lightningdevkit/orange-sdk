//! This module defines the `TrustedWalletInterface` trait and its associated types.

use crate::logging::Logger;
use crate::{InitFailure, TxStatus, WalletConfig};

use ldk_node::bitcoin::io;
use ldk_node::lightning::ln::msgs::DecodeError;
use ldk_node::lightning::util::ser::{Readable, Writeable, Writer};
use ldk_node::lightning_invoice::Bolt11Invoice;

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use spark_wallet::SparkWalletError;

use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

#[cfg(feature = "_test-utils")]
pub mod dummy;
pub mod spark;

/// Payment ID from a trusted wallet.
// todo spark uses uuids, should we force every other option to also use this?
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrustedPaymentId(pub(crate) uuid::Uuid);
impl Readable for TrustedPaymentId {
	fn read<R: io::Read>(r: &mut R) -> Result<Self, DecodeError> {
		Ok(TrustedPaymentId(uuid::Uuid::from_bytes(Readable::read(r)?)))
	}
}
impl Writeable for TrustedPaymentId {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), io::Error> {
		self.0.as_bytes().write(w)
	}
}
impl fmt::Display for TrustedPaymentId {
	fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		self.0.fmt(f)
	}
}
impl FromStr for TrustedPaymentId {
	type Err = <uuid::Uuid as FromStr>::Err;
	fn from_str(s: &str) -> Result<Self, <uuid::Uuid as FromStr>::Err> {
		Ok(TrustedPaymentId(uuid::Uuid::from_str(s)?))
	}
}

// todo generic error type
pub(crate) type Error = SparkWalletError;

/// Represents a payment with its associated details.
///
/// This struct contains information about a payment, including its unique ID,
/// amount, fee, status, and whether it is outbound or inbound.
#[derive(Debug, Clone)]
pub struct Payment {
	/// The unique identifier for the payment.
	pub id: TrustedPaymentId,
	/// The amount of the payment.
	pub amount: Amount,
	/// The fee associated with the payment.
	pub fee: Amount,
	/// The current status of the payment (e.g., pending, completed, failed).
	pub status: TxStatus,
	/// Indicates whether the payment is outbound (`true`) or inbound (`false`).
	pub outbound: bool,
}

// todo i dont think we need send + sync
/// Represents a trait for a trusted wallet interface.
pub trait TrustedWalletInterface: Sized + Send + Sync + private::Sealed {
	/// Extra configuration parameters for this type of wallet.
	type ExtraConfig;

	/// Initializes the wallet with the given configuration and logger.
	fn init(
		config: &WalletConfig<Self::ExtraConfig>, logger: Arc<Logger>,
	) -> impl Future<Output = Result<Self, InitFailure>> + Send;

	/// Returns the current balance of the wallet.
	fn get_balance(&self) -> impl Future<Output = Result<Amount, Error>> + Send;

	/// Generates a new reusable address for receiving payments.
	/// Generally, this should be a BOLT 12 offer.
	fn get_reusable_receive_uri(&self) -> impl Future<Output = Result<String, Error>> + Send;

	/// Generates a Bolt11 invoice for the specified amount.
	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> impl Future<Output = Result<Bolt11Invoice, Error>> + Send;

	/// Lists all payments made through the wallet.
	fn list_payments(&self) -> impl Future<Output = Result<Vec<Payment>, Error>> + Send;

	/// Estimates the fee for a payment to the given payment method with the specified amount.
	fn estimate_fee(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<Amount, Error>> + Send;

	/// Pays to the given payment method with the specified amount.
	fn pay(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<TrustedPaymentId, Error>> + Send;

	/// Sync the wallet.
	// todo this can be removed once we don't need it for spark, the wallet should handle this itself
	fn sync(&self) -> impl Future<Output = ()> + Send;
}

/// Private module with a marker trait. This is to get around `private_bounds` errors while also
/// keeping the [`TrustedWalletInterface`] trait sealed. This is a common pattern in Rust to
/// prevent external code from implementing a trait. The `Sealed` trait is empty and is only used to
/// mark the types that are allowed to implement the [`TrustedWalletInterface`] trait.
mod private {
	pub trait Sealed {}

	// Only implement Sealed for types you want to allow
	impl Sealed for super::spark::Spark {}
	#[cfg(feature = "_test-utils")]
	impl Sealed for super::dummy::DummyTrustedWallet {}
}
