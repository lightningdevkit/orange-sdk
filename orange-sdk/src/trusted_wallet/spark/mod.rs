//! An implementation of `TrustedWalletInterface` using the Spark SDK.

pub(crate) mod spark_store;

use crate::bitcoin::Network;
use crate::bitcoin::hex::FromHex;
use crate::logging::Logger;
use crate::store::{PaymentId, TxMetadataStore, TxStatus};
use crate::trusted_wallet::{Payment, TrustedError, TrustedWalletInterface};
use crate::{Event, EventQueue, InitFailure, Seed, WalletConfig};

use crate::dyn_store::DynStore;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::{log_debug, log_error, log_info, log_warn};
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::lightning_types::payment::{PaymentHash, PaymentPreimage};

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use breez_sdk_spark::{
	BreezSdk, EventListener, GetInfoRequest, LeafOptimizationConfig, ListPaymentsRequest,
	PaymentDetails, PaymentRequest, PaymentStatus, PaymentType, PrepareSendPaymentRequest,
	ReceivePaymentMethod, ReceivePaymentRequest, RegisterLightningAddressRequest, SdkBuilder,
	SdkError, SdkEvent, SendPaymentMethod, SendPaymentRequest, custom_storage,
};

use graduated_rebalancer::ReceivedLightningPayment;

use tokio::sync::watch;

use crate::runtime::Runtime;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Configuration options for the Spark wallet.
#[derive(Debug, Clone)]
pub struct SparkWalletConfig {
	/// How often to sync the wallet with the blockchain, in seconds.
	/// Default is 60 seconds.
	pub sync_interval_secs: u32,
	/// When this is set to `true` we will prefer to use spark payments over
	/// lightning when sending and receiving. This has the benefit of lower fees
	/// but is at the cost of privacy.
	pub prefer_spark_over_lightning: bool,
	/// The domain used for receiving through lnurl-pay and lightning address.
	pub lnurl_domain: Option<String>,
}

impl Default for SparkWalletConfig {
	fn default() -> Self {
		SparkWalletConfig {
			sync_interval_secs: 60,
			prefer_spark_over_lightning: false,
			lnurl_domain: Some("breez.tips".to_string()),
		}
	}
}

/// Breez API key for using the Spark SDK. We aren't using any of their services
/// but the SDK requires a valid API key to function.
const BREEZ_API_KEY: &str = "MIIBajCCARygAwIBAgIHPnfOjAhBgzAFBgMrZXAwEDEOMAwGA1UEAxMFQnJlZXowHhcNMjUwOTE5MjEzNTU1WhcNMzUwOTE3MjEzNTU1WjAqMRMwEQYDVQQKEwpvcmFuZ2Utc2RrMRMwEQYDVQQDEwpvcmFuZ2Utc2RrMCowBQYDK2VwAyEA0IP1y98gPByiIMoph1P0G6cctLb864rNXw1LRLOpXXejezB5MA4GA1UdDwEB/wQEAwIFoDAMBgNVHRMBAf8EAjAAMB0GA1UdDgQWBBTaOaPuXmtLDTJVv++VYBiQr9gHCTAfBgNVHSMEGDAWgBTeqtaSVvON53SSFvxMtiCyayiYazAZBgNVHREEEjAQgQ5iZW5Ac3BpcmFsLnh5ejAFBgMrZXADQQCry+1LkA3nrYa1sovS5iFI1Tkpmr/R0nM/4gJtsO93vFOkm3vBEGwjKAV7lrGzFcFbbuyM1wEJPi4Po1XCEG0D";

impl SparkWalletConfig {
	fn into_breez_config(self, network: Network) -> Result<breez_sdk_spark::Config, TrustedError> {
		let network = match network {
			Network::Bitcoin => breez_sdk_spark::Network::Mainnet,
			Network::Regtest => breez_sdk_spark::Network::Regtest,
			_ => return Err(TrustedError::InvalidNetwork),
		};

		Ok(breez_sdk_spark::Config {
			network,
			sync_interval_secs: self.sync_interval_secs,
			prefer_spark_over_lightning: self.prefer_spark_over_lightning,
			external_input_parsers: None,
			use_default_external_input_parsers: false,
			real_time_sync_server_url: None,
			api_key: Some(BREEZ_API_KEY.to_string()),
			max_deposit_claim_fee: None,
			lnurl_domain: self.lnurl_domain,
			private_enabled_default: true,
			leaf_optimization_config: LeafOptimizationConfig {
				auto_enabled: true,
				multiplicity: 1,
			},
			stable_balance_config: None,
			max_concurrent_claims: 4,
			spark_config: None,
			..breez_sdk_spark::default_config(network)
		})
	}
}

