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

use crate::dyn_store::DynStore;
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

/// A transaction is a record of a payment made or received. It contains information about the
/// transaction, such as the amount, fee, and status. It is used to track the state of a payment
/// and to provide information about the payment to the user.
#[derive(Debug, Clone)]
pub struct Transaction {
	/// The unique identifier for the payment.
	pub id: PaymentId,
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

/// A PaymentId is a unique identifier for a payment. It can be either a Lightning payment or a
/// Trusted payment. It is used to track the state of a payment and to provide information about
/// the payment to the user.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum PaymentId {
	/// A self-custodial payment identifier.
	SelfCustodial([u8; 32]),
	/// A trusted payment identifier.
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

/// Represents the type of payment, including its method and associated metadata.
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
	/// A single leg of a multi-path payment that is split across the trusted and lightning
	/// wallets. Both legs carry the same `surface_id`; the entry stored under that id additionally
	/// accumulates each leg's terminal result so the two can be coalesced into a single event. The
	/// accumulated fields are persisted, so the combined event is emitted exactly once even across
	/// restarts and races between the two legs completing.
	MppPayment {
		/// The id under which the combined payment is surfaced to the user (the trusted leg id).
		surface_id: PaymentId,
		/// The lightning leg's payment id (equal to the BOLT 11 payment hash).
		lightning_leg: [u8; 32],
		/// The total amount, in msats, of the combined payment (i.e. the invoice amount).
		total_amount_msat: u64,
		/// The payment type of the combined transaction.
		ty: PaymentType,
		/// The trusted leg's fee, in msats, set once that leg succeeds.
		trusted_fee_msat: Option<u64>,
		/// The lightning leg's fee, in msats, set once that leg succeeds.
		lightning_fee_msat: Option<u64>,
		/// The shared payment preimage, set once either leg succeeds.
		preimage: Option<[u8; 32]>,
		/// Set if either leg failed.
		failed: bool,
		/// Set once the single combined terminal event has been surfaced.
		finalized: bool,
	},
}

/// The terminal outcome of a multi-path payment once both legs have resolved, returned by
/// [`TxMetadataStore::record_mpp_leg`] to the caller that should surface the single combined event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MppOutcome {
	/// Both legs succeeded.
	Succeeded {
		/// The BOLT 11 payment hash.
		payment_hash: [u8; 32],
		/// The shared payment preimage.
		preimage: [u8; 32],
		/// The combined fee, in msats, across both legs.
		fee_msat: u64,
	},
	/// At least one leg failed.
	Failed {
		/// The BOLT 11 payment hash.
		payment_hash: [u8; 32],
	},
}

/// Which wallet a leg of a multi-path payment was paid out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MppLegKind {
	/// The leg paid by the trusted wallet (surfaced under the combined payment's id).
	Trusted,
	/// The leg paid by the self-custody lightning wallet.
	Lightning,
}

/// The state of a single leg of a multi-path payment as reflected in the transaction list.
#[derive(Debug, Clone, Copy)]
struct MppLeg {
	status: TxStatus,
	fee_msat: u64,
}

/// Accumulates the two legs of a multi-path payment back into the single combined [`Transaction`]
/// surfaced to the user. Each leg ([`TxType::MppPayment`]) is folded in with [`Self::accumulate`],
/// tagged by which wallet paid it, and the result is built with [`Self::into_transaction`]. Both
/// legs carry the same total amount, and their fees are summed. Tracking the trusted and lightning
/// legs separately (rather than just counting them) lets us tell which leg is missing or failed
/// when describing the payment's state.
#[derive(Debug, Default)]
pub(crate) struct MppMerge {
	total_amount_msat: u64,
	trusted: Option<MppLeg>,
	lightning: Option<MppLeg>,
	payment_type: Option<PaymentType>,
	time: Option<Duration>,
}

