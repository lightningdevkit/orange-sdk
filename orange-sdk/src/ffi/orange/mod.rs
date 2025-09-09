use crate::bitcoin::hashes::Hash;
use crate::ffi::bitcoin_payment_instructions::Amount;
use crate::{PaymentId as OrangePaymentId, Transaction as OrangeTransaction};
use crate::{impl_from_core_type, impl_into_core_type};
use std::sync::Arc;

pub mod config;
pub mod error;
pub mod wallet;

/// A transaction is a record of a payment made or received. It contains information about the
/// transaction, such as the amount, fee, and status. It is used to track the state of a payment
/// and to provide information about the payment to the user.
#[derive(Debug, Clone, uniffi::Object)]
pub struct Transaction(pub crate::store::Transaction);

impl_from_core_type!(OrangeTransaction, Transaction);
impl_into_core_type!(Transaction, OrangeTransaction);

#[uniffi::export]
impl Transaction {
	pub fn id(&self) -> PaymentId {
		self.0.id.into()
	}

	pub fn status(&self) -> TxStatus {
		self.0.status.into()
	}

	pub fn outbound(&self) -> bool {
		self.0.outbound
	}

	/// The amount of the payment
	///
	/// None if the payment is not yet completed
	pub fn amount(&self) -> Option<Arc<Amount>> {
		self.0.amount.map(|a| Arc::new(a.into()))
	}

	/// The fee paid for the payment
	///
	/// None if the payment is not yet completed
	pub fn fee(&self) -> Option<Arc<Amount>> {
		self.0.fee.map(|a| Arc::new(a.into()))
	}

	/// The timestamp of the transaction
	pub fn secs_since_epoch(&self) -> u64 {
		self.0.time_since_epoch.as_secs()
	}

	/// The type of payment being made
	pub fn payment_type(&self) -> PaymentType {
		self.0.payment_type.clone().into()
	}
}

/// A PaymentId is a unique identifier for a payment. It can be either a Lightning payment or a
/// Trusted payment. It is used to track the state of a payment and to provide information about
/// the payment to the user.
#[derive(Debug, Clone, Hash, PartialEq, Eq, uniffi::Object)]
pub enum PaymentId {
	Lightning(Vec<u8>),
	Trusted(Vec<u8>),
}

impl From<OrangePaymentId> for PaymentId {
	fn from(id: OrangePaymentId) -> Self {
		match id {
			OrangePaymentId::Lightning(hash) => Self::Lightning(hash.to_vec()),
			OrangePaymentId::Trusted(hash) => Self::Trusted(hash.to_vec()),
		}
	}
}

/// The status of a transaction. This is used to track the state of a transaction
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TxStatus {
	/// A pending transaction has not yet been paid.
	Pending,
	/// A completed transaction has been paid.
	Completed,
	/// A transaction that has failed.
	Failed,
}

impl From<crate::store::TxStatus> for TxStatus {
	fn from(status: crate::store::TxStatus) -> Self {
		match status {
			crate::store::TxStatus::Pending => TxStatus::Pending,
			crate::store::TxStatus::Completed => TxStatus::Completed,
			crate::store::TxStatus::Failed => TxStatus::Failed,
		}
	}
}

/// The type of payment being made. This indicates whether the payment is incoming or outgoing,
/// and what method is being used (Lightning, on-chain, etc.)
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PaymentType {
	/// An outgoing Lightning payment paying a BOLT 12 offer.
	OutgoingLightningBolt12 {
		/// The lightning "payment preimage" which represents proof that the payment completed.
		/// Will be set for any completed payments.
		payment_preimage: Option<Vec<u8>>,
	},
	/// An outgoing Lightning payment paying a BOLT 11 invoice.
	OutgoingLightningBolt11 {
		/// The lightning "payment preimage" which represents proof that the payment completed.
		/// Will be set for any completed payments.
		payment_preimage: Option<Vec<u8>>,
	},
	/// An outgoing on-chain Bitcoin payment.
	OutgoingOnChain {
		/// The optional transaction ID of the on-chain payment.
		/// Will be set for any completed payments.
		txid: Option<Vec<u8>>,
	},
	/// An incoming on-chain Bitcoin payment.
	IncomingOnChain {
		/// The optional transaction ID of the on-chain payment.
		/// Will be set for any completed payments.
		txid: Option<Vec<u8>>,
	},
	/// An incoming Lightning payment.
	IncomingLightning {},
	/// A payment that is internal to the trusted service.
	TrustedInternal {},
}

impl From<crate::store::PaymentType> for PaymentType {
	fn from(payment_type: crate::store::PaymentType) -> Self {
		match payment_type {
			crate::store::PaymentType::OutgoingLightningBolt12 { payment_preimage } => {
				PaymentType::OutgoingLightningBolt12 {
					payment_preimage: payment_preimage.map(|p| p.0.to_vec()),
				}
			},
			crate::store::PaymentType::OutgoingLightningBolt11 { payment_preimage } => {
				PaymentType::OutgoingLightningBolt11 {
					payment_preimage: payment_preimage.map(|p| p.0.to_vec()),
				}
			},
			crate::store::PaymentType::OutgoingOnChain { txid } => {
				PaymentType::OutgoingOnChain { txid: txid.map(|t| t.to_byte_array().to_vec()) }
			},
			crate::store::PaymentType::IncomingOnChain { txid } => {
				PaymentType::IncomingOnChain { txid: txid.map(|t| t.to_byte_array().to_vec()) }
			},
			crate::store::PaymentType::IncomingLightning {} => PaymentType::IncomingLightning {},
			crate::store::PaymentType::TrustedInternal {} => PaymentType::TrustedInternal {},
		}
	}
}
