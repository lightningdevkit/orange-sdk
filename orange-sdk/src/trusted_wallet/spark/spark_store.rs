//! Spark KV store implementation

use std::collections::HashMap;
use std::sync::Arc;

use crate::io;

use breez_sdk_spark::sync_storage::{
	IncomingChange, OutgoingChange, Record, RecordChange, RecordId, UnversionedRecordChange,
};
use breez_sdk_spark::{
	DepositInfo, Payment, PaymentDetails, PaymentMetadata, SetLnurlMetadataItem, StorageError,
	StorageListPaymentsRequest, UpdateDepositPayload,
};
use ldk_node::DynStore;
use ldk_node::lightning::util::persist::KVSTORE_NAMESPACE_KEY_MAX_LEN;
use ldk_node::lightning::util::persist::KVStore;

const SPARK_PRIMARY_NAMESPACE: &str = "spark";
const SPARK_CACHE_NAMESPACE: &str = "cache";
const SPARK_PAYMENTS_NAMESPACE: &str = "payment";
const SPARK_DEPOSITS_NAMESPACE: &str = "deposit";
const SPARK_METADATA_NAMESPACE: &str = "metadata";
const SPARK_SYNC_STATE_NAMESPACE: &str = "sync_state";
const SPARK_SYNC_OUT_NAMESPACE: &str = "sync_out";
const SPARK_SYNC_IN_NAMESPACE: &str = "sync_in";
const SPARK_SYNC_REV_NAMESPACE: &str = "sync_rev";

/// Key used to store the last server-acknowledged revision.
const REVISION_KEY: &str = "revision";
/// Key used to store the local outgoing queue counter.
const LOCAL_REVISION_KEY: &str = "local_revision";

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

