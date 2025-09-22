//! An implementation of `TrustedWalletInterface` using the Spark SDK.
use crate::bitcoin::hex::FromHex;
use crate::bitcoin::{Network, io};
use crate::logging::Logger;
use crate::store::{PaymentId, TxMetadataStore, TxStatus};
use crate::trusted_wallet::{Payment, TrustedError, TrustedWalletInterface};
use crate::{Event, EventQueue, InitFailure, Seed, WalletConfig};

use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::{log_debug, log_error, log_info, log_warn};
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::lightning_types::payment::{PaymentHash, PaymentPreimage};

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use breez_sdk_spark::{
	BreezSdk, DepositInfo, EventListener, GetInfoRequest, ListPaymentsRequest, PaymentDetails,
	PaymentMetadata, PaymentType, PrepareSendPaymentRequest, ReceivePaymentMethod,
	ReceivePaymentRequest, SdkBuilder, SdkError, SdkEvent, SendPaymentMethod, SendPaymentRequest,
	StorageError, UpdateDepositPayload,
};

use tokio::sync::watch;

use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use uuid::Uuid;

/// Configuration options for the Spark wallet.
#[derive(Debug, Copy, Clone)]
pub struct SparkWalletConfig {
	/// How often to sync the wallet with the blockchain, in seconds.
	/// Default is 60 seconds.
	pub sync_interval_secs: u32,
	/// When this is set to `true` we will prefer to use spark payments over
	/// lightning when sending and receiving. This has the benefit of lower fees
	/// but is at the cost of privacy.
	pub prefer_spark_over_lightning: bool,
}

impl Default for SparkWalletConfig {
	fn default() -> Self {
		SparkWalletConfig { sync_interval_secs: 60, prefer_spark_over_lightning: false }
	}
}

/// Breez API key for using the Spark SDK. We aren't using any of their services
/// but the SDK requires a valid API key to function.
const BREEZ_API_KEY: &str = "MIIBajCCARygAwIBAgIHPnfOjAhBgzAFBgMrZXAwEDEOMAwGA1UEAxMFQnJlZXowHhcNMjUwOTE5MjEzNTU1WhcNMzUwOTE3MjEzNTU1WjAqMRMwEQYDVQQKEwpvcmFuZ2Utc2RrMRMwEQYDVQQDEwpvcmFuZ2Utc2RrMCowBQYDK2VwAyEA0IP1y98gPByiIMoph1P0G6cctLb864rNXw1LRLOpXXejezB5MA4GA1UdDwEB/wQEAwIFoDAMBgNVHRMBAf8EAjAAMB0GA1UdDgQWBBTaOaPuXmtLDTJVv++VYBiQr9gHCTAfBgNVHSMEGDAWgBTeqtaSVvON53SSFvxMtiCyayiYazAZBgNVHREEEjAQgQ5iZW5Ac3BpcmFsLnh5ejAFBgMrZXADQQCry+1LkA3nrYa1sovS5iFI1Tkpmr/R0nM/4gJtsO93vFOkm3vBEGwjKAV7lrGzFcFbbuyM1wEJPi4Po1XCEG0D";

impl SparkWalletConfig {
	fn to_breez_config(self, network: Network) -> Result<breez_sdk_spark::Config, TrustedError> {
		let network = match network {
			Network::Bitcoin => breez_sdk_spark::Network::Mainnet,
			Network::Regtest => breez_sdk_spark::Network::Regtest,
			_ => return Err(TrustedError::InvalidNetwork),
		};

		Ok(breez_sdk_spark::Config {
			network,
			sync_interval_secs: self.sync_interval_secs,
			prefer_spark_over_lightning: self.prefer_spark_over_lightning,
			api_key: Some(BREEZ_API_KEY.to_string()),
			max_deposit_claim_fee: None,
			lnurl_domain: None,
		})
	}
}

/// A wallet implementation using the Breez Spark SDK.
#[derive(Clone)]
pub(crate) struct Spark {
	spark_wallet: Arc<BreezSdk>,
	shutdown_sender: watch::Sender<()>,
	logger: Arc<Logger>,
}