/// A wallet implementation using the Breez Spark SDK.
#[derive(Clone)]
pub(crate) struct Spark {
	event_queue: Arc<EventQueue>,
	spark_wallet: Arc<BreezSdk>,
	shutdown_sender: watch::Sender<()>,
	runtime: Arc<Runtime>,
	logger: Arc<Logger>,
}

impl TrustedWalletInterface for Spark {
	fn get_balance(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Amount, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let info = self.spark_wallet.get_info(GetInfoRequest { ensure_synced: None }).await?;
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
					expiry_secs: None,
					payment_hash: None,
					receiver_identity_public_key: None,
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
			let resp = self.spark_wallet.list_payments(ListPaymentsRequest::default()).await?;

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
					payment_request: PaymentRequest::Input { input: invoice.to_string() },
					amount: Some(sats.into()),
					token_identifier: None,
					conversion_options: None,
					fee_policy: None,
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
					payment_request: PaymentRequest::Input { input: invoice.to_string() },
					amount: Some(sats.into()),
					token_identifier: None,
					conversion_options: None,
					fee_policy: None,
				};
				let prepare = self.spark_wallet.prepare_send_payment(params).await?;

				let uuid = Uuid::now_v7();
				// spawn payment send in background since it can take a while and we don't want to block the caller
				let w = Arc::clone(&self.spark_wallet);
				let logger = Arc::clone(&self.logger);
				let event_queue = Arc::clone(&self.event_queue);
				let payment_hash = invoice.payment_hash().0;
				self.runtime.spawn_background_task(async move {
					match w
						.send_payment(SendPaymentRequest {
							prepare_response: prepare,
							options: None,
							idempotency_key: Some(uuid.to_string()),
						})
						.await
					{
						Ok(res) => {
							log_info!(logger, "Payment sent successfully: {res:?}");
							if let Err(e) = deliver_payment_result(&event_queue, &res.payment) {
								log_error!(logger, "Failed to read payment result: {e:?}");
							}
						},
						Err(e) => {
							log_error!(logger, "Failed to send payment: {e:?}");
							event_queue.rebalance_watchers.sent(payment_hash, None);
						},
					}
				});

				Ok(parse_payment_id(&uuid.to_string())?)
			} else {
				Err(TrustedError::UnsupportedOperation(
					"Only BOLT 11 is currently supported".to_owned(),
				))
			}
		})
	}

	fn supports_partial_payments(&self) -> bool {
		// Spark pays the full invoice through the Spark service, so partial MPP payments are not
		// supported.
		false
	}

	fn pay_partial(
		&self, _invoice: Bolt11Invoice, _partial_amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<[u8; 32], TrustedError>> + Send + '_>> {
		Box::pin(async move {
			Err(TrustedError::UnsupportedOperation(
				"Spark wallet does not support partial payments".to_owned(),
			))
		})
	}

	fn get_lightning_address(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Option<String>, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			match self.spark_wallet.get_lightning_address().await? {
				None => Ok(None),
				Some(addr) => Ok(Some(addr.lightning_address)),
			}
		})
	}

	fn register_lightning_address(
		&self, name: String,
	) -> Pin<Box<dyn Future<Output = Result<(), TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let res = self.get_lightning_address().await?;
			if res.is_some() {
				return Err(TrustedError::Other(
					"Wallet already has a lightning address".to_string(),
				));
			}

			let params = RegisterLightningAddressRequest { username: name, description: None };
			self.spark_wallet.register_lightning_address(params).await?;
			Ok(())
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
		config: &WalletConfig, spark_config: SparkWalletConfig, store: Arc<dyn DynStore>,
		event_queue: Arc<EventQueue>, tx_metadata: TxMetadataStore, logger: Arc<Logger>,
		runtime: Arc<Runtime>,
	) -> Result<Self, InitFailure> {
		let spark_config: breez_sdk_spark::Config =
			spark_config.into_breez_config(config.network)?;

		let seed = match &config.seed {
			Seed::Seed64(bytes) => breez_sdk_spark::Seed::Entropy(bytes.to_vec()),
			Seed::Mnemonic { mnemonic, passphrase } => breez_sdk_spark::Seed::Mnemonic {
				mnemonic: mnemonic.to_string(),
				passphrase: passphrase.clone(),
			},
		};

		let spark_store = Arc::new(spark_store::SparkStore::new(store));
		spark_store.migrate_deposit_details().await.map_err(|e| {
			log_error!(logger, "Failed to migrate Spark storage: {e:?}");
			InitFailure::TrustedFailure(SdkError::from(e).into())
		})?;
		let builder =
			SdkBuilder::new(spark_config, seed).with_storage_backend(custom_storage(spark_store));

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

		let listener_id = spark_wallet.add_event_listener(Box::new(listener)).await;
		log_info!(logger, "Added Spark event listener with ID: {listener_id}");
		let w = Arc::clone(&spark_wallet);
		let mut shutdown_recv = shutdown_receiver.clone();
		runtime.spawn_background_task(async move {
			let _ = shutdown_recv.changed().await;
			w.remove_event_listener(&listener_id).await;
		});

		log_info!(logger, "Spark wallet initialized");

		Ok(Spark { spark_wallet, shutdown_sender, event_queue, runtime, logger })
	}
}