impl MppMerge {
	/// Folds the `leg` of the multi-path payment into the merge.
	pub(crate) fn accumulate(
		&mut self, leg: MppLegKind, total_amount_msat: u64, fee_msat: u64, status: TxStatus,
		ty: PaymentType, time: Duration,
	) {
		self.total_amount_msat = total_amount_msat;
		let slot = match leg {
			MppLegKind::Trusted => &mut self.trusted,
			MppLegKind::Lightning => &mut self.lightning,
		};
		*slot = Some(MppLeg { status, fee_msat });
		// Prefer a payment type that carries the preimage (the lightning leg) for the
		// combined transaction.
		let ty_has_preimage = matches!(
			ty,
			PaymentType::OutgoingLightningBolt11 { payment_preimage: Some(_) }
				| PaymentType::OutgoingLightningBolt12 { payment_preimage: Some(_) }
		);
		if self.payment_type.is_none() || ty_has_preimage {
			self.payment_type = Some(ty);
		}
		self.time = Some(self.time.map_or(time, |t| t.min(time)));
	}

	/// The combined status of the payment: failed if either leg failed, completed only once both
	/// legs have completed, and otherwise still pending.
	pub(crate) fn status(&self) -> TxStatus {
		let leg_status = |leg: Option<MppLeg>| leg.map(|l| l.status);
		if [self.trusted, self.lightning].iter().any(|l| leg_status(*l) == Some(TxStatus::Failed)) {
			TxStatus::Failed
		} else if leg_status(self.trusted) == Some(TxStatus::Completed)
			&& leg_status(self.lightning) == Some(TxStatus::Completed)
		{
			TxStatus::Completed
		} else {
			TxStatus::Pending
		}
	}

	/// Builds the single combined transaction surfaced under `surface_id`, summing both legs' fees.
	pub(crate) fn into_transaction(self, surface_id: PaymentId) -> Transaction {
		let status = self.status();
		let fee_msat = self
			.trusted
			.map_or(0, |l| l.fee_msat)
			.saturating_add(self.lightning.map_or(0, |l| l.fee_msat));
		Transaction {
			id: surface_id,
			status,
			outbound: true,
			amount: Some(Amount::from_milli_sats(self.total_amount_msat).expect("valid amount")),
			fee: Some(Amount::from_milli_sats(fee_msat).expect("valid amount")),
			payment_type: self
				.payment_type
				.unwrap_or(PaymentType::OutgoingLightningBolt11 { payment_preimage: None }),
			time_since_epoch: self.time.unwrap_or_default(),
		}
	}
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
	(5, MppPayment) => {
		(0, surface_id, required),
		(2, total_amount_msat, required),
		(4, ty, required),
		(6, lightning_leg, required),
		(8, finalized, required),
		(10, failed, required),
		(12, trusted_fee_msat, option),
		(14, lightning_fee_msat, option),
		(16, preimage, option),
	},
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
	store: Arc<dyn DynStore>,
	/// Serializes the read-modify-persist sequence in [`Self::record_mpp_leg`]. Both legs of a
	/// multi-path payment record their result onto the same surface entry; without this, two
	/// concurrent legs could each encode a partial snapshot under the in-memory lock and then
	/// persist them out of order, leaving a stale (non-finalized) entry on disk that would
	/// re-emit the combined event after a restart.
	mpp_record_lock: Arc<tokio::sync::Mutex<()>>,
}

