//! This module defines the `TrustedWalletInterface` trait and its associated types.

use crate::logging::Logger;
use crate::{InitFailure, TxStatus, WalletConfig};

use ldk_node::bitcoin::io;
use ldk_node::lightning::ln::msgs::DecodeError;
use ldk_node::lightning::util::ser::{Readable, Writeable, Writer};
use ldk_node::lightning_invoice::Bolt11Invoice;

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use spark_rust::error::SparkSdkError;

use spark_protos::spark::TransferStatus;

use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

#[cfg(feature = "_test-utils")]
pub mod dummy;
pub mod spark;

// todo spark uses uuids, should we force every other option to also use this?
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TrustedPaymentId(pub(crate) uuid::Uuid);
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
pub(crate) type Error = SparkSdkError;

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

impl From<TransferStatus> for TxStatus {
	fn from(o: TransferStatus) -> TxStatus {
		match o {
			TransferStatus::SenderInitiated
			| TransferStatus::SenderKeyTweakPending
			| TransferStatus::SenderKeyTweaked
			| TransferStatus::ReceiverKeyTweaked => TxStatus::Pending,
			TransferStatus::Completed => TxStatus::Completed,
			TransferStatus::Expired
			| TransferStatus::Returned
			| TransferStatus::TransferStatusrReceiverRefundSigned => TxStatus::Failed,
		}
	}
}

// todo i dont think we need send + sync
pub(crate) trait TrustedWalletInterface: Sized + Send + Sync {
	type ExtraConfig;

	fn init(
		config: &WalletConfig<Self::ExtraConfig>, logger: Arc<Logger>,
	) -> impl Future<Output = Result<Self, InitFailure>> + Send;

	fn get_balance(&self) -> Amount;

	fn get_reusable_receive_uri(&self) -> impl Future<Output = Result<String, Error>> + Send;

	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> impl Future<Output = Result<Bolt11Invoice, Error>> + Send;

	fn list_payments(&self) -> impl Future<Output = Result<Vec<Payment>, Error>> + Send;

	fn estimate_fee(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<Amount, Error>> + Send;

	fn pay(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<TrustedPaymentId, Error>> + Send;

	fn sync(&self) -> impl Future<Output = ()> + Send;
}