struct SparkEventHandler {
	event_queue: Arc<EventQueue>,
	tx_metadata: TxMetadataStore,
	logger: Arc<Logger>,
}

#[async_trait::async_trait]
impl EventListener for SparkEventHandler {
	async fn on_event(&self, event: SdkEvent) {
		match event {
			SdkEvent::Synced => {
				log_debug!(self.logger, "Spark wallet synced");
			},
			SdkEvent::UnclaimedDeposits { unclaimed_deposits } => {
				log_warn!(
					self.logger,
					"Spark wallet failed to claim deposits! {unclaimed_deposits:?}"
				);
			},
			SdkEvent::ClaimedDeposits { claimed_deposits } => {
				log_info!(self.logger, "Spark wallet claimed deposits! {claimed_deposits:?}");
			},
			SdkEvent::PaymentSucceeded { payment } => {
				if let Err(e) = self.handle_payment_succeeded(payment).await {
					log_error!(self.logger, "Failed to handle payment succeeded: {e:?}");
				}
			},
			SdkEvent::PaymentFailed { payment } => {
				if let Err(e) = self.handle_payment_failed(payment).await {
					log_error!(self.logger, "Failed to handle payment succeeded: {e:?}");
				}
			},
			SdkEvent::PaymentPending { payment } => {
				log_debug!(
					self.logger,
					"Spark payment pending event received for payment: {payment:?}"
				);
			},
			SdkEvent::AutoOptimization { optimization_event } => {
				log_debug!(self.logger, "Spark optimization event: {optimization_event:?}");
			},
			SdkEvent::LightningAddressChanged { lightning_address } => {
				log_debug!(self.logger, "Spark lightning address changed: {lightning_address:?}");
			},
			SdkEvent::UnilateralExitStateChanged => {
				log_debug!(self.logger, "Spark unilateral exit state changed");
			},
			SdkEvent::NewDeposits { new_deposits } => {
				log_info!(self.logger, "Spark wallet new deposits: {new_deposits:?}");
			},
		}
	}
}

