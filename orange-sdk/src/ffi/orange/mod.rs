use crate::bitcoin::hashes::Hash;
use crate::ffi::bitcoin_payment_instructions::Amount;
use crate::{
	PaymentId as OrangePaymentId, Transaction as OrangeTransaction, event::Event as OrangeEvent,
};
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
			OrangePaymentId::SelfCustodial(hash) => Self::Lightning(hash.to_vec()),
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

/// An event emitted by [`Wallet`], which should be handled by the user.
///
/// [`Wallet`]: [`crate::Wallet`]
#[derive(Debug, Clone, Eq, PartialEq, uniffi::Enum)]
pub enum Event {
	/// An outgoing payment was successful.
	PaymentSuccessful {
		/// A local identifier used to track the payment.
		payment_id: Arc<PaymentId>,
		/// The hash of the payment.
		payment_hash: Vec<u8>,
		/// The preimage to the `payment_hash`.
		///
		/// Note that this serves as a payment receipt.
		payment_preimage: Vec<u8>,
		/// The total fee which was spent at intermediate hops in this payment.
		fee_paid_msat: Option<u64>,
	},
	/// An outgoing payment has failed.
	PaymentFailed {
		/// A local identifier used to track the payment.
		payment_id: Arc<PaymentId>,
		/// The hash of the payment.
		///
		/// This will be `None` if the payment failed before receiving an invoice when paying a
		/// BOLT12 [`Offer`].
		///
		/// [`Offer`]: ldk_node::lightning::offers::offer::Offer
		payment_hash: Option<Vec<u8>>,
		/// The reason why the payment failed.
		///
		/// Will be `None` if the failure reason is not known.
		reason: Option<String>,
	},
	/// A payment has been received.
	PaymentReceived {
		/// A local identifier used to track the payment.
		payment_id: Arc<PaymentId>,
		/// The hash of the payment.
		payment_hash: Vec<u8>,
		/// The value, in msats, that has been received.
		amount_msat: u64,
		/// Custom TLV records received on the payment
		custom_records: Vec<Vec<u8>>,
		/// The value, in msats, that was skimmed off of this payment as an extra fee taken by LSP.
		/// Typically, this is only present for payments that result in opening a channel.
		lsp_fee_msats: Option<u64>,
	},
	/// A payment has been received.
	OnchainPaymentReceived {
		/// A local identifier used to track the payment.
		payment_id: Arc<PaymentId>,
		/// The transaction ID.
		txid: Vec<u8>,
		/// The value, in sats, that has been received.
		amount_sat: u64,
	},
	/// A channel is ready to be used.
	ChannelOpened {
		/// The `channel_id` of the channel.
		channel_id: Vec<u8>,
		/// The `user_channel_id` of the channel.
		user_channel_id: Vec<u8>,
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: Vec<u8>,
		/// The outpoint of the channel's funding transaction.
		funding_txo: String,
	},
	/// A channel has been closed.
	///
	/// When a channel is closed, we will disable automatic rebalancing
	/// so new channels will not be opened until it is explicitly enabled again.
	ChannelClosed {
		/// The `channel_id` of the channel.
		channel_id: Vec<u8>,
		/// The `user_channel_id` of the channel.
		user_channel_id: Vec<u8>,
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: Vec<u8>,
		/// Why the channel was closed.
		///
		/// Will be `None` if the closure reason is not known.
		reason: Option<String>,
	},
	/// A rebalance from our trusted wallet has been initiated.
	RebalanceInitiated {
		/// The `payment_id` of the transaction that triggered the rebalance.
		trigger_payment_id: Arc<PaymentId>,
		/// The `payment_id` of the rebalance payment sent from the trusted wallet.
		trusted_rebalance_payment_id: Vec<u8>,
		/// The amount, in msats, of the rebalance payment.
		amount_msat: u64,
	},
	/// A rebalance from our trusted wallet was successful.
	RebalanceSuccessful {
		/// The `payment_id` of the transaction that triggered the rebalance.
		trigger_payment_id: Arc<PaymentId>,
		/// The `payment_id` of the rebalance payment sent from the trusted wallet.
		trusted_rebalance_payment_id: Vec<u8>,
		/// The `payment_id` of the rebalance payment sent to the LN wallet.
		ln_rebalance_payment_id: Vec<u8>,
		/// The amount, in msats, of the rebalance payment.
		amount_msat: u64,
		/// The fee paid, in msats, for the rebalance payment.
		fee_msat: u64,
	},
}