impl TrustedWalletInterface for Spark {
	fn get_balance(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Amount, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let info = self.spark_wallet.get_info(GetInfoRequest {}).await?;
			Amount::from_sats(info.balance_sats).map_err(|_| TrustedError::AmountError)
		})
	}

	fn get_reusable_receive_uri(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<String, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			Err(TrustedError::UnsupportedOperation("Spark does not support BOLT 12".to_owned()))
		})
	}

	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> Pin<Box<dyn Future<Output = Result<Bolt11Invoice, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			// check amount is not msat value
			let amount_sats = match amount {
				Some(a) => {
					let sats = a.sats().map_err(|_| {
						TrustedError::UnsupportedOperation(
							"msat amounts not supported by spark".to_owned(),
						)
					})?;
					Some(sats)
				},
				None => None,
			};

			let params = ReceivePaymentRequest {
				payment_method: ReceivePaymentMethod::Bolt11Invoice {
					description: "".to_string(), // empty description for smaller QRs and better privacy
					amount_sats,
				},
			};
			let res = self.spark_wallet.receive_payment(params).await?;

			Bolt11Invoice::from_str(&res.payment_request)
				.map_err(|e| TrustedError::Other(format!("Failed to parse invoice: {e}")))
		})
	}

	fn list_payments(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Vec<Payment>, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let resp = self
				.spark_wallet
				.list_payments(ListPaymentsRequest { limit: None, offset: None })
				.await?;

			let payments =
				resp.payments.into_iter().map(|p| p.try_into()).collect::<Result<_, _>>()?;

			Ok(payments)
		})
	}

	fn estimate_fee(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<Amount, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			if let PaymentMethod::LightningBolt11(invoice) = method {
				let sats = amount.sats().map_err(|_| {
					TrustedError::UnsupportedOperation(
						"msat amounts not supported by spark".to_owned(),
					)
				})?;

				let params = PrepareSendPaymentRequest {
					payment_request: invoice.to_string(),
					amount_sats: Some(sats),
				};
				let prepare = self.spark_wallet.prepare_send_payment(params).await?;
				match prepare.payment_method {
					SendPaymentMethod::Bolt11Invoice { lightning_fee_sats, .. } => {
						Amount::from_sats(lightning_fee_sats).map_err(|_| TrustedError::AmountError)
					},
					_ => unreachable!("we only asked for bolt11"),
				}
			} else {
				log_error!(self.logger, "Only BOLT 11 is currently supported for fee estimation");
				Err(TrustedError::UnsupportedOperation(
					"Only BOLT 11 is currently supported".to_owned(),
				))
			}
		})
	}

	fn pay(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<[u8; 32], TrustedError>> + Send + '_>> {
		Box::pin(async move {
			if let PaymentMethod::LightningBolt11(invoice) = method {
				let sats = amount.sats().map_err(|_| {
					TrustedError::UnsupportedOperation(
						"msat amounts not supported by spark".to_owned(),
					)
				})?;

				let params = PrepareSendPaymentRequest {
					payment_request: invoice.to_string(),
					amount_sats: Some(sats),
				};
				let prepare = self.spark_wallet.prepare_send_payment(params).await?;

				let res = self
					.spark_wallet
					.send_payment(SendPaymentRequest { prepare_response: prepare, options: None })
					.await?;

				let id = parse_payment_id(&res.payment.id)?;
				Ok(id)
			} else {
				Err(TrustedError::UnsupportedOperation(
					"Only BOLT 11 is currently supported".to_owned(),
				))
			}
		})
	}

	fn stop(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		Box::pin(async move {
			log_info!(self.logger, "Stopping Spark wallet");
			let _ = self.shutdown_sender.send(());
		})
	}
}

