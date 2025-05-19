//! A library implementing the full backend for a modern, highly usable, Bitcoin wallet focusing on
//! maximizing security and self-custody without trading off user experience.
//!
//! This crate should do everything you need to build a great Bitcoin wallet, except the UI.
//!
//! In order to maximize the user experience, small balances are held in a trusted service (XXX
//! which one), avoiding expensive setup fees, while larger balances are moved into on-chain
//! lightning channels, ensuring trust is minimized in the trusted service.
//!
//! Despite funds being stored in multiple places, the full balance can be treated as a single
//! wallet - payments can draw on both balances simultaneously and deposits are automatically
//! shifted to minimize fees and ensure maximal security.

use bitcoin_payment_instructions::amount::Amount;

use ldk_node::bitcoin::Txid;
use ldk_node::bitcoin::hex::{DisplayHex, FromHex};
use ldk_node::lightning::types::payment::PaymentPreimage;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::util::ser::{Readable, Writeable};
use ldk_node::lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};

use crate::trusted_wallet::TrustedPaymentId;

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;

const STORE_PRIMARY_KEY: &str = "orange_sdk";
const STORE_SECONDARY_KEY: &str = "payment_store";

/// The status of a transaction. This is used to track the state of a transaction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStatus {
	/// A pending transaction has not yet been paid.
	Pending,
	/// A completed transaction has been paid.
	Completed,
	/// A transaction that has failed.
	Failed,
}

/// A transaction is a record of a payment made or received. It contains information about the
/// transaction, such as the amount, fee, and status. It is used to track the state of a payment
/// and to provide information about the payment to the user.
#[derive(Debug, Clone)]
pub struct Transaction {
	/// The transaction status, either (Pending, Completed, or Failed)
	pub status: TxStatus,
	/// Indicates whether the payment is outbound (`true`) or inbound (`false`).
	pub outbound: bool,
	/// The amount of the payment
	///
	/// None if the payment is not yet completed
	pub amount: Option<Amount>,
	/// The fee paid for the payment
	///
	/// None if the payment is not yet completed
	pub fee: Option<Amount>,
	/// Represents the type of payment, including its method and associated metadata.
	pub payment_type: PaymentType,
	/// The time the transaction was created
	pub time_since_epoch: Duration,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) enum PaymentId {
	Lightning([u8; 32]),
	Trusted(TrustedPaymentId),
}

impl fmt::Display for PaymentId {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		match self {
			PaymentId::Lightning(bytes) => write!(fmt, "LN-{}", bytes.as_hex()),
			PaymentId::Trusted(s) => write!(fmt, "TR-{}", s),
		}
	}
}

impl FromStr for PaymentId {
	type Err = ();
	fn from_str(s: &str) -> Result<PaymentId, ()> {
		if s.len() < 4 {
			return Err(());
		}
		match &s[..3] {
			"LN-" => {
				let id = FromHex::from_hex(&s[3..]).map_err(|_| ())?;
				Ok(PaymentId::Lightning(id))
			},
			"TR-" => {
				let id = TrustedPaymentId::from_str(&s[3..]).map_err(|_| ())?;
				Ok(PaymentId::Trusted(id))
			},
			_ => Err(()),
		}
	}
}

impl_writeable_tlv_based_enum!(PaymentId,
	{0, Lightning} => (),
	{1, Trusted} => (),
);

/// Represents the type of payment, including its method and associated metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentType {
	/// An outgoing Lightning payment paying a BOLT 12 offer.
	///
	/// This type of payment includes a payment preimage, which serves as proof
	/// that the payment was completed. The preimage will be set for any
	/// [`TxStatus::Completed`] payments.
	OutgoingLightningBolt12 {
		/// The lightning "payment preimage" which represents proof that the payment completed.
		/// Will be set for any [`TxStatus::Completed`] payments.
		payment_preimage: Option<PaymentPreimage>,
		//offer: Offer,
		// TODO PoP
	},
	/// An outgoing Lightning payment paying a BOLT 11 invoice.
	///
	/// This type of payment includes a payment preimage, which serves as proof
	/// that the payment was completed. The preimage will be set for any
	/// [`TxStatus::Completed`] payments.
	OutgoingLightningBolt11 {
		/// The lightning "payment preimage" which represents proof that the payment completed.
		/// Will be set for any [`TxStatus::Completed`] payments.
		payment_preimage: Option<PaymentPreimage>,
		//invoice: Bolt11Invoice,
	},
	/// An outgoing on-chain Bitcoin payment.
	///
	/// This type of payment includes an optional transaction ID (`txid`) that
	/// identifies the on-chain transaction. This will be set for any
	/// [`TxStatus::Completed`] payments.
	OutgoingOnChain {
		/// The optional transaction ID of the on-chain payment.
		/// Will be set for any [`TxStatus::Completed`] payments.
		txid: Option<Txid>,
	},
	/// An incoming on-chain Bitcoin payment.
	///
	/// This type of payment includes an optional transaction ID (`txid`) that
	/// identifies the on-chain transaction. This will be set for any
	/// [`TxStatus::Completed`] payments.
	IncomingOnChain {
		/// The optional transaction ID of the on-chain payment.
		/// Will be set for any [`TxStatus::Completed`] payments.
		txid: Option<Txid>,
	},
	/// An incoming Lightning payment.
	///
	/// This type of payment is used for Lightning payments that are received.
	IncomingLightning {
		// TODO: Give all payment instructions an id so that incoming can get matched
	},
}