impl From<OrangeEvent> for Event {
	fn from(value: OrangeEvent) -> Self {
		match value {
			OrangeEvent::PaymentSuccessful {
				payment_id,
				payment_hash,
				payment_preimage,
				fee_paid_msat,
			} => Event::PaymentSuccessful {
				payment_id: Arc::new(payment_id.into()),
				payment_hash: payment_hash.0.to_vec(),
				payment_preimage: payment_preimage.0.to_vec(),
				fee_paid_msat,
			},
			OrangeEvent::PaymentFailed { payment_id, payment_hash, reason } => {
				Event::PaymentFailed {
					payment_id: Arc::new(payment_id.into()),
					payment_hash: payment_hash.map(|h| h.0.to_vec()),
					reason: reason.map(|r| format!("{:?}", r)),
				}
			},
			OrangeEvent::PaymentReceived {
				payment_id,
				payment_hash,
				amount_msat,
				custom_records,
				lsp_fee_msats,
			} => Event::PaymentReceived {
				payment_id: Arc::new(payment_id.into()),
				payment_hash: payment_hash.0.to_vec(),
				amount_msat,
				custom_records: custom_records.into_iter().map(|r| r.value).collect(),
				lsp_fee_msats,
			},
			OrangeEvent::OnchainPaymentReceived { payment_id, txid, amount_sat, status: _ } => {
				Event::OnchainPaymentReceived {
					payment_id: Arc::new(payment_id.into()),
					txid: txid.to_byte_array().to_vec(),
					amount_sat,
				}
			},
			OrangeEvent::ChannelOpened {
				channel_id,
				user_channel_id,
				counterparty_node_id,
				funding_txo,
			} => Event::ChannelOpened {
				channel_id: channel_id.0.to_vec(),
				user_channel_id: user_channel_id.0.to_be_bytes().to_vec(),
				counterparty_node_id: counterparty_node_id.serialize().to_vec(),
				funding_txo: funding_txo.to_string(),
			},
			OrangeEvent::ChannelClosed {
				channel_id,
				user_channel_id,
				counterparty_node_id,
				reason,
			} => Event::ChannelClosed {
				channel_id: channel_id.0.to_vec(),
				user_channel_id: user_channel_id.0.to_be_bytes().to_vec(),
				counterparty_node_id: counterparty_node_id.serialize().to_vec(),
				reason: reason.map(|r| format!("{:?}", r)),
			},
			OrangeEvent::RebalanceInitiated {
				trigger_payment_id,
				trusted_rebalance_payment_id,
				amount_msat,
			} => Event::RebalanceInitiated {
				trigger_payment_id: Arc::new(trigger_payment_id.into()),
				trusted_rebalance_payment_id: trusted_rebalance_payment_id.to_vec(),
				amount_msat,
			},
			OrangeEvent::RebalanceSuccessful {
				trigger_payment_id,
				trusted_rebalance_payment_id,
				ln_rebalance_payment_id,
				amount_msat,
				fee_msat,
			} => Event::RebalanceSuccessful {
				trigger_payment_id: Arc::new(trigger_payment_id.into()),
				trusted_rebalance_payment_id: trusted_rebalance_payment_id.to_vec(),
				ln_rebalance_payment_id: ln_rebalance_payment_id.to_vec(),
				amount_msat,
				fee_msat,
			},
		}
	}
}
