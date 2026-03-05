//! Transaction metadata storage and types.
//!
//! This module defines the public types used to represent transactions ([`Transaction`],
//! [`PaymentId`], [`TxStatus`], [`PaymentType`]) and the internal storage layer
//! ([`TxMetadataStore`]) that tracks payment metadata across both trusted and self-custodial
//! wallets.

use bitcoin_payment_instructions::amount::Amount;

use ldk_node::DynStore;
use ldk_node::bitcoin::Txid;
use ldk_node::bitcoin::hex::{DisplayHex, FromHex};
use ldk_node::lightning::io;
use ldk_node::lightning::ln::msgs::DecodeError;
use ldk_node::lightning::types::payment::PaymentPreimage;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::util::ser::{Readable, Writeable, Writer};
use ldk_node::lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};
use ldk_node::payment::PaymentDetails;

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;

const STORE_PRIMARY_KEY: &str = "orange_sdk";
const STORE_SECONDARY_KEY: &str = "payment_store";
const SPLICE_OUT_SECONDARY_KEY: &str = "splice_out";

/// The lifecycle state of a [`Transaction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStatus {
	/// The payment has been initiated but not yet settled.
	Pending,
	/// The payment settled successfully.
	Completed,
	/// The payment failed (e.g. no route, insufficient funds, timeout).
	Failed,
}

impl Writeable for TxStatus {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), io::Error> {
		match self {
			TxStatus::Pending => 0_u8.write(writer),
			TxStatus::Completed => 1_u8.write(writer),
			TxStatus::Failed => 2_u8.write(writer),
		}
	}
}

impl Readable for TxStatus {
	fn read<R: io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let n: u8 = Readable::read(reader)?;
		match n {
			0 => Ok(TxStatus::Pending),
			1 => Ok(TxStatus::Completed),
			2 => Ok(TxStatus::Failed),
			_ => Err(DecodeError::InvalidValue),
		}
	}
}

/// A unified record of a payment made or received, returned by [`Wallet::list_transactions`](crate::Wallet::list_transactions).
///
/// Transactions cover both trusted and self-custodial payments. Internal rebalance
/// transfers are merged into single entries with combined fees.
#[derive(Debug, Clone)]
pub struct Transaction {
	/// Unique identifier for this payment.
	///
	/// Use the variant ([`PaymentId::Trusted`] vs [`PaymentId::SelfCustodial`]) to determine
	/// which wallet layer handled the payment.
	pub id: PaymentId,
	/// Current lifecycle state of this transaction.
	pub status: TxStatus,
	/// `true` for outbound (sent) payments, `false` for inbound (received).
	pub outbound: bool,
	/// The payment amount, or `None` if not yet known (e.g. pending inbound).
	pub amount: Option<Amount>,
	/// The fee paid for this transaction, or `None` if not yet known.
	///
	/// For internal rebalance transfers, this is the combined fee across both
	/// the trusted and Lightning legs of the transfer.
	pub fee: Option<Amount>,
	/// The payment method and associated metadata (Lightning BOLT 11/12, on-chain, etc.).
	pub payment_type: PaymentType,
	/// When this transaction was created, as a duration since the Unix epoch.
	pub time_since_epoch: Duration,
}

/// A [Transaction] that is stored in the database. We have to modify the `Transaction` type
/// to have types that all implement `Writeable` and `Readable` so that we can store it in the database.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Some trusted backends don't use this
pub(crate) struct StoreTransaction {
	pub status: TxStatus,
	pub outbound: bool,
	pub amount_msats: Option<u64>,
	pub fee_msats: Option<u64>,
	pub payment_type: PaymentType,
	/// The time the transaction was created, in seconds since the epoch.
	pub time_since_epoch: u64,
}

impl_writeable_tlv_based!(StoreTransaction, {
	(0, status, required),
	(2, outbound, required),
	(3, amount_msats, option),
	(5, fee_msats, option),
	(6, payment_type, required),
	(8, time_since_epoch, required)
});

impl From<Transaction> for StoreTransaction {
	fn from(tx: Transaction) -> Self {
		StoreTransaction {
			status: tx.status,
			outbound: tx.outbound,
			amount_msats: tx.amount.map(|a| a.milli_sats()),
			fee_msats: tx.fee.map(|a| a.milli_sats()),
			payment_type: tx.payment_type,
			time_since_epoch: tx.time_since_epoch.as_secs(),
		}
	}
}