impl Spark {
	/// Initialize a new Spark wallet instance with the given configuration.
	pub(crate) async fn init(
		config: &WalletConfig, spark_config: SparkWalletConfig,
		store: Arc<dyn KVStore + Sync + Send>, event_queue: Arc<EventQueue>,
		tx_metadata: TxMetadataStore, logger: Arc<Logger>, runtime: Arc<Runtime>,
	) -> Result<Self, InitFailure> {
		let spark_config: breez_sdk_spark::Config = spark_config.to_breez_config(config.network)?;

		let seed = match &config.seed {
			Seed::Seed64(bytes) => breez_sdk_spark::Seed::Entropy(bytes.to_vec()),
			Seed::Mnemonic { mnemonic, passphrase } => breez_sdk_spark::Seed::Mnemonic {
				mnemonic: mnemonic.to_string(),
				passphrase: passphrase.clone(),
			},
		};

		let spark_store = SparkStore(store);
		let builder = SdkBuilder::new(spark_config, seed, Arc::new(spark_store));

		let spark_wallet = Arc::new(builder.build().await.map_err(|e| {
			log_error!(logger, "Failed to initialize Spark wallet: {e:?}");
			InitFailure::TrustedFailure(e.into())
		})?);

		log_info!(logger, "Started Spark wallet!");

		let (shutdown_sender, shutdown_receiver) = watch::channel::<()>(());
		let listener = SparkEventHandler {
			event_queue: Arc::clone(&event_queue),
			tx_metadata,
			logger: Arc::clone(&logger),
		};

		let listener_id = spark_wallet.add_event_listener(Box::new(listener));
		log_info!(logger, "Added Spark event listener with ID: {}", listener_id);
		let w = Arc::clone(&spark_wallet);
		let mut shutdown_recv = shutdown_receiver.clone();
		runtime.spawn(async move {
			let _ = shutdown_recv.changed().await;
			w.remove_event_listener(&listener_id);
		});

		log_info!(logger, "Spark wallet initialized");

		Ok(Spark { spark_wallet, shutdown_sender, logger })
	}
}

struct SparkEventHandler {
	event_queue: Arc<EventQueue>,
	#[allow(unused)] // will be used in future events
	tx_metadata: TxMetadataStore,
	logger: Arc<Logger>,
}

impl EventListener for SparkEventHandler {
	fn on_event(&self, event: SdkEvent) {
		match event {
			SdkEvent::Synced => {
				log_debug!(self.logger, "Spark wallet synced");
			},
			SdkEvent::ClaimDepositsFailed { unclaimed_deposits } => {
				log_warn!(
					self.logger,
					"Spark wallet failed to claim deposits! {unclaimed_deposits:?}"
				);
			},
			SdkEvent::ClaimDepositsSucceeded { claimed_deposits } => {
				log_info!(self.logger, "Spark wallet claimed deposits! {claimed_deposits:?}");
			},
			SdkEvent::PaymentSucceeded { payment } => {
				if let Err(e) = self.handle_payment_succeeded(payment) {
					log_error!(self.logger, "Failed to handle payment succeeded: {e:?}");
				}
			},
			SdkEvent::PaymentFailed { payment } => {
				if let Err(e) = self.handle_payment_failed(payment) {
					log_error!(self.logger, "Failed to handle payment succeeded: {e:?}");
				}
			},
		}
	}
}

impl SparkEventHandler {
	fn handle_payment_succeeded(
		&self, payment: breez_sdk_spark::Payment,
	) -> Result<(), TrustedError> {
		log_info!(self.logger, "Spark payment succeeded: {payment:?}");

		let id = parse_payment_id(&payment.id)?;

		match payment.payment_type {
			PaymentType::Send => {
				match payment.details {
					Some(PaymentDetails::Lightning { preimage, payment_hash, .. }) => {
						let preimage = preimage.ok_or_else(|| {
							TrustedError::Other(
								"Payment succeeded but preimage is missing".to_string(),
							)
						})?;

						let preimage: [u8; 32] = FromHex::from_hex(&preimage).map_err(|e| {
							TrustedError::Other(format!("Invalid preimage hex: {e:?}"))
						})?;
						let payment_hash: [u8; 32] =
							FromHex::from_hex(&payment_hash).map_err(|e| {
								TrustedError::Other(format!("Invalid payment_hash hex: {e:?}"))
							})?;

						self.event_queue.add_event(Event::PaymentSuccessful {
							payment_id: PaymentId::Trusted(id),
							payment_hash: PaymentHash(payment_hash),
							payment_preimage: PaymentPreimage(preimage),
							fee_paid_msat: Some(payment.fees * 1_000), // convert to msats
						})?;
					},
					_ => {
						log_debug!(self.logger, "Unsupported payment details for Send: {payment:?}")
					},
				}
			},
			PaymentType::Receive => {
				match payment.details {
					Some(PaymentDetails::Lightning { payment_hash, .. }) => {
						let payment_hash: [u8; 32] =
							FromHex::from_hex(&payment_hash).map_err(|e| {
								TrustedError::Other(format!("Invalid payment_hash hex: {e:?}"))
							})?;

						let lsp_fee_msats = if payment.fees == 0 {
							None
						} else {
							Some(payment.fees * 1_000) // convert to msats
						};

						self.event_queue.add_event(Event::PaymentReceived {
							payment_id: PaymentId::Trusted(id),
							payment_hash: PaymentHash(payment_hash),
							amount_msat: payment.amount * 1_000, // convert to msats
							custom_records: vec![],
							lsp_fee_msats,
						})?;
					},
					_ => {
						log_debug!(
							self.logger,
							"Unsupported payment details for Receive: {payment:?}"
						)
					},
				}
			},
		}

		Ok(())
	}

