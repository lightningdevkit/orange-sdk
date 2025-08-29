//! An implementation of `TrustedWalletInterface` using the Cashu (CDK) SDK.

use crate::logging::Logger;
use crate::store::{PaymentId, TxStatus};
use crate::trusted_wallet::{Payment, TrustedError, TrustedWalletInterface};
use crate::{Event, EventQueue, InitFailure, Seed, WalletConfig};

use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::hashes::sha256::Hash as Sha256;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::{log_error, log_info};
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::lightning_types::payment::PaymentHash;

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use cdk::amount::SplitTarget;
use cdk::nuts::CurrencyUnit;
use cdk::nuts::MeltOptions;
use cdk::nuts::nut23::Amountless;
use cdk::wallet::MintQuote;
use cdk::wallet::Wallet;
use cdk::wallet::types::{Transaction, TransactionDirection};
use cdk::{Amount as CdkAmount, StreamExt};

use tokio::sync::{mpsc, watch};

use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

/// Cashu KV store implementation
pub mod cashu_store;

use cashu_store::CashuKvDatabase;

/// Configuration for the Cashu wallet
#[derive(Debug, Clone)]
pub struct CashuConfig {
	/// The mint URL to connect to
	pub mint_url: String,
	/// The currency unit to use (typically Sat)
	pub unit: CurrencyUnit,
}

/// A wallet implementation using the Cashu (CDK) SDK.
#[derive(Clone)]
pub struct Cashu {
	cashu_wallet: Arc<Wallet>,
	shutdown_sender: watch::Sender<()>,
	logger: Arc<Logger>,
	supports_bolt12: bool,
	mint_quote_sender: mpsc::Sender<MintQuote>,
}

impl TrustedWalletInterface for Cashu {
	fn get_balance(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Amount, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let balance = self.cashu_wallet.total_balance().await.map_err(|e| {
				TrustedError::WalletOperationFailed(format!("Failed to get balance: {e}"))
			})?;

			Amount::from_sats(balance.into()).map_err(|_| TrustedError::AmountError)
		})
	}

	fn get_reusable_receive_uri(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<String, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			if !self.supports_bolt12 {
				return Err(TrustedError::UnsupportedOperation(
					"Cashu mint does not support BOLT 12".to_owned(),
				));
			}

			let mint_quote =
				self.cashu_wallet.mint_bolt12_quote(None, None).await.map_err(|e| {
					TrustedError::WalletOperationFailed(format!("Failed to create mint quote: {e}"))
				})?;

			// Send the quote to monitoring channel - if it fails, log but don't fail the operation
			if let Err(e) = self.mint_quote_sender.send(mint_quote.clone()).await {
				log_error!(
					self.logger,
					"Failed to send mint quote {} for monitoring: {e}",
					mint_quote.id
				);
			}

			Ok(mint_quote.request)
		})
	}

	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> Pin<Box<dyn Future<Output = Result<Bolt11Invoice, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			match amount {
				None => Err(TrustedError::UnsupportedOperation(
					"Cashu does not support amount-less invoices".to_owned(),
				)),
				Some(a) if a == Amount::ZERO => Err(TrustedError::UnsupportedOperation(
					"Cashu does not support amount-less invoices".to_owned(),
				)),
				Some(a) => {
					let cdk_amount = CdkAmount::from(a.sats_rounding_up());
					let quote =
						self.cashu_wallet.mint_quote(cdk_amount, None).await.map_err(|e| {
							TrustedError::WalletOperationFailed(format!(
								"Failed to create mint quote: {e}"
							))
						})?;

					// Get the invoice from the quote
					let invoice = Bolt11Invoice::from_str(&quote.request).map_err(|e| {
						TrustedError::Other(format!("Failed to parse invoice: {e}"))
					})?;

					// Send the quote to monitoring channel - if it fails, log but don't fail the operation
					let id = quote.id.clone();
					if let Err(e) = self.mint_quote_sender.send(quote).await {
						log_error!(
							self.logger,
							"Failed to send mint quote {id} for monitoring: {e}",
						);
					}

					Ok(invoice)
				},
			}
		})
	}

	fn list_payments(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Vec<Payment>, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let transactions = self.cashu_wallet.list_transactions(None).await.map_err(|e| {
				TrustedError::WalletOperationFailed(format!("Failed to list transactions: {e}"))
			})?;

			// Convert CDK Transaction to Payment
			let payments = transactions
				.into_iter()
				.map(Self::convert_transaction_to_payment)
				.collect::<Result<Vec<_>, _>>()?;

			Ok(payments)
		})
	}

	fn estimate_fee(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<Amount, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let melt_options = Some(MeltOptions::Amountless {
				amountless: Amountless { amount_msat: amount.milli_sats().into() },
			});

			match method {
				PaymentMethod::LightningBolt11(invoice) => {
					let quote = self
						.cashu_wallet
						.melt_quote(invoice.to_string(), melt_options)
						.await
						.map_err(|e| {
							TrustedError::WalletOperationFailed(format!(
								"Failed to get melt quote: {e}"
							))
						})?;

					// The fee is in the quote
					let fee_sats: u64 = quote.fee_reserve.into();
					Amount::from_sats(fee_sats).map_err(|_| TrustedError::AmountError)
				},
				PaymentMethod::LightningBolt12(offer) => {
					let quote = self
						.cashu_wallet
						.melt_bolt12_quote(offer.to_string(), melt_options)
						.await
						.map_err(|e| {
							TrustedError::WalletOperationFailed(format!(
								"Failed to get melt quote: {e}"
							))
						})?;

					// The fee is in the quote
					let fee_sats: u64 = quote.fee_reserve.into();
					Amount::from_sats(fee_sats).map_err(|_| TrustedError::AmountError)
				},
				PaymentMethod::OnChain(_) => Err(TrustedError::UnsupportedOperation(
					"Cashu mint does not support onchain".to_owned(),
				)),
			}
		})
	}

	fn pay(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<[u8; 32], TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let melt_options = Some(MeltOptions::Amountless {
				amountless: Amountless { amount_msat: amount.milli_sats().into() },
			});

			let quote = match method {
				PaymentMethod::LightningBolt11(invoice) => {
					// Create a melt quote
					self.cashu_wallet.melt_quote(invoice.to_string(), melt_options).await.map_err(
						|e| {
							TrustedError::WalletOperationFailed(format!(
								"Failed to create melt quote: {e}"
							))
						},
					)?
				},
				PaymentMethod::LightningBolt12(offer) => {
					if !self.supports_bolt12 {
						return Err(TrustedError::UnsupportedOperation(
							"Cashu mint does not support BOLT 12".to_owned(),
						));
					}

					self.cashu_wallet
						.melt_bolt12_quote(offer.to_string(), melt_options)
						.await
						.map_err(|e| {
							TrustedError::WalletOperationFailed(format!(
								"Failed to create melt quote: {e}"
							))
						})?
				},
				PaymentMethod::OnChain(_) => {
					return Err(TrustedError::UnsupportedOperation(
						"Cashu mint does not support onchain".to_owned(),
					));
				},
			};

			// Execute the melt
			let _melt_response = self.cashu_wallet.melt(&quote.id).await.map_err(|e| {
				TrustedError::WalletOperationFailed(format!("Failed to execute melt: {e}"))
			})?;

			// Convert quote ID to a 32-byte array for consistency
			// We'll use the quote ID as the payment identifier
			let payment_id = Self::id_to_32_byte_array(&quote.id);

			Ok(payment_id)
		})
	}

	fn stop(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		Box::pin(async move {
			log_info!(self.logger, "Stopping Cashu wallet");
			let _ = self.shutdown_sender.send(());
		})
	}
}