impl_writeable_tlv_based_enum!(PaymentType,
	(0, OutgoingLightningBolt12) => { (0, payment_preimage, option), },
	(1, OutgoingLightningBolt11) => { (0, payment_preimage, option), },
	(2, OutgoingOnChain) => { (0, txid, option), },
	(3, IncomingOnChain) => { (0, txid, option) },
	(4, IncomingLightning) => { },

);

#[derive(Debug, Clone)]
pub(crate) enum TxType {
	TransferToNonTrusted {
		trusted_payment: TrustedPaymentId,
		lightning_payment: [u8; 32],
		payment_triggering_transfer: PaymentId,
	},
	PaymentTriggeringTransferToNonTrusted {
		// TODO: We should remove `ty` once we get the info we need from the trusted end
		ty: PaymentType,
	},
	Payment {
		// TODO: We should remove `ty` once we get the info we need from the trusted end
		ty: PaymentType,
	},
}

impl_writeable_tlv_based_enum!(TxType,
	(0, TransferToNonTrusted) => {
		(0, trusted_payment, required),
		(2, lightning_payment, required),
		(4, payment_triggering_transfer, required),
	},
	(1, PaymentTriggeringTransferToNonTrusted) => { (0, ty, required), },
	(2, Payment) => { (0, ty, required), },
);

#[derive(Clone, Debug)]
pub(crate) struct TxMetadata {
	// TODO: We should remove `time` once we get the info we need from the trusted end
	pub(crate) time: Duration,
	pub(crate) ty: TxType,
}

impl_writeable_tlv_based!(TxMetadata, { (0, ty, required), (2, time, required) });

pub(crate) struct TxMetadataStore {
	tx_metadata: RwLock<HashMap<PaymentId, TxMetadata>>,
	store: Arc<dyn KVStore + Send + Sync>,
}

impl TxMetadataStore {
	pub fn new(store: Arc<dyn KVStore + Send + Sync>) -> TxMetadataStore {
		let keys = store
			.list(STORE_PRIMARY_KEY, STORE_SECONDARY_KEY)
			.expect("We do not allow reads to fail");
		let mut tx_metadata = HashMap::with_capacity(keys.len());
		for key in keys {
			let data_bytes = store
				.read(STORE_PRIMARY_KEY, STORE_SECONDARY_KEY, &key)
				.expect("We do not allow reads to fail");
			let key =
				PaymentId::from_str(&key).expect("Invalid key in transaction metadata storage");
			let data = Readable::read(&mut &data_bytes[..])
				.expect("Invalid data in transaction metadata storage");
			tx_metadata.insert(key, data);
		}
		TxMetadataStore { store, tx_metadata: RwLock::new(tx_metadata) }
	}

	pub fn read(&self) -> RwLockReadGuard<HashMap<PaymentId, TxMetadata>> {
		self.tx_metadata.read().unwrap()
	}

	fn do_set(&self, key: PaymentId, value: TxMetadata) -> bool {
		let mut tx_metadata = self.tx_metadata.write().unwrap();
		let key_str = key.to_string();
		let ser = value.encode();
		let old = tx_metadata.insert(key, value);
		self.store
			.write(STORE_PRIMARY_KEY, STORE_SECONDARY_KEY, &key_str, &ser)
			.expect("We do not allow writes to fail");
		old.is_some()
	}

	pub fn upsert(&self, key: PaymentId, value: TxMetadata) {
		self.do_set(key, value);
	}

	pub fn insert(&self, key: PaymentId, value: TxMetadata) {
		let had_old = self.do_set(key, value);
		debug_assert!(!had_old);
	}

	pub fn set_tx_caused_rebalance(&self, payment_id: &PaymentId) -> Result<(), ()> {
		let mut tx_metadata = self.tx_metadata.write().unwrap();
		if let Some(metadata) = tx_metadata.get_mut(payment_id) {
			if let TxType::Payment { ty } = &mut metadata.ty {
				metadata.ty = TxType::PaymentTriggeringTransferToNonTrusted { ty: ty.clone() };
				let key_str = payment_id.to_string();
				let ser = metadata.encode();
				self.store
					.write(STORE_PRIMARY_KEY, STORE_SECONDARY_KEY, &key_str, &ser)
					.expect("We do not allow writes to fail");
				Ok(())
			} else {
				Err(())
			}
		} else {
			Err(())
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::str::FromStr;

	#[test]
	fn test_payment_id_round_trip() {
		let ln_id_bytes = [
			1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
			25, 26, 27, 28, 29, 30, 31, 32,
		];
		let ln_id = PaymentId::Lightning(ln_id_bytes);
		let ln_id_str = ln_id.to_string();
		assert_eq!(
			ln_id_str,
			"LN-0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20"
		);
		let parsed_ln_id = PaymentId::from_str(&ln_id_str).unwrap();
		assert_eq!(ln_id, parsed_ln_id);

		let trusted_uuid = uuid::Uuid::new_v4();
		let trusted_id = PaymentId::Trusted(TrustedPaymentId(trusted_uuid));
		let trusted_id_str = trusted_id.to_string();
		assert_eq!(trusted_id_str, format!("TR-{trusted_uuid}"));
		let parsed_trusted_id = PaymentId::from_str(&trusted_id_str).unwrap();
		assert_eq!(trusted_id, parsed_trusted_id);

		// Test invalid formats
		assert!(PaymentId::from_str("INVALID").is_err());
		assert!(PaymentId::from_str("LN-invalidhex").is_err());
		assert!(PaymentId::from_str("TR-invaliduuid").is_err());
		assert!(PaymentId::from_str("XX-something").is_err());
	}
}