/// A unique identifier for a payment, tagged by which wallet layer handled it.
///
/// The string representation uses a `SC-` prefix for self-custodial payments and a `TR-`
/// prefix for trusted payments, followed by the hex-encoded 32-byte ID. This format
/// round-trips via [`Display`](std::fmt::Display) and [`FromStr`](std::str::FromStr).
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum PaymentId {
	/// A payment handled by the self-custodial Lightning node.
	SelfCustodial([u8; 32]),
	/// A payment handled by the trusted wallet backend (Spark, Cashu, etc.).
	Trusted([u8; 32]),
}

impl fmt::Display for PaymentId {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		match self {
			PaymentId::SelfCustodial(bytes) => write!(fmt, "SC-{}", bytes.as_hex()),
			PaymentId::Trusted(s) => write!(fmt, "TR-{}", s.as_hex()),
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
			"SC-" => {
				let id = FromHex::from_hex(&s[3..]).map_err(|_| ())?;
				Ok(PaymentId::SelfCustodial(id))
			},
			"TR-" => {
				let id = FromHex::from_hex(&s[3..]).map_err(|_| ())?;
				Ok(PaymentId::Trusted(id))
			},
			_ => Err(()),
		}
	}
}

impl_writeable_tlv_based_enum!(PaymentId,
	{0, SelfCustodial} => (),
	{1, Trusted} => (),
);

/// The payment method and associated metadata for a [`Transaction`].
///
/// For outgoing Lightning payments, the `payment_preimage` field serves as cryptographic
/// proof of payment and is populated once the payment reaches [`TxStatus::Completed`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
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
	IncomingLightning {},
	/// A payment that is internal to the trusted service. We should not create any
	/// of these but its possible we have them in our transaction history and we
	/// need to be able to read them.
	TrustedInternal {},
}

impl_writeable_tlv_based_enum!(PaymentType,
	(0, OutgoingLightningBolt12) => { (0, payment_preimage, option), },
	(1, OutgoingLightningBolt11) => { (0, payment_preimage, option), },
	(2, OutgoingOnChain) => { (0, txid, option), },
	(3, IncomingOnChain) => { (0, txid, option) },
	(4, IncomingLightning) => { },
	(5, TrustedInternal) => { },
);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum TxType {
	TrustedToLightning {
		trusted_payment: [u8; 32],
		lightning_payment: [u8; 32],
		payment_triggering_transfer: PaymentId,
	},
	OnchainToLightning {
		/// The transaction ID of the tx that opens the channel.
		channel_txid: Txid,
		/// The transaction ID of the on-chain payment that triggered the Lightning payment.
		triggering_txid: Txid,
	},
	PaymentTriggeringTransferLightning {
		ty: PaymentType,
	},
	Payment {
		ty: PaymentType,
	},
	PendingRebalance {},
}

impl TxType {
	pub(crate) fn is_rebalance(&self) -> bool {
		matches!(
			self,
			TxType::PendingRebalance {}
				| TxType::TrustedToLightning { .. }
				| TxType::OnchainToLightning { .. }
		)
	}
}

impl_writeable_tlv_based_enum!(TxType,
	(0, TrustedToLightning) => {
		(0, trusted_payment, required),
		(2, lightning_payment, required),
		(4, payment_triggering_transfer, required),
	},
	(1, OnchainToLightning) => {
		(0, channel_txid, required),
		(2, triggering_txid, required),
	},
	(2, PaymentTriggeringTransferLightning) => { (0, ty, required), },
	(3, Payment) => { (0, ty, required), },
	(4, PendingRebalance) => {},
);

#[derive(Debug, Copy, Clone)]
pub(crate) struct TxMetadata {
	// TODO: We should remove `time` once we get the info we need from the trusted end
	pub(crate) time: Duration,
	pub(crate) ty: TxType,
}

impl_writeable_tlv_based!(TxMetadata, { (0, ty, required), (2, time, required) });

#[derive(Clone)]
pub(crate) struct TxMetadataStore {
	tx_metadata: Arc<RwLock<HashMap<PaymentId, TxMetadata>>>,
	store: Arc<DynStore>,
}