impl Cashu {
	pub(crate) async fn init(
		config: &WalletConfig, cashu_config: CashuConfig, store: Arc<dyn KVStore + Sync + Send>,
		event_queue: Arc<EventQueue>, logger: Arc<Logger>,
	) -> Result<Self, InitFailure> {
		// Create the seed from the configuration
		let seed: [u8; 64] = match &config.seed {
			Seed::Seed64(bytes) => {
				// Hash the seed to make sure it does not conflict with the lightning keys
				let seed = Sha256::hash(bytes);
				let mut seed_array = [0u8; 64];
				// Copy the 32-byte hash twice to fill 64 bytes
				seed_array[..32].copy_from_slice(seed.as_byte_array());
				seed_array[32..].copy_from_slice(seed.as_byte_array());
				seed_array
			},
			Seed::Mnemonic { mnemonic, passphrase } => {
				// Use the mnemonic directly as seed
				mnemonic.to_seed(passphrase.as_deref().unwrap_or(""))
			},
		};

		let db = Arc::new(CashuKvDatabase::new(store).map_err(|e| {
			InitFailure::TrustedFailure(TrustedError::Other(format!(
				"Failed to create Cashu database: {e}"
			)))
		})?);

		// Create the Cashu wallet
		let cashu_wallet = Arc::new(
			Wallet::new(&cashu_config.mint_url, cashu_config.unit, db, seed, None).map_err(
				|e| {
					InitFailure::TrustedFailure(TrustedError::Other(format!(
						"Failed to create Cashu wallet: {e}"
					)))
				},
			)?,
		);

		let supports_bolt12 = cashu_wallet
			.fetch_mint_info()
			.await
			.ok()
			.flatten()
			.map(|info| {
				info.nuts.nut04.supported_methods().contains(&&cdk::nuts::PaymentMethod::Bolt12)
			})
			.unwrap_or(false);

		let (shutdown_sender, mut shutdown_receiver) = watch::channel::<()>(());

		// Create channel for mint quote monitoring with bounded capacity
		let (mint_quote_sender, mut mint_quote_receiver) = mpsc::channel::<MintQuote>(32);

		// Start mint quote monitoring task
		let wallet_for_monitoring = Arc::clone(&cashu_wallet);
		let logger_for_monitoring = Arc::clone(&logger);
		tokio::spawn(async move {
			loop {
				tokio::select! {
					_ = shutdown_receiver.changed() => {
						log_info!(logger_for_monitoring, "Mint quote monitoring loop shutdown signal received");
						return;
					}
					Some(mint_quote) = mint_quote_receiver.recv() => {
						log_info!(logger_for_monitoring, "Received mint quote for monitoring: {}", mint_quote.id);

						// Start monitoring this quote
						let wallet = Arc::clone(&wallet_for_monitoring);
						let event_queue = Arc::clone(&event_queue);
						let logger = Arc::clone(&logger_for_monitoring);
						tokio::spawn(async move {
							if let Err(e) = Self::monitor_mint_quote(wallet, event_queue, &logger, mint_quote).await {
								log_error!(logger, "Failed to monitor mint quote: {e:?}");
							}
						});
					}
				}
			}
		});

		if let Ok(pending_mints) = cashu_wallet.get_active_mint_quotes().await {
			for pending_mint in pending_mints {
				let id = pending_mint.id.clone();
				if let Err(e) = mint_quote_sender.send(pending_mint).await {
					log_error!(
						logger,
						"Failed to send pending mint quote {id} for monitoring: {e}",
					);
				}
			}
		}

		Ok(Cashu { cashu_wallet, shutdown_sender, logger, supports_bolt12, mint_quote_sender })
	}

