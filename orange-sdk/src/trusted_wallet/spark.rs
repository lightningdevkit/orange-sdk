//! An implementation of `TrustedWalletInterface` using the Spark SDK.
use crate::bitcoin::consensus::encode::deserialize_hex;
use crate::bitcoin::hex::FromHex;
use crate::bitcoin::{Txid, io};
use crate::logging::Logger;
use crate::store::{PaymentId, StoreTransaction, TxStatus};
use crate::trusted_wallet::{Payment, TrustedError, TrustedWalletInterface};
use crate::{Event, EventQueue, InitFailure, PaymentType, Seed, WalletConfig};

use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::hashes::sha256::Hash as Sha256;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::util::ser::{Readable, Writeable};
use ldk_node::lightning::{log_debug, log_error, log_info};
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::lightning_types::payment::{PaymentHash, PaymentPreimage};
use ldk_node::payment::ConfirmationStatus;

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use spark_wallet::{
	DefaultSigner, Order, PagingFilter, PayLightningInvoiceResult, SparkWallet, SparkWalletConfig,
	SparkWalletError, SspUserRequest, TransferStatus, WalletEvent, WalletTransfer,
};

use tokio::sync::watch;

use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use uuid::Uuid;

/// A wallet implementation using the Breez Spark SDK.
#[derive(Clone)]
pub struct Spark {
	spark_wallet: Arc<SparkWallet<DefaultSigner>>,
	store: Arc<dyn KVStore + Send + Sync>,
	shutdown_sender: watch::Sender<()>,
	logger: Arc<Logger>,
}

impl TrustedWalletInterface for Spark {
	type ExtraConfig = SparkWalletConfig;

	fn init(
		config: &WalletConfig<SparkWalletConfig>, store: Arc<dyn KVStore + Sync + Send>,
		event_queue: Arc<EventQueue>, logger: Arc<Logger>,
	) -> impl Future<Output = Result<Self, InitFailure>> + Send {
		async move {
			if config.network != config.extra_config.network.into() {
				Err(TrustedError::InvalidNetwork)?
			}

			let signer = match &config.seed {
				Seed::Seed64(bytes) => {
					// hash the seed to make sure it does not conflict with the lightning keys
					let seed = Sha256::hash(bytes);
					DefaultSigner::new(&seed[..], config.extra_config.network)
						.map_err(|e| TrustedError::Other(format!("Failed to create signer: {e}")))?
				},
				Seed::Mnemonic { mnemonic, passphrase } => {
					// We don't hash the seed here, as mnemonics are meant to be easily recoverable
					// and if we hashed them, then you could not recover your spark coins from the mnemonic
					// in separate wallets.
					let seed = mnemonic.to_seed(passphrase.as_deref().unwrap_or(""));
					DefaultSigner::new(&seed[..], config.extra_config.network)
						.map_err(|e| TrustedError::Other(format!("Failed to create signer: {e}")))?
				},
			};

			let spark_wallet = Arc::new(
				SparkWallet::connect(config.extra_config.clone(), signer)
					.await
					.map_err(|e| InitFailure::TrustedFailure(e.into()))?,
			);

			let (shutdown_sender, mut shutdown_receiver) = watch::channel::<()>(());
			let mut events = spark_wallet.subscribe_events();
			let l = Arc::clone(&logger);
			let w = Arc::clone(&spark_wallet);
			let s = Arc::clone(&store);
			tokio::spawn(async move {
				loop {
					tokio::select! {
						_ = shutdown_receiver.changed() => {
							log_info!(l, "Deposit tracking loop shutdown signal received");
							return;
						}
						event = events.recv() => {
							match event {
								Ok(event) => {
									log_debug!(l, "Spark event: {event:?}");
									match event {
										WalletEvent::DepositConfirmed(node_id) => {
											if let Ok(transfers) = w.list_transfers(None).await {
												if let Some(transfer) = transfers
													.into_iter()
													.find(|t| t.leaves.iter().any(|l| l.leaf.id == node_id))
												{
													event_queue
														.add_event(Event::OnchainPaymentReceived {
															payment_id: PaymentId::Trusted(
																convert_from_transfer_id(transfer.id.to_bytes()),
															),
															// todo this is kinda hacky, maybe we should make this optional
															txid: transfer
																.leaves
																.iter()
																.find(|t| t.leaf.id == node_id)
																.map(|t| t.leaf
																.node_tx
																.compute_txid())
																.unwrap_or(Txid::all_zeros()),
															amount_sat: transfer.total_value_sat,
															status: ConfirmationStatus::Unconfirmed, // fixme dont have block height
														})
														.unwrap();
												}
											}
										},
										WalletEvent::StreamConnected => {
											log_debug!(l, "Spark wallet stream connected");
										},
										WalletEvent::StreamDisconnected => {
											log_debug!(l, "Spark wallet stream connected");
										},
										WalletEvent::Synced => {
											log_debug!(l, "Spark wallet synced");
											if let Err(e) = Self::sync_payments_to_storage(w.as_ref(), &s, l.as_ref()).await {
												log_error!(l, "Failed to sync payments to storage: {e:?}");
											} else {
												log_info!(l, "Payments synced to storage");
											}
										},
										WalletEvent::TransferClaimed(transfer) => {
											if let Err(e) = Self::sync_payments_to_storage(w.as_ref(), &s, l.as_ref()).await {
												log_error!(l, "Failed to sync payments to storage: {e:?}");
											} else {
												log_info!(l, "Payments synced to storage");
											}

											match transfer.user_request {
												None => {
													log_debug!(l, "Transfer claimed without user request: {transfer:?}");
												},
												Some(SspUserRequest::LightningReceiveRequest(req)) => {
													if let Ok(hash) = FromHex::from_hex(&req.invoice.payment_hash) {
														event_queue
															.add_event(Event::PaymentReceived {
																payment_id: PaymentId::Trusted(convert_from_transfer_id(transfer.id.to_bytes())),
																payment_hash: PaymentHash(hash),
																amount_msat: transfer.total_value_sat * 1_000, // convert to msats
																custom_records: vec![],
																lsp_fee_msats: None,
															})
															.unwrap();
													}
												},
												Some(req) => {
													log_debug!(l, "Transfer claimed with user request: {req:?}");
												}
											}
										},
									}
								},
								Err(e) => {
									log_debug!(l, "Spark event error: {e:?}");
								},
							}
						}
					}
				}
			});

			Ok(Spark { spark_wallet, store, shutdown_sender, logger })
		}
	}