impl TxMetadataStore {
	pub async fn new(store: Arc<DynStore>) -> TxMetadataStore {
		let keys = KVStore::list(store.as_ref(), STORE_PRIMARY_KEY, STORE_SECONDARY_KEY)
			.await
			.expect("We do not allow reads to fail");
		let mut tx_metadata = HashMap::with_capacity(keys.len());
		for key in keys {
			let data_bytes =
				KVStore::read(store.as_ref(), STORE_PRIMARY_KEY, STORE_SECONDARY_KEY, &key)
					.await
					.expect("We do not allow reads to fail");
			let key =
				PaymentId::from_str(&key).expect("Invalid key in transaction metadata storage");
			let data = Readable::read(&mut &data_bytes[..])
				.expect("Invalid data in transaction metadata storage");
			tx_metadata.insert(key, data);
		}
		TxMetadataStore { store, tx_metadata: Arc::new(RwLock::new(tx_metadata)) }
	}

	pub fn read(&self) -> RwLockReadGuard<'_, HashMap<PaymentId, TxMetadata>> {
		self.tx_metadata.read().unwrap()
	}

	async fn do_set(&self, key: PaymentId, value: TxMetadata) -> bool {
		let key_str = key.to_string();
		let ser = value.encode();
		let old = {
			let mut tx_metadata = self.tx_metadata.write().unwrap();
			tx_metadata.insert(key, value)
		};
		KVStore::write(self.store.as_ref(), STORE_PRIMARY_KEY, STORE_SECONDARY_KEY, &key_str, ser)
			.await
			.expect("We do not allow writes to fail");
		old.is_some()
	}

	pub async fn upsert(&self, key: PaymentId, value: TxMetadata) {
		self.do_set(key, value).await;
	}

	pub async fn insert(&self, key: PaymentId, value: TxMetadata) {
		let had_old = self.do_set(key, value).await;
		debug_assert!(!had_old);
	}

	pub async fn set_tx_caused_rebalance(&self, payment_id: &PaymentId) -> Result<(), ()> {
		let (key_str, ser) = {
			let mut tx_metadata = self.tx_metadata.write().unwrap();
			if let Some(metadata) = tx_metadata.get_mut(payment_id) {
				if let TxType::Payment { ty } = &mut metadata.ty {
					metadata.ty = TxType::PaymentTriggeringTransferLightning { ty: *ty };
					let key_str = payment_id.to_string();
					let ser = metadata.encode();
					(key_str, ser)
				} else {
					eprintln!("payment_id {payment_id} is not a payment, cannot set rebalance");
					return Err(());
				}
			} else {
				eprintln!("doesn't exist in metadata store: {payment_id}");
				return Err(());
			}
		};
		KVStore::write(self.store.as_ref(), STORE_PRIMARY_KEY, STORE_SECONDARY_KEY, &key_str, ser)
			.await
			.expect("We do not allow writes to fail");
		Ok(())
	}

	/// Sets the preimage for an outgoing lightning payment. If the payment already has a preimage,
	/// this is a no-op and returns Ok(()). If the payment_id does not exist in the store, or if the payment
	/// is not an outgoing lightning payment, returns Err(()).
	pub async fn set_preimage(&self, payment_id: PaymentId, preimage: [u8; 32]) -> Result<(), ()> {
		let (key_str, ser) = {
			let mut tx_metadata = self.tx_metadata.write().unwrap();
			if let Some(metadata) = tx_metadata.get_mut(&payment_id) {
				match metadata.ty {
					TxType::Payment { ty } => match ty {
						PaymentType::OutgoingLightningBolt12 { payment_preimage } => {
							if payment_preimage.is_some() {
								return Ok(());
							} else {
								metadata.ty = TxType::Payment {
									ty: PaymentType::OutgoingLightningBolt12 {
										payment_preimage: Some(PaymentPreimage(preimage)),
									},
								};
								(payment_id.to_string(), metadata.encode())
							}
						},
						PaymentType::OutgoingLightningBolt11 { payment_preimage } => {
							if payment_preimage.is_some() {
								return Ok(());
							} else {
								metadata.ty = TxType::Payment {
									ty: PaymentType::OutgoingLightningBolt11 {
										payment_preimage: Some(PaymentPreimage(preimage)),
									},
								};
								(payment_id.to_string(), metadata.encode())
							}
						},
						_ => {
							eprintln!(
								"payment_id {payment_id} is not an outgoing lightning payment, cannot set preimage"
							);
							return Err(());
						},
					},
					_ => {
						// if we're trying to set a preimage on a non-payment, just continue
						// this should only happen when we finish a rebalance payment
						return Ok(());
					},
				}
			} else {
				eprintln!("doesn't exist in metadata store: {payment_id}");
				return Err(());
			}
		};

		KVStore::write(self.store.as_ref(), STORE_PRIMARY_KEY, STORE_SECONDARY_KEY, &key_str, ser)
			.await
			.expect("We do not allow writes to fail");
		Ok(())
	}
}

