//! Spark KV store implementation

use std::sync::Arc;

use crate::io;
use ldk_node::DynStore;
use ldk_node::lightning::util::persist::KVStore;

use breez_sdk_spark::{
	DepositInfo, ListPaymentsRequest, Payment, PaymentDetails, PaymentMetadata, StorageError,
	UpdateDepositPayload,
};
use ldk_node::lightning::util::persist::KVSTORE_NAMESPACE_KEY_MAX_LEN;

const SPARK_PRIMARY_NAMESPACE: &str = "spark";
const SPARK_CACHE_NAMESPACE: &str = "cache";
const SPARK_PAYMENTS_NAMESPACE: &str = "payment";
const SPARK_DEPOSITS_NAMESPACE: &str = "deposit";

#[derive(Clone)]
pub(crate) struct SparkStore(pub(crate) Arc<DynStore>);

/// The Spark sdk can produce keys that are too long, we just truncate them here
fn sanitize_key(key: String) -> String {
	if key.len() > KVSTORE_NAMESPACE_KEY_MAX_LEN {
		key[..KVSTORE_NAMESPACE_KEY_MAX_LEN].to_string()
	} else {
		key
	}
}

#[async_trait::async_trait]
impl breez_sdk_spark::Storage for SparkStore {
	async fn delete_cached_item(&self, key: String) -> Result<(), StorageError> {
		let key = sanitize_key(key);
		KVStore::remove(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_CACHE_NAMESPACE, &key, false)
			.await
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
		Ok(())
	}

	async fn get_cached_item(&self, key: String) -> Result<Option<String>, StorageError> {
		let key = sanitize_key(key);
		match KVStore::read(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_CACHE_NAMESPACE, &key)
			.await
		{
			Ok(bytes) => Ok(Some(String::from_utf8(bytes).map_err(|e| {
				StorageError::Serialization(format!("Invalid UTF-8 in cached item: {e:?}"))
			})?)),
			Err(e) => {
				if let io::ErrorKind::NotFound = e.kind() {
					Ok(None)
				} else {
					Err(StorageError::Implementation(format!("{e:?}")))
				}
			},
		}
	}

	async fn set_cached_item(&self, key: String, value: String) -> Result<(), StorageError> {
		let key = sanitize_key(key);
		KVStore::write(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_CACHE_NAMESPACE,
			&key,
			value.into_bytes(),
		)
		.await
		.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
		Ok(())
	}

	async fn list_payments(
		&self, request: ListPaymentsRequest,
	) -> Result<Vec<breez_sdk_spark::Payment>, StorageError> {
		let keys =
			KVStore::list(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_PAYMENTS_NAMESPACE)
				.await
				.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		let mut payments = Vec::with_capacity(keys.len());
		for key in keys {
			let data = KVStore::read(
				self.0.as_ref(),
				SPARK_PRIMARY_NAMESPACE,
				SPARK_PAYMENTS_NAMESPACE,
				&key,
			)
			.await
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

			let payment: breez_sdk_spark::Payment = serde_json::from_slice(&data)
				.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;
			payments.push(payment);
		}

		let sort_ascending = request.sort_ascending.unwrap_or(false);
		if sort_ascending {
			payments.sort_by_key(|p| p.timestamp);
		} else {
			payments.sort_by_key(|p| std::cmp::Reverse(p.timestamp));
		}

		// apply offset and limit
		let start = request.offset.unwrap_or(0) as usize;
		let end = if let Some(l) = request.limit {
			(start + l as usize).min(payments.len())
		} else {
			payments.len()
		};
		let payments =
			if start < payments.len() { payments[start..end].to_vec() } else { Vec::new() };

		Ok(payments)
	}

	async fn insert_payment(&self, payment: breez_sdk_spark::Payment) -> Result<(), StorageError> {
		let data = serde_json::to_vec(&payment)
			.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;

		KVStore::write(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_PAYMENTS_NAMESPACE,
			&payment.id,
			data,
		)
		.await
		.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
		Ok(())
	}