	fn get_balance(&self) -> impl Future<Output = Result<Amount, TrustedError>> + Send {
		async move {
			let sats = self.spark_wallet.get_balance().await?;
			Amount::from_sats(sats).map_err(|_| TrustedError::AmountError)
		}
	}

	fn get_reusable_receive_uri(
		&self,
	) -> impl Future<Output = Result<String, TrustedError>> + Send {
		async move {
			Err(TrustedError::UnsupportedOperation("Spark does not support BOLT 12".to_owned()))
		}
	}

	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> impl Future<Output = Result<Bolt11Invoice, TrustedError>> + Send {
		async move {
			// TODO: get upstream to let us be amount-less
			match amount {
				None => Err(TrustedError::UnsupportedOperation(
					"Spark does not support amount-less invoices".to_owned(),
				)),
				Some(a) if a == Amount::ZERO => Err(TrustedError::UnsupportedOperation(
					"Spark does not support amount-less invoices".to_owned(),
				)),
				Some(a) => {
					let res = self
						.spark_wallet
						.create_lightning_invoice(a.sats_rounding_up(), None)
						.await?;

					Bolt11Invoice::from_str(&res.invoice)
						.map_err(|e| TrustedError::Other(format!("Failed to parse invoice: {e}")))
				},
			}
		}
	}