	/// Convert an ID string to a 32-byte array
	///
	/// This is a helper function to avoid code duplication when converting various ID types
	/// (transaction IDs, quote IDs, etc.) to a fixed-size 32-byte array for consistency.
	fn id_to_32_byte_array(id: &str) -> [u8; 32] {
		let mut id_array = [0u8; 32];
		let id_bytes = id.as_bytes();
		let copy_len = std::cmp::min(id_bytes.len(), 32);
		id_array[..copy_len].copy_from_slice(&id_bytes[..copy_len]);
		id_array
	}

	/// Convert a CDK Transaction to a Payment struct
	fn convert_transaction_to_payment(transaction: Transaction) -> Result<Payment, TrustedError> {
		// Convert transaction ID to a 32-byte array
		let payment_id = Self::id_to_32_byte_array(&transaction.id().to_string());

		// Convert amounts - CDK amounts are u64 representing sats
		let amount =
			Amount::from_sats(transaction.amount.into()).map_err(|_| TrustedError::AmountError)?;
		let fee =
			Amount::from_sats(transaction.fee.into()).map_err(|_| TrustedError::AmountError)?;

		let outbound = transaction.direction == TransactionDirection::Outgoing;

		// For Cashu, we'll assume all completed transactions are successful
		// and all others are pending. CDK doesn't have a direct status mapping.
		let status = TxStatus::Completed; // Assume completed since it's in the transaction list

		// Convert timestamp to Duration since epoch
		let time_since_epoch = Duration::from_secs(transaction.timestamp);

		Ok(Payment { id: payment_id, amount, fee, status, outbound, time_since_epoch })
	}

	/// Monitor a mint quote and automatically mint tokens when the quote is paid
	async fn monitor_mint_quote(
		wallet: Arc<Wallet>, event_queue: Arc<EventQueue>, logger: &Logger, mint_quote: MintQuote,
	) -> Result<(), TrustedError> {
		log_info!(logger, "Starting monitoring for mint quote: {}", mint_quote.id);

		// Wait for the mint quote to be paid and mint the tokens
		let mut stream = wallet.proof_stream(mint_quote.clone(), SplitTarget::default(), None);
		while let Some(proofs) = stream.next().await {
			let proofs =
				proofs.map_err(|e| TrustedError::Other(format!("Failed mint proofs: {e}")))?;
			log_info!(
				logger,
				"Successfully minted {} proofs for quote: {}",
				proofs.len(),
				mint_quote.id
			);

			// Convert quote ID to a 32-byte payment ID
			let payment_id = Self::id_to_32_byte_array(&mint_quote.id);

			// Create a payment hash from the quote ID
			// fixme: can we get the actual payment hash?
			let hash = Sha256::hash(mint_quote.id.as_bytes());

			// Send a PaymentReceived event
			event_queue
				.add_event(Event::PaymentReceived {
					payment_id: PaymentId::Trusted(payment_id),
					payment_hash: PaymentHash(hash.to_byte_array()),
					amount_msat: u64::from(mint_quote.amount.unwrap_or_default()) * 1_000, // convert to msats
					custom_records: vec![],
					lsp_fee_msats: None,
				})
				.map_err(|e| TrustedError::Other(format!("Failed to add event: {e}")))?;

			log_info!(logger, "Sent PaymentReceived event for mint quote: {}", mint_quote.id);
		}
		Ok(())
	}
}