/// Create a KV key from a RecordId.
fn record_id_key(id: &RecordId) -> String {
	sanitize_key(format!("{}_{}", id.r#type, id.data_id))
}

/// Serialize a Record to JSON bytes.
fn record_to_bytes(record: &Record) -> Result<Vec<u8>, StorageError> {
	let obj = serde_json::json!({
		"record_type": record.id.r#type,
		"data_id": record.id.data_id,
		"schema_version": record.schema_version,
		"data": record.data,
		"revision": record.revision,
	});
	serde_json::to_vec(&obj).map_err(|e| StorageError::Serialization(format!("{e:?}")))
}

/// Deserialize a Record from JSON bytes.
fn bytes_to_record(bytes: &[u8]) -> Result<Record, StorageError> {
	let v: serde_json::Value =
		serde_json::from_slice(bytes).map_err(|e| StorageError::Serialization(format!("{e:?}")))?;
	Ok(Record {
		id: RecordId::new(
			v["record_type"].as_str().unwrap_or_default().to_string(),
			v["data_id"].as_str().unwrap_or_default().to_string(),
		),
		schema_version: v["schema_version"].as_str().unwrap_or_default().to_string(),
		data: serde_json::from_value(v["data"].clone())
			.map_err(|e| StorageError::Serialization(format!("{e:?}")))?,
		revision: v["revision"].as_u64().unwrap_or(0),
	})
}

impl SparkStore {
	/// Read the sync state for a given RecordId, if it exists.
	async fn read_sync_state(&self, id: &RecordId) -> Result<Option<Record>, StorageError> {
		let key = record_id_key(id);
		match KVStore::read(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_SYNC_STATE_NAMESPACE,
			&key,
		)
		.await
		{
			Ok(bytes) => Ok(Some(bytes_to_record(&bytes)?)),
			Err(e) => {
				if let io::ErrorKind::NotFound = e.kind() {
					Ok(None)
				} else {
					Err(StorageError::Implementation(format!("{e:?}")))
				}
			},
		}
	}

	/// Write/update the sync state for a record.
	async fn write_sync_state(&self, record: &Record) -> Result<(), StorageError> {
		let key = record_id_key(&record.id);
		let data = record_to_bytes(record)?;
		KVStore::write(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_SYNC_STATE_NAMESPACE,
			&key,
			data,
		)
		.await
		.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
		Ok(())
	}

	/// Read a u64 value from the sync_rev namespace.
	async fn read_revision(&self, key: &str) -> Result<u64, StorageError> {
		match KVStore::read(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_SYNC_REV_NAMESPACE, key)
			.await
		{
			Ok(bytes) => {
				let s = String::from_utf8(bytes)
					.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;
				Ok(s.parse::<u64>().unwrap_or(0))
			},
			Err(e) => {
				if let io::ErrorKind::NotFound = e.kind() {
					Ok(0)
				} else {
					Err(StorageError::Implementation(format!("{e:?}")))
				}
			},
		}
	}

	/// Write a u64 value to the sync_rev namespace.
	async fn write_revision(&self, key: &str, value: u64) -> Result<(), StorageError> {
		KVStore::write(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_SYNC_REV_NAMESPACE,
			key,
			value.to_string().into_bytes(),
		)
		.await
		.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
		Ok(())
	}

	/// Update the server revision to the max of current and new.
	async fn update_server_revision(&self, new_revision: u64) -> Result<(), StorageError> {
		let current = self.read_revision(REVISION_KEY).await?;
		if new_revision > current {
			self.write_revision(REVISION_KEY, new_revision).await?;
		}
		Ok(())
	}

	/// Allocate the next local outgoing revision counter.
	async fn next_local_revision(&self) -> Result<u64, StorageError> {
		let current = self.read_revision(LOCAL_REVISION_KEY).await?;
		let next = current + 1;
		self.write_revision(LOCAL_REVISION_KEY, next).await?;
		Ok(next)
	}
}

#[async_trait::async_trait]
impl breez_sdk_spark::Storage for SparkStore {
	async fn delete_cached_item(&self, key: String) -> Result<(), StorageError> {
		let key = sanitize_key(key);
		KVStore::remove(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_CACHE_NAMESPACE,
			&key,
			false,
		)
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
		&self, request: StorageListPaymentsRequest,
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

	async fn insert_payment_metadata(
		&self, payment_id: String, metadata: PaymentMetadata,
	) -> Result<(), StorageError> {
		// Read existing metadata to merge (COALESCE behavior)
		let existing = match KVStore::read(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_METADATA_NAMESPACE,
			&payment_id,
		)
		.await
		{
			Ok(bytes) => serde_json::from_slice::<PaymentMetadata>(&bytes).ok(),
			Err(_) => None,
		};

		let merged = if let Some(existing) = existing {
			PaymentMetadata {
				parent_payment_id: metadata.parent_payment_id.or(existing.parent_payment_id),
				lnurl_pay_info: metadata.lnurl_pay_info.or(existing.lnurl_pay_info),
				lnurl_withdraw_info: metadata.lnurl_withdraw_info.or(existing.lnurl_withdraw_info),
				lnurl_description: metadata.lnurl_description.or(existing.lnurl_description),
				conversion_info: metadata.conversion_info.or(existing.conversion_info),
			}
		} else {
			metadata
		};

		let data = serde_json::to_vec(&merged)
			.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;

		KVStore::write(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_METADATA_NAMESPACE,
			&payment_id,
			data,
		)
		.await
		.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
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
		let payments = self.list_payments(StorageListPaymentsRequest::default()).await?;

		let p = payments.into_iter().find(|p| {
			if let Some(details) = p.details.as_ref() {
				match details {
					PaymentDetails::Spark { invoice_details, .. } => {
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

	async fn get_payments_by_parent_ids(
		&self, parent_payment_ids: Vec<String>,
	) -> Result<HashMap<String, Vec<Payment>>, StorageError> {
		if parent_payment_ids.is_empty() {
			return Ok(HashMap::new());
		}

		let keys =
			KVStore::list(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_METADATA_NAMESPACE)
				.await
				.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		let parent_set: std::collections::HashSet<&str> =
			parent_payment_ids.iter().map(|s| s.as_str()).collect();
		let mut result: HashMap<String, Vec<Payment>> = HashMap::new();

		for key in keys {
			let data = KVStore::read(
				self.0.as_ref(),
				SPARK_PRIMARY_NAMESPACE,
				SPARK_METADATA_NAMESPACE,
				&key,
			)
			.await
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

			if let Ok(metadata) = serde_json::from_slice::<PaymentMetadata>(&data) {
				if let Some(ref parent_id) = metadata.parent_payment_id {
					if parent_set.contains(parent_id.as_str()) {
						// key is the payment_id
						if let Ok(payment) = self.get_payment_by_id(key).await {
							result.entry(parent_id.clone()).or_default().push(payment);
						}
					}
				}
			}
		}

		// Sort by timestamp within each group
		for payments in result.values_mut() {
			payments.sort_by_key(|p| p.timestamp);
		}

		Ok(result)
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
		KVStore::remove(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_DEPOSITS_NAMESPACE,
			&id,
			false,
		)
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

	async fn set_lnurl_metadata(
		&self, _metadata: Vec<SetLnurlMetadataItem>,
	) -> Result<(), StorageError> {
		// we don't use this
		Ok(())
	}

	async fn add_outgoing_change(
		&self, record: UnversionedRecordChange,
	) -> Result<u64, StorageError> {
		let local_revision = self.next_local_revision().await?;

		let entry = serde_json::json!({
			"record_type": record.id.r#type,
			"data_id": record.id.data_id,
			"schema_version": record.schema_version,
			"updated_fields": record.updated_fields,
			"local_revision": local_revision,
		});

		let data = serde_json::to_vec(&entry)
			.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;
		// Zero-pad for correct lexicographic ordering
		let key = format!("{local_revision:020}");

		KVStore::write(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_SYNC_OUT_NAMESPACE,
			&key,
			data,
		)
		.await
		.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		Ok(local_revision)
	}

	async fn complete_outgoing_sync(
		&self, record: Record, local_revision: u64,
	) -> Result<(), StorageError> {
		// Remove the pending outgoing record
		let key = format!("{local_revision:020}");
		let _ = KVStore::remove(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_SYNC_OUT_NAMESPACE,
			&key,
			false,
		)
		.await;

		// Update sync state with the server-acknowledged record
		self.write_sync_state(&record).await?;

		// Update the committed server revision
		self.update_server_revision(record.revision).await?;

		Ok(())
	}

	async fn get_pending_outgoing_changes(
		&self, limit: u32,
	) -> Result<Vec<OutgoingChange>, StorageError> {
		let mut keys =
			KVStore::list(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_SYNC_OUT_NAMESPACE)
				.await
				.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		// Sort ascending by local_revision (zero-padded keys sort correctly)
		keys.sort();
		keys.truncate(limit as usize);

		let mut results = Vec::new();
		for key in keys {
			let data = KVStore::read(
				self.0.as_ref(),
				SPARK_PRIMARY_NAMESPACE,
				SPARK_SYNC_OUT_NAMESPACE,
				&key,
			)
			.await
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

			let v: serde_json::Value = serde_json::from_slice(&data)
				.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;

			let record_type = v["record_type"].as_str().unwrap_or_default().to_string();
			let data_id = v["data_id"].as_str().unwrap_or_default().to_string();

			let change = RecordChange {
				id: RecordId::new(record_type.clone(), data_id.clone()),
				schema_version: v["schema_version"].as_str().unwrap_or_default().to_string(),
				updated_fields: serde_json::from_value(v["updated_fields"].clone())
					.map_err(|e| StorageError::Serialization(format!("{e:?}")))?,
				local_revision: v["local_revision"].as_u64().unwrap_or(0),
			};

			// Get parent state from sync_state if it exists
			let parent = self.read_sync_state(&RecordId::new(record_type, data_id)).await?;

			results.push(OutgoingChange { change, parent });
		}

		Ok(results)
	}

	async fn get_last_revision(&self) -> Result<u64, StorageError> {
		self.read_revision(REVISION_KEY).await
	}

	async fn insert_incoming_records(&self, records: Vec<Record>) -> Result<(), StorageError> {
		for record in records {
			let key = sanitize_key(format!(
				"{}_{}_{}",
				record.id.r#type, record.id.data_id, record.revision
			));
			let data = record_to_bytes(&record)?;

			KVStore::write(
				self.0.as_ref(),
				SPARK_PRIMARY_NAMESPACE,
				SPARK_SYNC_IN_NAMESPACE,
				&key,
				data,
			)
			.await
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
		}
		Ok(())
	}

	async fn delete_incoming_record(&self, record: Record) -> Result<(), StorageError> {
		let key =
			sanitize_key(format!("{}_{}_{}", record.id.r#type, record.id.data_id, record.revision));
		let _ = KVStore::remove(
			self.0.as_ref(),
			SPARK_PRIMARY_NAMESPACE,
			SPARK_SYNC_IN_NAMESPACE,
			&key,
			false,
		)
		.await;
		Ok(())
	}

	async fn get_incoming_records(&self, limit: u32) -> Result<Vec<IncomingChange>, StorageError> {
		let keys = KVStore::list(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_SYNC_IN_NAMESPACE)
			.await
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		// Read all incoming records
		let mut incoming: Vec<Record> = Vec::new();
		for key in keys {
			let data = KVStore::read(
				self.0.as_ref(),
				SPARK_PRIMARY_NAMESPACE,
				SPARK_SYNC_IN_NAMESPACE,
				&key,
			)
			.await
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

			incoming.push(bytes_to_record(&data)?);
		}

		// Sort by revision ascending (server order)
		incoming.sort_by_key(|r| r.revision);
		incoming.truncate(limit as usize);

		// Build IncomingChange with old_state from sync_state
		let mut results = Vec::new();
		for record in incoming {
			let old_state = self.read_sync_state(&record.id).await?;
			results.push(IncomingChange { new_state: record, old_state });
		}

		Ok(results)
	}

	async fn get_latest_outgoing_change(&self) -> Result<Option<OutgoingChange>, StorageError> {
		let mut keys =
			KVStore::list(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_SYNC_OUT_NAMESPACE)
				.await
				.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		if keys.is_empty() {
			return Ok(None);
		}

		// Sort descending to get the most recent
		keys.sort();
		let key = keys.last().unwrap();

		let data =
			KVStore::read(self.0.as_ref(), SPARK_PRIMARY_NAMESPACE, SPARK_SYNC_OUT_NAMESPACE, key)
				.await
				.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		let v: serde_json::Value = serde_json::from_slice(&data)
			.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;

		let record_type = v["record_type"].as_str().unwrap_or_default().to_string();
		let data_id = v["data_id"].as_str().unwrap_or_default().to_string();

		let change = RecordChange {
			id: RecordId::new(record_type.clone(), data_id.clone()),
			schema_version: v["schema_version"].as_str().unwrap_or_default().to_string(),
			updated_fields: serde_json::from_value(v["updated_fields"].clone())
				.map_err(|e| StorageError::Serialization(format!("{e:?}")))?,
			local_revision: v["local_revision"].as_u64().unwrap_or(0),
		};

		let parent = self.read_sync_state(&RecordId::new(record_type, data_id)).await?;

		Ok(Some(OutgoingChange { change, parent }))
	}

	async fn update_record_from_incoming(&self, record: Record) -> Result<(), StorageError> {
		// Update sync_state with the processed record
		self.write_sync_state(&record).await?;

		// Update the committed server revision
		self.update_server_revision(record.revision).await?;

		Ok(())
	}
}