	fn handle_payment_failed(&self, payment: breez_sdk_spark::Payment) -> Result<(), TrustedError> {
		log_info!(self.logger, "Spark payment failed: {payment:?}");

		let id = parse_payment_id(&payment.id)?;

		match payment.payment_type {
			PaymentType::Send => match payment.details {
				Some(PaymentDetails::Lightning { payment_hash, .. }) => {
					let payment_hash: [u8; 32] = FromHex::from_hex(&payment_hash).map_err(|e| {
						TrustedError::Other(format!("Invalid payment_hash hex: {e:?}"))
					})?;

					self.event_queue.add_event(Event::PaymentFailed {
						payment_id: PaymentId::Trusted(id),
						payment_hash: Some(PaymentHash(payment_hash)),
						reason: None,
					})?;
				},
				_ => {
					log_debug!(self.logger, "Unsupported payment details for Send: {payment:?}")
				},
			},
			PaymentType::Receive => {
				log_debug!(self.logger, "Receive payments cannot fail: {payment:?}");
			},
		}

		Ok(())
	}
}

fn parse_payment_id(id: &str) -> Result<[u8; 32], TrustedError> {
	// Spark uses UUIDs for payment IDs, so we need to convert them
	// to our format. Spark uses a UUID in the format `SparkLightningSendRequest:<uuid>`
	// We only need the UUID part, so we split by ':' and take the last part.
	// If the format is invalid, we return an error.
	let uuid = if let Some(id) = id.split(':').next_back() {
		Uuid::from_str(id)
			.map_err(|_| TrustedError::Other(format!("Failed to parse payment id: {id}")))?
	} else {
		// if it's not in the expected format, try to parse the whole thing as a uuid
		Uuid::from_str(id)
			.map_err(|_| TrustedError::Other(format!("Failed to parse payment id: {id}")))?
	};
	Ok(convert_from_uuid_id(uuid.into_bytes()))
}

// spark uses uuid which are only 16 bytes, just pad 0 bytes to the back for ease
fn convert_from_uuid_id(uuid: [u8; 16]) -> [u8; 32] {
	let mut bytes = [0; 32];
	bytes[..16].copy_from_slice(&uuid);
	bytes
}

impl From<breez_sdk_spark::PaymentStatus> for TxStatus {
	fn from(o: breez_sdk_spark::PaymentStatus) -> TxStatus {
		match o {
			breez_sdk_spark::PaymentStatus::Pending => TxStatus::Pending,
			breez_sdk_spark::PaymentStatus::Completed => TxStatus::Completed,
			breez_sdk_spark::PaymentStatus::Failed => TxStatus::Failed,
		}
	}
}

impl From<SdkError> for TrustedError {
	fn from(e: SdkError) -> Self {
		TrustedError::WalletOperationFailed(format!("{e:?}"))
	}
}

const SPARK_PRIMARY_NAMESPACE: &str = "spark";
const SPARK_CACHE_NAMESPACE: &str = "cache";
const SPARK_PAYMENTS_NAMESPACE: &str = "payment";
const SPARK_DEPOSITS_NAMESPACE: &str = "deposit";