	async fn set_payment_metadata(
		&self, _: String, _: PaymentMetadata,
	) -> Result<(), StorageError> {
		// we don't use this
		Ok(())
	}

	async fn get_payment_by_id(
		&self, id: String,
	) -> Result<breez_sdk_spark::Payment, StorageError> {
		let data =
			KVStore::read(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_PAYMENTS_NAMESPACE, &id)
				.await
				.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		let payment: breez_sdk_spark::Payment = serde_json::from_slice(&data)
			.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;
		Ok(payment)
	}

	async fn get_payment_by_invoice(
		&self, invoice: String,
	) -> Result<Option<Payment>, StorageError> {
		let payments = self.list_payments(ListPaymentsRequest::default()).await?;

		let p = payments.into_iter().find(|p| {
			if let Some(details) = p.details.as_ref() {
				match details {
					PaymentDetails::Spark { invoice_details } => {
						if invoice_details.as_ref().is_some_and(|i| i.invoice == invoice) {
							return true;
						}
					},
					PaymentDetails::Token { .. } => {},
					PaymentDetails::Lightning { invoice: inv, .. } => {
						if *inv == invoice {
							return true;
						}
					},
					PaymentDetails::Withdraw { .. } => {},
					PaymentDetails::Deposit { .. } => {},
				}
			}
			false
		});

		Ok(p)
	}

	async fn add_deposit(
		&self, txid: String, vout: u32, amount_sats: u64,
	) -> Result<(), StorageError> {
		let id = format!("{txid}:{vout}");
		let info = DepositInfo {
			txid,
			vout,
			amount_sats,
			refund_tx: None,
			refund_tx_id: None,
			claim_error: None,
		};

		let data =
			serde_json::to_vec(&info).map_err(|e| StorageError::Serialization(format!("{e:?}")))?;

		KVStore::write(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_DEPOSITS_NAMESPACE,
			&id,
			data,
		)
		.await
		.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		Ok(())
	}

	async fn delete_deposit(&self, txid: String, vout: u32) -> Result<(), StorageError> {
		let id = format!("{txid}:{vout}");
		KVStore::remove(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_DEPOSITS_NAMESPACE, &id)
			.await
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
		Ok(())
	}

	async fn list_deposits(&self) -> Result<Vec<DepositInfo>, StorageError> {
		let keys =
			KVStore::list(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_DEPOSITS_NAMESPACE)
				.await
				.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		let mut deposits = Vec::with_capacity(keys.len());
		for key in keys {
			let data = KVStore::read(
				self.0.as_ref(),
				SPARK_PRIMARY_NAMESPACE,
				SPARK_DEPOSITS_NAMESPACE,
				&key,
			)
			.await
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

			let deposit: DepositInfo = serde_json::from_slice(&data)
				.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;
			deposits.push(deposit);
		}

		Ok(deposits)
	}

	async fn update_deposit(
		&self, txid: String, vout: u32, payload: UpdateDepositPayload,
	) -> Result<(), StorageError> {
		let id = format!("{txid}:{vout}");

		let data = match KVStore::read(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_DEPOSITS_NAMESPACE,
			&id,
		)
		.await
		{
			Ok(data) => data,
			Err(e) => {
				if let io::ErrorKind::NotFound = e.kind() {
					// deposit does not exist, nothing to update
					return Ok(());
				} else {
					Err(StorageError::Implementation(format!("{e:?}")))?
				}
			},
		};

		let mut deposit: DepositInfo = serde_json::from_slice(&data)
			.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;

		match payload {
			UpdateDepositPayload::ClaimError { error } => {
				deposit.claim_error = Some(error);
			},
			UpdateDepositPayload::Refund { refund_txid, refund_tx } => {
				deposit.refund_tx_id = Some(refund_txid);
				deposit.refund_tx = Some(refund_tx);
			},
		}

		let data = serde_json::to_vec(&deposit)
			.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;

		KVStore::write(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_DEPOSITS_NAMESPACE,
			&id,
			data,
		)
		.await
		.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		Ok(())
	}
}