impl TxMetadataStore {
	pub async fn new(store: Arc<dyn DynStore>) -> TxMetadataStore {
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
		TxMetadataStore {
			store,
			tx_metadata: Arc::new(RwLock::new(tx_metadata)),
			mpp_record_lock: Arc::new(tokio::sync::Mutex::new(())),
		}
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

	/// Atomically marks `trigger_id` as a rebalance trigger and writes `splice_metadata` for
	/// `splice_id`. The in-memory updates happen under a single write lock so that a concurrent
	/// `list_transactions` either sees both changes or neither — without this, the rebalancer
	/// briefly exposes a state where the trigger has been promoted to
	/// `PaymentTriggeringTransferLightning` but the matching `OnchainToLightning` splice entry
	/// hasn't landed yet, which makes the `InternalTransfer` validation in `list_transactions`
	/// trip on a missing `send_fee`.
	pub async fn set_tx_caused_rebalance_with_splice(
		&self, trigger_id: &PaymentId, splice_id: PaymentId, splice_metadata: TxMetadata,
	) -> Result<(), ()> {
		let (trigger_entry, splice_entry) = {
			let mut tx_metadata = self.tx_metadata.write().unwrap();
			let trigger = match tx_metadata.get_mut(trigger_id) {
				Some(metadata) => {
					if let TxType::Payment { ty } = &mut metadata.ty {
						metadata.ty = TxType::PaymentTriggeringTransferLightning { ty: *ty };
						(trigger_id.to_string(), metadata.encode())
					} else {
						eprintln!("payment_id {trigger_id} is not a payment, cannot set rebalance");
						return Err(());
					}
				},
				None => {
					eprintln!("doesn't exist in metadata store: {trigger_id}");
					return Err(());
				},
			};
			tx_metadata.insert(splice_id, splice_metadata);
			let splice = (splice_id.to_string(), splice_metadata.encode());
			(trigger, splice)
		};
		let (trigger_res, splice_res) = tokio::join!(
			KVStore::write(
				self.store.as_ref(),
				STORE_PRIMARY_KEY,
				STORE_SECONDARY_KEY,
				&trigger_entry.0,
				trigger_entry.1,
			),
			KVStore::write(
				self.store.as_ref(),
				STORE_PRIMARY_KEY,
				STORE_SECONDARY_KEY,
				&splice_entry.0,
				splice_entry.1,
			),
		);
		trigger_res.expect("We do not allow writes to fail");
		splice_res.expect("We do not allow writes to fail");
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

	/// Records the terminal result of one leg of a multi-path payment, accumulating it onto the
	/// shared entry surfaced to the user. `leg_id` is the leg's own payment id and `success` is
	/// `Some((fee_msat, preimage))` if it succeeded or `None` if it failed.
	///
	/// Returns `Some` exactly once — for the call that completes the payment (both legs succeeded,
	/// or the first leg failed) — with the combined [`MppOutcome`] the caller should surface as a
	/// single event. Returns `None` while still waiting on the other leg, after the combined event
	/// has already been produced, or if `leg_id` is not a multi-path payment leg.
	///
	/// All accumulated state is persisted, so the combined event is emitted exactly once even across
	/// restarts and races between the two legs completing.
	pub(crate) async fn record_mpp_leg(
		&self, leg_id: PaymentId, success: Option<(u64, [u8; 32])>,
	) -> Option<(PaymentId, MppOutcome)> {
		// Hold this across the whole read-modify-persist so the two legs can't interleave: the
		// snapshot we persist below must reflect the latest in-memory mutation, otherwise a stale
		// (non-finalized) entry could be written last and re-emit the combined event on restart.
		let _record_guard = self.mpp_record_lock.lock().await;

		let (surface_id, key_str, ser, outcome) = {
			let mut map = self.tx_metadata.write().unwrap();

			// The leg's own entry tells us which combined payment it belongs to.
			let surface_id = match map.get(&leg_id).map(|m| m.ty) {
				Some(TxType::MppPayment { surface_id, .. }) => surface_id,
				_ => return None,
			};
			// The combined payment is surfaced under the trusted leg's id.
			let is_trusted_leg = leg_id == surface_id;

			let outcome = {
				let metadata = match map.get_mut(&surface_id) {
					Some(metadata) => metadata,
					None => return None,
				};
				let TxType::MppPayment {
					lightning_leg,
					trusted_fee_msat,
					lightning_fee_msat,
					preimage,
					failed,
					finalized,
					..
				} = &mut metadata.ty
				else {
					return None;
				};
				if *finalized {
					return None;
				}

				match success {
					Some((fee_msat, leg_preimage)) => {
						if is_trusted_leg {
							*trusted_fee_msat = Some(fee_msat);
						} else {
							*lightning_fee_msat = Some(fee_msat);
						}
						*preimage = Some(leg_preimage);
					},
					None => *failed = true,
				}

				let payment_hash = *lightning_leg;
				if *failed {
					*finalized = true;
					Some(MppOutcome::Failed { payment_hash })
				} else if let (Some(trusted_fee), Some(lightning_fee)) =
					(*trusted_fee_msat, *lightning_fee_msat)
				{
					*finalized = true;
					Some(MppOutcome::Succeeded {
						payment_hash,
						preimage: preimage.expect("set whenever a leg succeeds"),
						fee_msat: trusted_fee.saturating_add(lightning_fee),
					})
				} else {
					None
				}
			};

			// We mutated the surface entry (recorded this leg), so persist it regardless of whether
			// the payment is now complete.
			let ser = map.get(&surface_id).expect("surface entry present").encode();
			(surface_id, surface_id.to_string(), ser, outcome)
		};

		KVStore::write(self.store.as_ref(), STORE_PRIMARY_KEY, STORE_SECONDARY_KEY, &key_str, ser)
			.await
			.expect("We do not allow writes to fail");

		outcome.map(|outcome| (surface_id, outcome))
	}
}

const REBALANCE_ENABLED_KEY: &str = "rebalance_enabled";

pub(crate) async fn get_rebalance_enabled(store: &dyn DynStore) -> bool {
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

pub(crate) async fn set_rebalance_enabled(store: &dyn DynStore, enabled: bool) {
	let bytes = enabled.encode();
	KVStore::write(store, STORE_PRIMARY_KEY, "", REBALANCE_ENABLED_KEY, bytes)
		.await
		.expect("Failed to write rebalance_enabled");
}

pub(crate) async fn write_splice_out(store: &dyn DynStore, details: &PaymentDetails) {
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

pub(crate) async fn read_splice_outs(store: &dyn DynStore) -> Vec<PaymentDetails> {
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
	use ldk_node::io::sqlite_store::SqliteStore;
	use std::path::PathBuf;
	use std::str::FromStr;
	use std::time::{SystemTime, UNIX_EPOCH};

	fn temp_sqlite_store() -> (PathBuf, Arc<dyn DynStore>) {
		let path = std::env::temp_dir().join(format!(
			"orange-sdk-mpp-finalize-test-{}",
			SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
		));
		let store = SqliteStore::new(path.clone(), Some("orange.sqlite".to_string()), None)
			.expect("sqlite store");
		(path, Arc::new(store))
	}

	const TRUSTED_LEG: [u8; 32] = [7u8; 32];
	const LIGHTNING_LEG: [u8; 32] = [9u8; 32];

	fn surface_id() -> PaymentId {
		PaymentId::Trusted(TRUSTED_LEG)
	}
	fn lightning_id() -> PaymentId {
		PaymentId::SelfCustodial(LIGHTNING_LEG)
	}

	fn mpp_metadata() -> TxMetadata {
		TxMetadata {
			ty: TxType::MppPayment {
				surface_id: surface_id(),
				lightning_leg: LIGHTNING_LEG,
				total_amount_msat: 200_000,
				ty: PaymentType::OutgoingLightningBolt11 { payment_preimage: None },
				trusted_fee_msat: None,
				lightning_fee_msat: None,
				preimage: None,
				failed: false,
				finalized: false,
			},
			time: Duration::from_secs(1),
		}
	}

	// Both legs record their result onto the shared entry; the second to land yields the single
	// combined outcome.
	async fn insert_legs(tx_metadata: &TxMetadataStore) {
		tx_metadata.insert(surface_id(), mpp_metadata()).await;
		tx_metadata.insert(lightning_id(), mpp_metadata()).await;
	}

	#[tokio::test]
	async fn record_mpp_leg_combines_both_legs_once() {
		let (_path, store) = temp_sqlite_store();
		let tx_metadata = TxMetadataStore::new(store).await;
		insert_legs(&tx_metadata).await;

		// First leg: still waiting on the other.
		assert_eq!(tx_metadata.record_mpp_leg(surface_id(), Some((1_000, [1u8; 32]))).await, None);

		// Second leg completes the payment: a single combined success with the summed fees.
		assert_eq!(
			tx_metadata.record_mpp_leg(lightning_id(), Some((2_000, [1u8; 32]))).await,
			Some((
				surface_id(),
				MppOutcome::Succeeded {
					payment_hash: LIGHTNING_LEG,
					preimage: [1u8; 32],
					fee_msat: 3_000,
				}
			))
		);

		// A replayed leg event (e.g. after a crash) does not produce a second event.
		assert_eq!(tx_metadata.record_mpp_leg(surface_id(), Some((1_000, [1u8; 32]))).await, None);
		assert_eq!(
			tx_metadata.record_mpp_leg(lightning_id(), Some((2_000, [1u8; 32]))).await,
			None
		);
	}

	// The exactly-once guarantee must survive a crash/restart: the recorded legs and the finalized
	// flag are persisted, so reloading the store from disk still suppresses a re-emit. We simulate a
	// crash/restart by dropping the `TxMetadataStore` and rebuilding it from the same on-disk store.
	#[tokio::test]
	async fn record_mpp_leg_persists_across_restart() {
		let path = std::env::temp_dir().join(format!(
			"orange-sdk-mpp-record-test-{}",
			SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
		));
		let open = |path: PathBuf| -> Arc<dyn DynStore> {
			Arc::new(
				SqliteStore::new(path, Some("orange.sqlite".to_string()), None)
					.expect("sqlite store"),
			)
		};

		{
			let tx_metadata = TxMetadataStore::new(open(path.clone())).await;
			insert_legs(&tx_metadata).await;
			assert_eq!(
				tx_metadata.record_mpp_leg(surface_id(), Some((1_000, [1u8; 32]))).await,
				None
			);
			assert!(
				tx_metadata
					.record_mpp_leg(lightning_id(), Some((2_000, [1u8; 32])))
					.await
					.is_some()
			);
		}

		// Simulate a crash/restart: reload from the same on-disk store.
		{
			let tx_metadata = TxMetadataStore::new(open(path.clone())).await;
			// Both the recorded legs and the finalized flag survived, so a replayed event after the
			// restart does not surface a duplicate combined event.
			assert_eq!(
				tx_metadata.record_mpp_leg(lightning_id(), Some((2_000, [1u8; 32]))).await,
				None
			);
		}
	}

	#[tokio::test]
	async fn record_mpp_leg_surfaces_failure_and_guards_non_mpp() {
		let (_path, store) = temp_sqlite_store();
		let tx_metadata = TxMetadataStore::new(store).await;
		insert_legs(&tx_metadata).await;

		// A failed leg fails the whole payment immediately, exactly once.
		assert_eq!(
			tx_metadata.record_mpp_leg(surface_id(), None).await,
			Some((surface_id(), MppOutcome::Failed { payment_hash: LIGHTNING_LEG }))
		);
		assert_eq!(
			tx_metadata.record_mpp_leg(lightning_id(), Some((2_000, [1u8; 32]))).await,
			None
		);

		// Unknown ids and non-MPP payments are never treated as MPP legs.
		assert_eq!(tx_metadata.record_mpp_leg(PaymentId::Trusted([5u8; 32]), None).await, None);
		let plain = PaymentId::Trusted([4u8; 32]);
		tx_metadata
			.insert(
				plain,
				TxMetadata {
					ty: TxType::Payment {
						ty: PaymentType::OutgoingLightningBolt11 { payment_preimage: None },
					},
					time: Duration::from_secs(1),
				},
			)
			.await;
		assert_eq!(tx_metadata.record_mpp_leg(plain, Some((1, [0u8; 32]))).await, None);
	}

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