#[derive(Clone)]
struct SparkStore(Arc<dyn KVStore + Send + Sync>);

#[async_trait::async_trait]
impl breez_sdk_spark::Storage for SparkStore {
	async fn delete_cached_item(&self, key: String) -> Result<(), StorageError> {
		self.0
			.remove(SPARK_PRIMARY_NAMESPACE, SPARK_CACHE_NAMESPACE, &key, false)
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
		Ok(())
	}

	async fn get_cached_item(&self, key: String) -> Result<Option<String>, StorageError> {
		match self.0.read(SPARK_PRIMARY_NAMESPACE, SPARK_CACHE_NAMESPACE, &key) {
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
		self.0
			.write(SPARK_PRIMARY_NAMESPACE, SPARK_CACHE_NAMESPACE, &key, value.as_bytes())
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
		Ok(())
	}

	async fn list_payments(
		&self, offset: Option<u32>, limit: Option<u32>,
	) -> Result<Vec<breez_sdk_spark::Payment>, StorageError> {
		let keys = self
			.0
			.list(SPARK_PRIMARY_NAMESPACE, SPARK_PAYMENTS_NAMESPACE)
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		let mut payments = Vec::with_capacity(keys.len());
		for key in keys {
			let data = self
				.0
				.read(SPARK_PRIMARY_NAMESPACE, SPARK_PAYMENTS_NAMESPACE, &key)
				.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

			let payment: breez_sdk_spark::Payment = serde_json::from_slice(&data)
				.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;
			payments.push(payment);
		}
		// sort
		payments.sort_by_key(|p| p.timestamp);

		// apply offset and limit
		let start = offset.unwrap_or(0) as usize;
		let end = if let Some(l) = limit {
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

		self.0
			.write(SPARK_PRIMARY_NAMESPACE, SPARK_PAYMENTS_NAMESPACE, &payment.id, &data)
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
		let data = self
			.0
			.read(SPARK_PRIMARY_NAMESPACE, SPARK_PAYMENTS_NAMESPACE, &id)
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		let payment: breez_sdk_spark::Payment = serde_json::from_slice(&data)
			.map_err(|e| StorageError::Serialization(format!("{e:?}")))?;
		Ok(payment)
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

		self.0
			.write(SPARK_PRIMARY_NAMESPACE, SPARK_DEPOSITS_NAMESPACE, &id, &data)
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		Ok(())
	}

	async fn delete_deposit(&self, txid: String, vout: u32) -> Result<(), StorageError> {
		let id = format!("{txid}:{vout}");
		self.0
			.remove(SPARK_PRIMARY_NAMESPACE, SPARK_DEPOSITS_NAMESPACE, &id, false)
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;
		Ok(())
	}

	async fn list_deposits(&self) -> Result<Vec<DepositInfo>, StorageError> {
		let keys = self
			.0
			.list(SPARK_PRIMARY_NAMESPACE, SPARK_DEPOSITS_NAMESPACE)
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		let mut deposits = Vec::with_capacity(keys.len());
		for key in keys {
			let data = self
				.0
				.read(SPARK_PRIMARY_NAMESPACE, SPARK_DEPOSITS_NAMESPACE, &key)
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

		let data = match self.0.read(SPARK_PRIMARY_NAMESPACE, SPARK_DEPOSITS_NAMESPACE, &id) {
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

		self.0
			.write(SPARK_PRIMARY_NAMESPACE, SPARK_DEPOSITS_NAMESPACE, &id, &data)
			.map_err(|e| StorageError::Implementation(format!("{e:?}")))?;

		Ok(())
	}
}

impl TryFrom<breez_sdk_spark::Payment> for Payment {
	type Error = TrustedError;

	fn try_from(value: breez_sdk_spark::Payment) -> Result<Self, Self::Error> {
		let id = parse_payment_id(&value.id)?;

		Ok(Payment {
			id,
			amount: Amount::from_sats(value.amount).map_err(|_| TrustedError::AmountError)?,
			fee: Amount::from_sats(value.fees).map_err(|_| TrustedError::AmountError)?,
			status: value.status.into(),
			outbound: value.payment_type == PaymentType::Send,
			time_since_epoch: Duration::from_secs(value.timestamp),
		})
	}
}