	fn list_payments(&self) -> impl Future<Output = Result<Vec<Payment>, TrustedError>> + Send {
		async move {
			let keys = self.store.list(SPARK_PRIMARY_NAMESPACE, SPARK_PAYMENTS_NAMESPACE)?;

			let mut res = Vec::with_capacity(keys.len());
			for key in keys {
				let data =
					self.store.read(SPARK_PRIMARY_NAMESPACE, SPARK_PAYMENTS_NAMESPACE, &key)?;
				let mut cursor = io::Cursor::new(data);
				let store_tx: StoreTransaction =
					StoreTransaction::read(&mut cursor).map_err(|e| {
						TrustedError::Other(format!("Failed to decode payment {key}: {e}"))
					})?;
				let uuid = Uuid::from_str(&key).map_err(|e| {
					TrustedError::Other(format!("Failed to parse payment id {key}: {e}"))
				})?;

				// skip any payment without an amount yet
				let amount = match store_tx.amount_msats {
					Some(amount) => {
						Amount::from_milli_sats(amount).map_err(|_| TrustedError::AmountError)?
					},
					None => {
						debug_assert_ne!(
							store_tx.status,
							TxStatus::Completed,
							"Completed payments should have an amount"
						);
						continue;
					},
				};

				// if we have no fee, assume zero
				let fee = match store_tx.fee_msats {
					Some(fee) => {
						Amount::from_milli_sats(fee).map_err(|_| TrustedError::AmountError)?
					},
					None => Amount::ZERO,
				};

				res.push(Payment {
					status: store_tx.status,
					id: convert_from_transfer_id(uuid.into_bytes()),
					amount,
					outbound: store_tx.outbound,
					fee,
					time_since_epoch: Duration::from_secs(store_tx.time_since_epoch),
				});
			}
			Ok(res)
		}
	}