impl SparkEventHandler {
	async fn handle_payment_succeeded(
		&self, payment: breez_sdk_spark::Payment,
	) -> Result<(), TrustedError> {
		log_info!(self.logger, "Spark payment succeeded: {payment:?}");

		deliver_payment_result(&self.event_queue, &payment)?;

		let id = parse_payment_id(&payment.id)?;
		let fees_msat = fee_paid_msat(&payment);

		match payment.payment_type {
			PaymentType::Send => match payment.details {
				Some(PaymentDetails::Lightning { htlc_details, .. }) => {
					let payment_id = PaymentId::Trusted(id);
					let is_rebalance = {
						let map = self.tx_metadata.read();
						map.get(&payment_id).is_some_and(|m| m.ty.is_rebalance())
					};

					if is_rebalance {
						log_info!(
							self.logger,
							"Ignoring successful payment event for rebalance payment: {payment_id:?}"
						);

						return Ok(());
					}

					let preimage_hex = htlc_details.preimage.ok_or_else(|| {
						TrustedError::Other("Payment succeeded but preimage is missing".to_string())
					})?;

					let preimage: [u8; 32] = FromHex::from_hex(&preimage_hex)
						.map_err(|e| TrustedError::Other(format!("Invalid preimage hex: {e:?}")))?;
					let payment_hash: [u8; 32] = FromHex::from_hex(&htlc_details.payment_hash)
						.map_err(|e| {
							TrustedError::Other(format!("Invalid payment_hash hex: {e:?}"))
						})?;

					if self.tx_metadata.set_preimage(payment_id, preimage).await.is_err() {
						log_error!(
							self.logger,
							"Failed to set preimage for payment {payment_id:?}"
						);
					}

					self.event_queue
						.add_event(Event::PaymentSuccessful {
							payment_id,
							payment_hash: PaymentHash(payment_hash),
							payment_preimage: PaymentPreimage(preimage),
							fee_paid_msat: Some(fees_msat),
						})
						.await?;
				},
				_ => {
					log_debug!(self.logger, "Unsupported payment details for Send: {payment:?}")
				},
			},
			PaymentType::Receive => {
				match payment.details {
					Some(PaymentDetails::Lightning { htlc_details, .. }) => {
						let payment_hash: [u8; 32] = FromHex::from_hex(&htlc_details.payment_hash)
							.map_err(|e| {
								TrustedError::Other(format!("Invalid payment_hash hex: {e:?}"))
							})?;

						let lsp_fee_msats = (payment.fees != 0).then_some(fees_msat);

						self.event_queue
							.add_event(Event::PaymentReceived {
								payment_id: PaymentId::Trusted(id),
								payment_hash: PaymentHash(payment_hash),
								amount_msat: (payment.amount * 1_000) as u64, // convert to msats
								custom_records: vec![],
								lsp_fee_msats,
							})
							.await?;
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

	async fn handle_payment_failed(
		&self, payment: breez_sdk_spark::Payment,
	) -> Result<(), TrustedError> {
		log_info!(self.logger, "Spark payment failed: {payment:?}");
		deliver_payment_result(&self.event_queue, &payment)?;

		let id = parse_payment_id(&payment.id)?;

		match payment.payment_type {
			PaymentType::Send => match payment.details {
				Some(PaymentDetails::Lightning { htlc_details, .. }) => {
					let payment_id = PaymentId::Trusted(id);
					let is_rebalance = {
						let map = self.tx_metadata.read();
						map.get(&payment_id).is_some_and(|m| m.ty.is_rebalance())
					};

					if is_rebalance {
						log_info!(
							self.logger,
							"Ignoring failed payment event for rebalance payment: {payment_id:?}"
						);
						return Ok(());
					}

					let payment_hash: [u8; 32] = FromHex::from_hex(&htlc_details.payment_hash)
						.map_err(|e| {
							TrustedError::Other(format!("Invalid payment_hash hex: {e:?}"))
						})?;

					self.event_queue
						.add_event(Event::PaymentFailed {
							payment_id,
							payment_hash: Some(PaymentHash(payment_hash)),
							reason: None,
						})
						.await?;
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

fn deliver_payment_result(
	event_queue: &EventQueue, payment: &breez_sdk_spark::Payment,
) -> Result<(), TrustedError> {
	if payment.payment_type != PaymentType::Send {
		return Ok(());
	}
	if let Some(PaymentDetails::Lightning { htlc_details, .. }) = &payment.details {
		let hash: [u8; 32] = FromHex::from_hex(&htlc_details.payment_hash)
			.map_err(|e| TrustedError::Other(format!("Invalid payment hash: {e:?}")))?;
		let receipt = match payment.status {
			PaymentStatus::Completed => Some(ReceivedLightningPayment {
				id: parse_payment_id(&payment.id)?,
				fee_paid_msat: Some(fee_paid_msat(payment)),
			}),
			PaymentStatus::Failed => None,
			PaymentStatus::Pending => return Ok(()),
		};
		event_queue.rebalance_watchers.sent(hash, receipt);
	}
	Ok(())
}

fn fee_paid_msat(payment: &breez_sdk_spark::Payment) -> u64 {
	(payment.fees * 1_000) as u64
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

impl TryFrom<breez_sdk_spark::Payment> for Payment {
	type Error = TrustedError;

	fn try_from(value: breez_sdk_spark::Payment) -> Result<Self, Self::Error> {
		let id = parse_payment_id(&value.id)?;

		Ok(Payment {
			id,
			amount: Amount::from_sats(value.amount as u64)
				.map_err(|_| TrustedError::AmountError)?,
			fee: Amount::from_sats(value.fees as u64).map_err(|_| TrustedError::AmountError)?,
			status: value.status.into(),
			outbound: value.payment_type == PaymentType::Send,
			time_since_epoch: Duration::from_secs(value.timestamp),
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::store::{TxMetadata, TxType};
	use crate::test_store::{TestStore, test_event_queue, test_logger, test_runtime};
	use breez_sdk_spark::{SparkHtlcDetails, SparkHtlcStatus};

	fn payment(status: PaymentStatus) -> breez_sdk_spark::Payment {
		breez_sdk_spark::Payment {
			id: "00112233-4455-6677-8899-aabbccddeeff".into(),
			payment_type: PaymentType::Send,
			status,
			amount: 1000,
			fees: 3,
			timestamp: 1,
			method: breez_sdk_spark::PaymentMethod::Lightning,
			conversion_details: None,
			details: Some(PaymentDetails::Lightning {
				description: None,
				invoice: String::new(),
				destination_pubkey: String::new(),
				htlc_details: SparkHtlcDetails {
					payment_hash: "02".repeat(32),
					preimage: None,
					expiry_time: 0,
					status: SparkHtlcStatus::WaitingForPreimage,
				},
				lnurl_pay_info: None,
				lnurl_withdraw_info: None,
				lnurl_receive_metadata: None,
			}),
		}
	}

	#[tokio::test]
	async fn pending_spark_result_waits_for_terminal_status() {
		let store = TestStore::default();
		let tx_metadata = TxMetadataStore::new(store.shared()).await;
		let queue = test_event_queue(&store, tx_metadata, test_runtime()).await;
		let mut wait = queue.rebalance_watchers.register([2; 32]);
		queue
			.rebalance_watchers
			.received([2; 32], Some(ReceivedLightningPayment { id: [3; 32], fee_paid_msat: None }));
		deliver_payment_result(&queue, &payment(PaymentStatus::Pending)).unwrap();
		assert!(
			std::future::poll_fn(|cx| std::task::Poll::Ready(wait.as_mut().poll(cx)))
				.await
				.is_pending()
		);
		let success = payment(PaymentStatus::Completed);
		deliver_payment_result(&queue, &success).unwrap();
		let received = wait.await.unwrap();
		assert_eq!(received.trusted.id, parse_payment_id(&success.id).unwrap());
		assert_eq!(received.trusted.fee_paid_msat, Some(3000));
	}

	#[tokio::test]
	async fn spark_failure_resolves_wait_when_rebalance_event_is_suppressed() {
		let store = TestStore::default();
		let tx_metadata = TxMetadataStore::new(store.shared()).await;
		let failed = payment(PaymentStatus::Failed);
		tx_metadata
			.insert(
				PaymentId::Trusted(parse_payment_id(&failed.id).unwrap()),
				TxMetadata {
					time: Duration::ZERO,
					ty: TxType::PendingRebalance {
						payment_hash: None,
						trigger: None,
						amount_msat: None,
					},
				},
			)
			.await;
		let queue = test_event_queue(&store, tx_metadata.clone(), test_runtime()).await;
		let handler = SparkEventHandler {
			event_queue: Arc::clone(&queue),
			tx_metadata,
			logger: test_logger(),
		};
		let wait = queue.rebalance_watchers.register([2; 32]);
		handler.handle_payment_failed(failed).await.unwrap();
		assert!(tokio::time::timeout(Duration::from_secs(1), wait).await.unwrap().is_none());
		assert!(queue.next_event().is_none());
	}
}