const REBALANCE_ENABLED_KEY: &str = "rebalance_enabled";

pub(crate) async fn get_rebalance_enabled(store: &DynStore) -> bool {
	match KVStore::read(store, STORE_PRIMARY_KEY, "", REBALANCE_ENABLED_KEY).await {
		Ok(bytes) => Readable::read(&mut &bytes[..]).expect("Invalid data in rebalance_enabled"),
		Err(e) if e.kind() == io::ErrorKind::NotFound => {
			// if rebalance_enabled is not found, default to true
			// and write it to the store so we don't have to do this again
			let rebalance_enabled = true;
			set_rebalance_enabled(store, rebalance_enabled).await;
			rebalance_enabled
		},
		Err(e) => {
			panic!("Failed to read rebalance_enabled: {e}");
		},
	}
}

pub(crate) async fn set_rebalance_enabled(store: &DynStore, enabled: bool) {
	let bytes = enabled.encode();
	KVStore::write(store, STORE_PRIMARY_KEY, "", REBALANCE_ENABLED_KEY, bytes)
		.await
		.expect("Failed to write rebalance_enabled");
}

pub(crate) async fn write_splice_out(store: &DynStore, details: &PaymentDetails) {
	KVStore::write(
		store,
		STORE_PRIMARY_KEY,
		SPLICE_OUT_SECONDARY_KEY,
		&details.id.0.to_lower_hex_string(),
		details.encode(),
	)
	.await
	.expect("Failed to write splice out txid");
}

pub(crate) async fn read_splice_outs(store: &DynStore) -> Vec<PaymentDetails> {
	let keys = KVStore::list(store, STORE_PRIMARY_KEY, SPLICE_OUT_SECONDARY_KEY)
		.await
		.expect("We do not allow reads to fail");
	let mut splice_outs = Vec::with_capacity(keys.len());
	for key in keys {
		let data_bytes = KVStore::read(store, STORE_PRIMARY_KEY, SPLICE_OUT_SECONDARY_KEY, &key)
			.await
			.expect("We do not allow reads to fail");
		let data =
			Readable::read(&mut &data_bytes[..]).expect("Invalid data in splice out storage");
		splice_outs.push(data);
	}
	splice_outs
}

#[cfg(test)]
mod tests {
	use super::*;
	use ldk_node::bitcoin::hex::DisplayHex;
	use std::str::FromStr;

	#[test]
	fn test_payment_id_round_trip() {
		let ln_id_bytes = [
			1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
			25, 26, 27, 28, 29, 30, 31, 32,
		];
		let ln_id = PaymentId::SelfCustodial(ln_id_bytes);
		let ln_id_str = ln_id.to_string();
		assert_eq!(
			ln_id_str,
			"SC-0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20"
		);
		let parsed_ln_id = PaymentId::from_str(&ln_id_str).unwrap();
		assert_eq!(ln_id, parsed_ln_id);

		let trusted_id_bytes = [0; 32];
		let trusted_id = PaymentId::Trusted(trusted_id_bytes);
		let trusted_id_str = trusted_id.to_string();
		assert_eq!(trusted_id_str, format!("TR-{}", trusted_id_bytes.as_hex()));
		let parsed_trusted_id = PaymentId::from_str(&trusted_id_str).unwrap();
		assert_eq!(trusted_id, parsed_trusted_id);

		// Test invalid formats
		assert!(PaymentId::from_str("INVALID").is_err());
		assert!(PaymentId::from_str("LN-invalidhex").is_err());
		assert!(PaymentId::from_str("TR-invalidhex").is_err());
		assert!(PaymentId::from_str("SC-invalidhex").is_err());
		assert!(PaymentId::from_str("XX-something").is_err());
	}
}