	fn estimate_fee(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<Amount, TrustedError>> + Send {
		async move {
			if let PaymentMethod::LightningBolt11(invoice) = method {
				let fee_sats = self
					.spark_wallet
					.fetch_lightning_send_fee_estimate(
						&invoice.to_string(),
						Some(amount.sats_rounding_up()), // fixme: why do they do sat amounts?
					)
					.await?;

				Amount::from_sats(fee_sats).map_err(|_| TrustedError::AmountError)
			} else {
				log_error!(self.logger, "Only BOLT 11 is currently supported for fee estimation");
				Err(TrustedError::UnsupportedOperation(
					"Only BOLT 11 is currently supported".to_owned(),
				))
			}
		}
	}

	fn pay(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<[u8; 32], TrustedError>> + Send {
		async move {
			if let PaymentMethod::LightningBolt11(invoice) = method {
				let res = self
					.spark_wallet
					.pay_lightning_invoice(
						&invoice.to_string(),
						Some(amount.sats_rounding_up()), // fixme: why do they do sat amounts?
						None,
						true, // prefer spark to make things cheaper
					)
					.await?;

				match res {
					PayLightningInvoiceResult::LightningPayment(pay) => {
						// Spark uses UUIDs for payment IDs, so we need to convert them
						// to our format. Spark uses a UUID in the format `SparkLightningSendRequest:<uuid>`
						// We only need the UUID part, so we split by ':' and take the last part.
						// If the format is invalid, we return an error.
						if let Some(id) = pay.id.split(':').next_back() {
							let uuid = Uuid::from_str(id).map_err(|_| {
								TrustedError::Other(format!("Failed to parse payment id: {id}"))
							})?;
							Ok(convert_from_transfer_id(uuid.into_bytes()))
						} else {
							log_error!(self.logger, "Invalid payment id format: {}", pay.id);
							Err(TrustedError::Other(format!(
								"Invalid payment id format: {}",
								pay.id
							)))
						}
					},
					PayLightningInvoiceResult::Transfer(transfer) => {
						Ok(convert_from_transfer_id(transfer.id.to_bytes()))
					},
				}
			} else {
				Err(TrustedError::UnsupportedOperation(
					"Only BOLT 11 is currently supported".to_owned(),
				))
			}
		}
	}

	fn stop(&self) -> impl Future<Output = ()> + Send {
		async move {
			log_info!(self.logger, "Stopping Spark wallet");
			let _ = self.shutdown_sender.send(());
		}
	}
}

const SPARK_PRIMARY_NAMESPACE: &str = "spark";
const SPARK_SYNC_NAMESPACE: &str = "sync_info";
const SPARK_PAYMENTS_NAMESPACE: &str = "payment";
const SPARK_SYNC_OFFSET_KEY: &str = "sync_offset";

impl Spark {
	/// Synchronizes payments from transfers to persistent storage
	async fn sync_payments_to_storage(
		spark_wallet: &SparkWallet<DefaultSigner>, store: &Arc<dyn KVStore + Send + Sync>,
		logger: &Logger,
	) -> Result<(), TrustedError> {
		// sync payments
		const BATCH_SIZE: u64 = 50;

		// Get the last offset we processed from storage
		let current_offset = match store.read(
			SPARK_PRIMARY_NAMESPACE,
			SPARK_SYNC_NAMESPACE,
			SPARK_SYNC_OFFSET_KEY,
		) {
			Ok(data) => u64::from_be_bytes(data.try_into().map_err(|e| {
				TrustedError::Other(format!("Failed to convert sync offset: {e:?}"))
			})?),
			Err(e) => {
				if e.kind() == io::ErrorKind::NotFound {
					// If not found, start from the beginning
					log_info!(logger, "No sync info found, starting from offset 0");
					0
				} else {
					log_error!(logger, "Failed to read sync info: {e:?}");
					return Err(TrustedError::IOError(e));
				}
			},
		};

		// We'll keep querying in batches until we have all transfers
		let mut next_offset = current_offset;
		let mut has_more = true;
		log_info!(logger, "Syncing payments to storage, offset = {next_offset}");
		let mut pending_payments = 0;
		while has_more {
			// Get batch of transfers starting from current offset
			let transfers_response = spark_wallet
				.list_transfers(Some(PagingFilter::new(
					Some(next_offset),
					Some(BATCH_SIZE),
					Some(Order::Ascending),
				)))
				.await?;

			log_info!(
				logger,
				"Syncing payments to storage, offset = {next_offset}, transfers = {}",
				transfers_response.len()
			);

			// Process transfers in this batch
			for transfer in &transfers_response {
				// Create a payment record
				let store_tx: StoreTransaction = StoreTransaction::try_from(transfer)?;
				// Insert payment into storage
				if let Err(err) = store.write(
					SPARK_PRIMARY_NAMESPACE,
					SPARK_PAYMENTS_NAMESPACE,
					transfer.id.to_string().as_str(),
					&store_tx.encode(),
				) {
					log_error!(logger, "Failed to insert payment: {err:?}");
				}
				if store_tx.status == TxStatus::Pending {
					pending_payments += 1;
				}
				log_info!(logger, "Inserted payment: {store_tx:?}");
			}

			// Check if we have more transfers to fetch
			next_offset = next_offset.saturating_add(transfers_response.len() as u64);
			// Update our last processed offset in the storage. We should remove pending payments
			// from the offset as they might be removed from the list later.
			let saved_offset = next_offset - pending_payments;
			let save_res = store.write(
				SPARK_PRIMARY_NAMESPACE,
				SPARK_SYNC_NAMESPACE,
				SPARK_SYNC_OFFSET_KEY,
				&saved_offset.to_be_bytes(),
			);

			if let Err(err) = save_res {
				log_error!(logger, "Failed to update last sync offset: {err:?}");
			}
			has_more = transfers_response.len() as u64 == BATCH_SIZE;
		}

		Ok(())
	}
}

impl TryFrom<&WalletTransfer> for StoreTransaction {
	type Error = TrustedError;

	fn try_from(transfer: &WalletTransfer) -> Result<Self, TrustedError> {
		let fee_sats: u64 = match &transfer.user_request {
			Some(user_request) => match user_request {
				SspUserRequest::LightningSendRequest(r) => {
					r.fee.as_sats().map_err(|e| TrustedError::Other(format!("{e:?}")))?
				},
				SspUserRequest::CoopExitRequest(r) => {
					r.fee.as_sats().map_err(|e| TrustedError::Other(format!("{e:?}")))?
				},
				SspUserRequest::LeavesSwapRequest(r) => {
					r.fee.as_sats().map_err(|e| TrustedError::Other(format!("{e:?}")))?
				},
				SspUserRequest::ClaimStaticDeposit(_) => 0,
				SspUserRequest::LightningReceiveRequest(_) => 0,
			},
			None => 0,
		};

		let payment_type = transfer
			.user_request
			.as_ref()
			.map(|t| t.try_into())
			.transpose()?
			.unwrap_or(PaymentType::TrustedInternal {});

		Ok(StoreTransaction {
			status: transfer.status.into(),
			outbound: matches!(transfer.direction, spark_wallet::TransferDirection::Outgoing),
			amount_msats: Some(transfer.total_value_sat * 1_000),
			fee_msats: Some(fee_sats * 1000),
			payment_type,
			time_since_epoch: transfer
				.updated_at
				.map(|x| x.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs())
				.unwrap_or(0),
		})
	}
}

impl TryFrom<&SspUserRequest> for PaymentType {
	type Error = SparkWalletError;

	fn try_from(user_request: &SspUserRequest) -> Result<Self, Self::Error> {
		let details = match user_request {
			SspUserRequest::CoopExitRequest(request) => PaymentType::OutgoingOnChain {
				txid: Some(Txid::from_str(&request.coop_exit_txid).map_err(|e| {
					SparkWalletError::Generic(format!("Invalid CoopExitRequest txid: {e}"))
				})?),
			},
			SspUserRequest::LeavesSwapRequest(_) => PaymentType::TrustedInternal {},
			SspUserRequest::LightningReceiveRequest(_) => PaymentType::IncomingLightning {},
			SspUserRequest::LightningSendRequest(request) => {
				let preimage: Option<[u8; 32]> = request
					.lightning_send_payment_preimage
					.as_deref()
					.map(|t| {
						deserialize_hex(t).map_err(|e| {
							SparkWalletError::Generic(format!(
								"Invalid LightningSendRequest preimage: {e}"
							))
						})
					})
					.transpose()?;
				let payment_preimage = preimage.map(PaymentPreimage);
				PaymentType::OutgoingLightningBolt11 { payment_preimage }
			},
			SspUserRequest::ClaimStaticDeposit(request) => PaymentType::IncomingOnChain {
				txid: Some(Txid::from_str(&request.transaction_id).map_err(|e| {
					SparkWalletError::Generic(format!("Invalid CoopExitRequest txid: {e}"))
				})?),
			},
		};
		Ok(details)
	}
}

impl From<TransferStatus> for TxStatus {
	fn from(o: TransferStatus) -> TxStatus {
		match o {
			TransferStatus::SenderInitiated
			| TransferStatus::SenderInitiatedCoordinator
			| TransferStatus::SenderKeyTweakPending
			| TransferStatus::SenderKeyTweaked
			| TransferStatus::ReceiverKeyTweakLocked
			| TransferStatus::ReceiverKeyTweakApplied
			| TransferStatus::ReceiverKeyTweaked => TxStatus::Pending,
			TransferStatus::Completed => TxStatus::Completed,
			TransferStatus::Expired
			| TransferStatus::Returned
			| TransferStatus::ReceiverRefundSigned => TxStatus::Failed,
		}
	}
}

impl From<SparkWalletError> for TrustedError {
	fn from(e: SparkWalletError) -> Self {
		match e {
			SparkWalletError::ValidationError(_) => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::InsufficientFunds => TrustedError::InsufficientFunds,
			SparkWalletError::InvalidNetwork => TrustedError::InvalidNetwork,
			SparkWalletError::InvalidAddress(_) => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::InvalidOutputIndex => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::LeavesNotFound => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::NotADepositOutput => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::SignerServiceError(_) => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::DepositAddressUsed => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::OperatorRpcError(_) => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::OperatorPoolError(_) => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::AddressError(_) => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::TreeServiceError(_) => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::ServiceError(_) => {
				TrustedError::WalletOperationFailed(format!("{e:?}"))
			},
			SparkWalletError::SspError(_) => TrustedError::WalletOperationFailed(format!("{e:?}")),
			SparkWalletError::Generic(str) => TrustedError::Other(str),
		}
	}
}

// spark uses uuid which are only 16 bytes, just pad 0 bytes to the back for ease
fn convert_from_transfer_id(uuid: [u8; 16]) -> [u8; 32] {
	let mut bytes = [0; 32];
	bytes[..16].copy_from_slice(&uuid);
	bytes
}
