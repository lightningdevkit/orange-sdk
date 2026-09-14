//! An implementation of `TrustedWalletInterface` using the Cashu (CDK) SDK.

use super::payment_store::PaymentStore;
use crate::logging::Logger;
use crate::runtime::Runtime;
use crate::store::{PaymentId, TxMetadataStore, TxStatus};
use crate::trusted_wallet::{Payment, TrustedError, TrustedWalletInterface};
use crate::{Event, EventQueue, InitFailure, Seed, WalletConfig};

use crate::dyn_store::DynStore;
use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::hashes::sha256::Hash as Sha256;
use ldk_node::bitcoin::hex::FromHex;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::{log_debug, log_error, log_info, log_warn};
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::lightning_types::payment::{PaymentHash, PaymentPreimage};

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use cdk::amount::SplitTarget;
use cdk::nuts::MeltOptions;
use cdk::nuts::nut00::PaymentMethod as CdkPaymentMethod;
use cdk::nuts::nut23::Amountless;
use cdk::nuts::{CurrencyUnit, MeltQuoteState};
use cdk::types::FinalizedMelt;
use cdk::wallet::Wallet;
use cdk::wallet::types::{Transaction, TransactionDirection};
use cdk::wallet::{MeltQuote, MintQuote};
use cdk::{Amount as CdkAmount, StreamExt};

use graduated_rebalancer::ReceivedLightningPayment;

use tokio::sync::{Notify, RwLock, mpsc, watch};

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Cashu KV store implementation
pub mod cashu_store;

use cashu_store::{CashuKvDatabase, read_has_recovered, write_has_recovered};

/// Configuration for the Cashu wallet
#[derive(Debug, Clone)]
pub struct CashuConfig {
	/// The mint URL to connect to
	pub mint_url: String,
	/// The currency unit to use (typically Sat)
	pub unit: CurrencyUnit,
	/// Optional npub.cash URL for lightning address support (e.g., `https://npubx.cash`)
	pub npubcash_url: Option<String>,
}

/// A wallet implementation using the Cashu (CDK) SDK.
#[derive(Clone)]
pub struct Cashu {
	melt: Arc<MeltContext>,
	cashu_wallet: Arc<Wallet>,
	unit: CurrencyUnit,
	shutdown_sender: watch::Sender<()>,
	logger: Arc<Logger>,
	supports_bolt12: Arc<std::sync::atomic::AtomicBool>,
	supports_mpp: Arc<std::sync::atomic::AtomicBool>,
	mint_quote_sender: mpsc::Sender<MintQuote>,
	runtime: Arc<Runtime>,
	npubcash_url: Option<String>,
	npub: Option<String>,
}

impl TrustedWalletInterface for Cashu {
	fn get_balance(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Amount, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let balance = self.cashu_wallet.total_balance().await.map_err(|e| {
				TrustedError::WalletOperationFailed(format!("Failed to get balance: {e}"))
			})?;

			convert_amount(balance, &self.unit)
		})
	}

	fn get_reusable_receive_uri(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<String, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			if !self.supports_bolt12.load(std::sync::atomic::Ordering::Relaxed) {
				return Err(TrustedError::UnsupportedOperation(
					"Cashu mint does not support BOLT 12".to_owned(),
				));
			}

			let mint_quote = self
				.cashu_wallet
				.mint_quote(CdkPaymentMethod::BOLT12, None, None, None)
				.await
				.map_err(|e| {
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
					let cdk_amount = match self.unit {
						CurrencyUnit::Sat => {
							CdkAmount::from(a.sats().map_err(|()| TrustedError::AmountError)?)
						},
						CurrencyUnit::Msat => CdkAmount::from(a.milli_sats()),
						_ => {
							return Err(TrustedError::Other(format!(
								"Unsupported currency unit {:?} for Cashu wallet",
								self.unit
							)));
						},
					};
					let quote = self
						.cashu_wallet
						.mint_quote(CdkPaymentMethod::BOLT11, Some(cdk_amount), None, None)
						.await
						.map_err(|e| {
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
				.map(|t| Self::convert_transaction_to_payment(t, &self.unit))
				.collect::<Result<Vec<_>, _>>()?;

			Ok(self.melt.payments.merge(payments).await)
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
						.melt_quote(
							CdkPaymentMethod::BOLT11,
							invoice.to_string(),
							melt_options,
							None,
						)
						.await
						.map_err(|e| {
							TrustedError::WalletOperationFailed(format!(
								"Failed to get melt quote: {e}"
							))
						})?;

					// The fee is in the quote
					let quote_fee = convert_amount(quote.fee_reserve, &self.unit)?;
					let input_fee =
						self.estimate_input_fee(quote.amount + quote.fee_reserve).await?;
					Ok(quote_fee.saturating_add(input_fee))
				},
				PaymentMethod::LightningBolt12(offer) => {
					let quote = self
						.cashu_wallet
						.melt_quote(CdkPaymentMethod::BOLT12, offer.to_string(), melt_options, None)
						.await
						.map_err(|e| {
							TrustedError::WalletOperationFailed(format!(
								"Failed to get melt quote: {e}"
							))
						})?;

					// The fee is in the quote
					let quote_fee = convert_amount(quote.fee_reserve, &self.unit)?;
					let input_fee =
						self.estimate_input_fee(quote.amount + quote.fee_reserve).await?;
					Ok(quote_fee.saturating_add(input_fee))
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

			let mut payment_hash: Option<PaymentHash> = None;

			let quote = match method {
				PaymentMethod::LightningBolt11(invoice) => {
					payment_hash = Some(invoice.payment_hash());

					// if we have an active quote for this invoice, use it
					// otherwise create a new one
					// this is to avoid creating multiple quotes for the same invoice and can cause database errors
					// this typically happens when we estimate the fee first and then pay
					let quotes = self.cashu_wallet.get_active_melt_quotes().await.map_err(|e| {
						TrustedError::WalletOperationFailed(format!(
							"Failed to get active melt quotes: {e}"
						))
					})?;
					let active_quote =
						quotes.into_iter().find(|q| q.request == invoice.to_string());

					match active_quote {
						Some(q) => q,
						None => self
							.cashu_wallet
							.melt_quote(
								CdkPaymentMethod::BOLT11,
								invoice.to_string(),
								melt_options,
								None,
							)
							.await
							.map_err(|e| {
								TrustedError::WalletOperationFailed(format!(
									"Failed to create melt quote: {e}"
								))
							})?,
					}
				},
				PaymentMethod::LightningBolt12(offer) => {
					if !self.supports_bolt12.load(std::sync::atomic::Ordering::Relaxed) {
						return Err(TrustedError::UnsupportedOperation(
							"Cashu mint does not support BOLT 12".to_owned(),
						));
					}

					// todo probably should check for existing active quote here as well

					self.cashu_wallet
						.melt_quote(CdkPaymentMethod::BOLT12, offer.to_string(), melt_options, None)
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

			// Convert quote ID to a 32-byte array for consistency
			// We'll use the quote ID as the payment identifier
			let payment_id = Self::id_to_32_byte_array(&quote.id);

			self.start_melt(&quote, payment_id, amount, payment_hash).await?;

			Ok(payment_id)
		})
	}

	fn supports_partial_payments(&self) -> bool {
		// Partial MPP payments require the mint to advertise NUT-15 support for BOLT 11 in our unit.
		self.supports_mpp.load(std::sync::atomic::Ordering::Relaxed)
	}

	fn pay_partial(
		&self, invoice: Bolt11Invoice, partial_amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<[u8; 32], TrustedError>> + Send + '_>> {
		Box::pin(async move {
			if !self.supports_mpp.load(std::sync::atomic::Ordering::Relaxed) {
				return Err(TrustedError::UnsupportedOperation(
					"Cashu mint does not support partial (MPP) payments".to_owned(),
				));
			}

			// An MPP melt declares the partial amount this mint should pay toward the invoice; the
			// remainder is paid out of the lightning wallet over the same payment hash.
			let melt_options = Some(MeltOptions::new_mpp(partial_amount.milli_sats()));
			let payment_hash = Some(invoice.payment_hash());

			let quote = self
				.cashu_wallet
				.melt_quote(CdkPaymentMethod::BOLT11, invoice.to_string(), melt_options, None)
				.await
				.map_err(|e| {
					TrustedError::WalletOperationFailed(format!(
						"Failed to create MPP melt quote: {e}"
					))
				})?;

			let payment_id = Self::id_to_32_byte_array(&quote.id);
			self.start_melt(&quote, payment_id, partial_amount, payment_hash).await?;
			Ok(payment_id)
		})
	}

	fn get_lightning_address(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Option<String>, TrustedError>> + Send + '_>> {
		Box::pin(async {
			match (&self.npubcash_url, &self.npub) {
				(Some(url), Some(npub)) => {
					let domain = url.trim_start_matches("https://").trim_start_matches("http://");
					Ok(Some(format!("{npub}@{domain}")))
				},
				_ => Ok(None),
			}
		})
	}

	fn register_lightning_address(
		&self, _name: String,
	) -> Pin<Box<dyn Future<Output = Result<(), TrustedError>> + Send + '_>> {
		Box::pin(async {
			if self.npubcash_url.is_none() {
				return Err(TrustedError::UnsupportedOperation(
					"npubcash_url is not configured".to_string(),
				));
			}
			// npub.cash addresses are deterministic from the Nostr keys,
			// and set_mint_url is called during init. Nothing to do here.
			Ok(())
		})
	}

	fn stop(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		Box::pin(async move {
			log_info!(self.logger, "Stopping Cashu wallet");
			let _ = self.shutdown_sender.send(());
		})
	}
}

const PAYMENT_HASH_METADATA_KEY: &str = "payment_hash";

impl Cashu {
	pub(crate) async fn init(
		config: &WalletConfig, cashu_config: CashuConfig, store: Arc<dyn DynStore>,
		event_queue: Arc<EventQueue>, tx_metadata: TxMetadataStore, logger: Arc<Logger>,
		runtime: Arc<Runtime>,
	) -> Result<Self, InitFailure> {
		match &cashu_config.unit {
			CurrencyUnit::Sat | CurrencyUnit::Msat => {},
			unit => {
				return Err(InitFailure::TrustedFailure(TrustedError::Other(format!(
					"Unsupported currency unit {unit} for Cashu wallet"
				))));
			},
		}

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

		let payments = Arc::new(PaymentStore::new(Arc::clone(&store), Arc::clone(&logger)).await?);
		let db = Arc::new(
			CashuKvDatabase::new(Arc::clone(&store), Arc::clone(&runtime)).await.map_err(|e| {
				InitFailure::TrustedFailure(TrustedError::Other(format!(
					"Failed to create Cashu database: {e}"
				)))
			})?,
		);

		// Create the Cashu wallet
		let cashu_wallet = Arc::new(
			Wallet::new(&cashu_config.mint_url, cashu_config.unit.clone(), db, seed, None)
				.map_err(|e| {
					InitFailure::TrustedFailure(TrustedError::Other(format!(
						"Failed to create Cashu wallet: {e}"
					)))
				})?,
		);

		let supports_bolt12 = Arc::new(std::sync::atomic::AtomicBool::new(false));
		let supports_mpp = Arc::new(std::sync::atomic::AtomicBool::new(false));
		{
			let w = Arc::clone(&cashu_wallet);
			let bolt12_flag = Arc::clone(&supports_bolt12);
			let mpp_flag = Arc::clone(&supports_mpp);
			let unit = cashu_config.unit.clone();
			runtime.spawn_cancellable_background_task(async move {
				if let Some(info) = w.fetch_mint_info().await.ok().flatten() {
					if info.nuts.nut04.supported_methods().contains(&&CdkPaymentMethod::BOLT12) {
						bolt12_flag.store(true, std::sync::atomic::Ordering::Relaxed);
					}
					// NUT-15 advertises which (method, unit) pairs the mint will accept partial MPP
					// melts for.
					let mpp_supported = info
						.nuts
						.nut15
						.methods
						.iter()
						.any(|m| m.method == CdkPaymentMethod::BOLT11 && m.unit == unit);
					if mpp_supported {
						mpp_flag.store(true, std::sync::atomic::Ordering::Relaxed);
					}
				}
			});
		}

		let (shutdown_sender, mut shutdown_receiver) = watch::channel::<()>(());

		let melt = Arc::new(MeltContext {
			payments,
			in_flight: Mutex::new(HashSet::new()),
			gate: RwLock::new(()),
			reconcile: Notify::new(),
			event_queue: Arc::clone(&event_queue),
			tx_metadata: tx_metadata.clone(),
			unit: cashu_config.unit.clone(),
			logger: Arc::clone(&logger),
		});

		// Resolve melts whose outcome was unknown when the process last stopped, or whose
		// request failed in a way that does not prove the mint did not pay. The task wakes up
		// when a melt ends without a definite result and backs off while anything is unresolved.
		let melt_for_reconcile = Arc::clone(&melt);
		let wallet_for_reconcile = Arc::clone(&cashu_wallet);
		let mut shutdown_for_reconcile = shutdown_sender.subscribe();
		runtime.spawn_cancellable_background_task(async move {
			const MIN_BACKOFF: Duration = Duration::from_secs(30);
			const MAX_BACKOFF: Duration = Duration::from_secs(10 * 60);
			let mut backoff = MIN_BACKOFF;
			loop {
				let unresolved = melt_for_reconcile.reconcile(&wallet_for_reconcile).await;
				let notified = melt_for_reconcile.reconcile.notified();
				if unresolved == 0 {
					backoff = MIN_BACKOFF;
					tokio::select! {
						_ = shutdown_for_reconcile.changed() => return,
						_ = notified => {},
					}
				} else {
					tokio::select! {
						_ = shutdown_for_reconcile.changed() => return,
						// A melt just ended without a result; check it promptly.
						_ = notified => backoff = MIN_BACKOFF,
						_ = tokio::time::sleep(backoff) => {
							backoff = (backoff * 2).min(MAX_BACKOFF);
						},
					}
				}
			}
		});

		// Create channel for mint quote monitoring with bounded capacity
		let (mint_quote_sender, mut mint_quote_receiver) = mpsc::channel::<MintQuote>(32);

		// Start mint quote monitoring task
		let wallet_for_monitoring = Arc::clone(&cashu_wallet);
		let logger_for_monitoring = Arc::clone(&logger);
		let eq_for_monitoring = Arc::clone(&event_queue);
		let rt_for_monitoring = Arc::clone(&runtime);
		runtime.spawn_cancellable_background_task(async move {
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
						let event_queue = Arc::clone(&eq_for_monitoring);
						let logger = Arc::clone(&logger_for_monitoring);
						rt_for_monitoring.spawn_cancellable_background_task(async move {
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

		// spawn background task to check all pending mint quotes
		let c = Arc::clone(&cashu_wallet);
		let l = Arc::clone(&logger);
		runtime.spawn_cancellable_background_task(async move {
			if let Err(e) = c.check_all_mint_quotes().await {
				log_error!(l, "Failed to check pending mint quotes: {e}");
			}
		});

		// spawn background task to recover funds if first time initializing
		let has_recovered = read_has_recovered(&store).await?;
		if !has_recovered {
			let w = Arc::clone(&cashu_wallet);
			let l = Arc::clone(&logger);
			runtime.spawn_background_task(async move {
				match w.restore().await {
					Err(e) => log_error!(l, "Failed to restore cashu mint: {e}"),
					Ok(restored) => {
						if restored.unspent > cdk::Amount::ZERO {
							log_info!(l, "Restored cashu mint: {}: {:#?}", w.mint_url, restored);
						}
						if let Err(e) = write_has_recovered(&store, true).await {
							log_error!(l, "Failed to write has_recovered flag: {e:?}");
						}
					},
				}
			});
		}

		// Initialize npub.cash if configured
		let npubcash_url = cashu_config.npubcash_url.clone();
		let mut npub: Option<String> = None;

		if let Some(ref url) = npubcash_url {
			npub = Some(Self::derive_npub(&seed).map_err(|e| {
				InitFailure::TrustedFailure(TrustedError::WalletOperationFailed(format!(
					"Failed to derive npub: {e}"
				)))
			})?);

			// Enable npub.cash and start polling in background to avoid blocking init
			let wallet_for_npubcash = Arc::clone(&cashu_wallet);
			let sender_for_npubcash = mint_quote_sender.clone();
			let logger_for_npubcash = Arc::clone(&logger);
			let mut shutdown_for_npubcash = shutdown_sender.subscribe();
			let url = url.clone();
			runtime.spawn_cancellable_background_task(async move {
				if let Err(e) = wallet_for_npubcash.enable_npubcash(url.clone()).await {
					log_error!(logger_for_npubcash, "Failed to enable npub.cash: {e}");
					return;
				}
				log_info!(logger_for_npubcash, "npub.cash enabled with URL: {url}");

				let poll_interval = Duration::from_secs(30);
				let mut interval = tokio::time::interval(poll_interval);
				loop {
					tokio::select! {
						_ = shutdown_for_npubcash.changed() => {
							log_info!(logger_for_npubcash, "npub.cash polling shutdown");
							return;
						}
						_ = interval.tick() => {
							match wallet_for_npubcash.sync_npubcash_quotes().await {
								Ok(quotes) => {
									for quote in quotes {
										if matches!(quote.state, cdk::nuts::MintQuoteState::Paid) {
											let id = quote.id.clone();
											if let Err(e) = sender_for_npubcash.send(quote).await {
												log_error!(
													logger_for_npubcash,
													"Failed to send npub.cash quote {id} for monitoring: {e}"
												);
											}
										}
									}
								},
								Err(e) => {
									log_error!(
										logger_for_npubcash,
										"Failed to sync npub.cash quotes: {e}"
									);
								},
							}
						}
					}
				}
			});
		}

		Ok(Cashu {
			melt,
			cashu_wallet,
			unit: cashu_config.unit,
			shutdown_sender,
			logger,
			supports_bolt12,
			supports_mpp,
			mint_quote_sender,
			runtime,
			npubcash_url,
			npub,
		})
	}

	/// Derive the npub (bech32-encoded Nostr public key) from the wallet seed.
	///
	/// Uses the same derivation as CDK's `derive_npubcash_keys`: the first 32 bytes
	/// of the seed as a secp256k1 secret key, then bech32-encodes the x-only public key.
	fn derive_npub(seed: &[u8; 64]) -> Result<String, String> {
		use ldk_node::bitcoin::bech32::{Bech32, Hrp, encode};
		use ldk_node::bitcoin::secp256k1::{Secp256k1, SecretKey};

		let sk =
			SecretKey::from_slice(&seed[..32]).map_err(|e| format!("Invalid secret key: {e}"))?;
		let secp = Secp256k1::new();
		let (xonly, _) = sk.public_key(&secp).x_only_public_key();
		let hrp = Hrp::parse("npub").expect("valid hrp");
		encode::<Bech32>(hrp, &xonly.serialize()).map_err(|e| format!("bech32 encode: {e}"))
	}

	/// Persists the attempt, then melts the quote unless it is already in flight.
	///
	/// A quote the mint still lists as unpaid can be melted again after a transient error; the
	/// CDK and the mint reject a quote that is actually being paid. A quote that is pending at the
	/// mint is left to the reconciliation task, which reports its final outcome.
	async fn start_melt(
		&self, quote: &MeltQuote, payment_id: [u8; 32], amount: Amount,
		payment_hash: Option<PaymentHash>,
	) -> Result<(), TrustedError> {
		if quote.state == MeltQuoteState::Unpaid {
			// Register before persisting so reconciliation never sees this payment as abandoned.
			if !self.melt.in_flight.lock().unwrap().insert(payment_id) {
				log_debug!(self.logger, "Melt for quote {} is already in flight", quote.id);
				return Ok(());
			}
			let reference = Some(quote.id.clone());
			if let Err(e) = self.melt.payments.insert_pending(payment_id, amount, reference).await {
				self.melt.in_flight.lock().unwrap().remove(&payment_id);
				return Err(e);
			}
			self.spawn_melt(quote.id.clone(), payment_id, payment_hash);
		} else {
			self.melt.payments.insert_pending(payment_id, amount, Some(quote.id.clone())).await?;
			log_info!(
				self.logger,
				"Quote {} is {}; waiting for its outcome instead of melting again",
				quote.id,
				quote.state
			);
			self.melt.reconcile.notify_one();
		}
		Ok(())
	}

	/// Executes a previously-created melt quote in a background task, emitting a
	/// [`PaymentSuccessful`] or [`PaymentFailed`] event when it completes. The payment is not
	/// awaited; this only kicks off the melt.
	///
	/// [`PaymentSuccessful`]: crate::event::Event::PaymentSuccessful
	/// [`PaymentFailed`]: crate::event::Event::PaymentFailed
	fn spawn_melt(
		&self, quote_id: String, payment_id: [u8; 32], payment_hash: Option<PaymentHash>,
	) {
		let cashu_wallet = Arc::clone(&self.cashu_wallet);
		let melt = Arc::clone(&self.melt);
		self.runtime.spawn_background_task(async move {
			let gate = melt.gate.read().await;
			let mut metadata = HashMap::new();
			if let Some(hash) = &payment_hash {
				metadata.insert(PAYMENT_HASH_METADATA_KEY.to_string(), hash.to_string());
			}

			let mut submitted = false;
			let melt_result = async {
				let prepared = cashu_wallet.prepare_melt(&quote_id, metadata).await?;
				submitted = true;
				prepared.confirm().await
			}
			.await;
			let resolved = melt
				.handle_melt_result(&quote_id, payment_id, payment_hash, melt_result, submitted)
				.await;
			melt.in_flight.lock().unwrap().remove(&payment_id);
			drop(gate);
			if !resolved {
				melt.reconcile.notify_one();
			}
		});
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
	fn convert_transaction_to_payment(
		transaction: Transaction, unit: &CurrencyUnit,
	) -> Result<Payment, TrustedError> {
		// Convert transaction ID to a 32-byte array
		let id = transaction.quote_id.ok_or(TrustedError::WalletOperationFailed(
			"Missing quote ID in transaction".to_owned(),
		))?;
		let payment_id = Self::id_to_32_byte_array(&id);

		// Convert amounts - CDK amounts are u64 representing sats
		let amount = convert_amount(transaction.amount, unit)?;
		let fee = convert_amount(transaction.fee, unit)?;

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

			// Parse the invoice to get the payment hash
			// todo this won't work for bolt12
			let invoice = Bolt11Invoice::from_str(&mint_quote.request).map_err(|e| {
				TrustedError::Other(format!("Failed to parse invoice from mint quote: {e}"))
			})?;
			let hash = invoice.payment_hash();

			// Send a PaymentReceived event
			event_queue
				.add_event(Event::PaymentReceived {
					payment_id: PaymentId::Trusted(payment_id),
					payment_hash: hash,
					amount_msat: u64::from(mint_quote.amount.unwrap_or_default()) * 1_000, /* convert to msats */
					custom_records: vec![],
					lsp_fee_msats: None,
				})
				.await
				.map_err(|e| TrustedError::Other(format!("Failed to add event: {e}")))?;

			log_info!(logger, "Sent PaymentReceived event for mint quote: {}", mint_quote.id);
		}
		Ok(())
	}

	async fn estimate_input_fee(&self, input_amount: CdkAmount) -> Result<Amount, TrustedError> {
		let proofs = self.cashu_wallet.get_unspent_proofs().await.map_err(|e| {
			TrustedError::WalletOperationFailed(format!("Failed to get unspent proofs: {e}"))
		})?;

		let mut counts_by_keyset = HashMap::new();
		for proof in proofs {
			*counts_by_keyset.entry(proof.keyset_id).or_insert(0_u64) += 1;
		}

		let mut fee = Amount::ZERO;
		for (keyset_id, proof_count) in counts_by_keyset {
			let keyset_fee =
				self.cashu_wallet.calculate_fee(proof_count, keyset_id).await.map_err(|e| {
					TrustedError::WalletOperationFailed(format!(
						"Failed to calculate input fee: {e}"
					))
				})?;
			fee = fee.saturating_add(convert_amount(keyset_fee, &self.unit)?);
		}

		let active_keyset = self.cashu_wallet.get_active_keyset().await.map_err(|e| {
			TrustedError::WalletOperationFailed(format!("Failed to get active keyset: {e}"))
		})?;
		let fee_and_amounts =
			self.cashu_wallet.get_keyset_fees_and_amounts_by_id(active_keyset.id).await.map_err(
				|e| {
					TrustedError::WalletOperationFailed(format!(
						"Failed to get keyset fee amounts: {e}"
					))
				},
			)?;
		let output_count = input_amount.split(&fee_and_amounts).map_err(|e| {
			TrustedError::WalletOperationFailed(format!(
				"Failed to calculate melt output count: {e}"
			))
		})?;
		let output_fee = self
			.cashu_wallet
			.calculate_fee(output_count.len() as u64, active_keyset.id)
			.await
			.map_err(|e| {
				TrustedError::WalletOperationFailed(format!("Failed to calculate output fee: {e}"))
			})?;
		fee = fee.saturating_add(convert_amount(output_fee, &self.unit)?);

		Ok(fee)
	}
}

/// State shared between melt tasks and the reconciliation task.
struct MeltContext {
	payments: Arc<PaymentStore>,
	/// Payment IDs with a melt running in this process.
	in_flight: Mutex<HashSet<[u8; 32]>>,
	/// Melt tasks hold this shared; reconciliation holds it exclusively so it never touches a
	/// saga while the melt that owns it is running.
	gate: RwLock<()>,
	/// Wakes the reconciliation task after a melt ended without a definite outcome.
	reconcile: Notify,
	event_queue: Arc<EventQueue>,
	tx_metadata: TxMetadataStore,
	unit: CurrencyUnit,
	logger: Arc<Logger>,
}

impl MeltContext {
	/// Records the result of a melt request. Returns whether the outcome is final.
	async fn handle_melt_result(
		&self, quote_id: &str, payment_id: [u8; 32], payment_hash: Option<PaymentHash>,
		result: Result<FinalizedMelt, cdk::Error>, submitted: bool,
	) -> bool {
		match result {
			Ok(res) => self.handle_melt_outcome(quote_id, payment_id, payment_hash, &res).await,
			Err(e) => {
				log_error!(self.logger, "Failed to melt quote {quote_id}: {e}");
				if matches!(e, cdk::Error::PendingQuote | cdk::Error::PaidQuote)
					|| (submitted && !melt_was_rejected(&e))
				{
					// The mint may have paid despite this error. Keep history pending.
					return false;
				}
				self.melt_failed(payment_id, payment_hash).await;
				true
			},
		}
	}

	/// Records the mint's answer for a melt. Returns whether the outcome is final.
	async fn handle_melt_outcome(
		&self, quote_id: &str, payment_id: [u8; 32], payment_hash: Option<PaymentHash>,
		res: &FinalizedMelt,
	) -> bool {
		match res.state() {
			MeltQuoteState::Paid => {
				log_info!(self.logger, "Successfully sent for quote: {quote_id}");
				self.melt_succeeded(quote_id, payment_id, payment_hash, res).await;
				true
			},
			// The CDK reports a melt it compensated without ever executing as unpaid.
			MeltQuoteState::Failed | MeltQuoteState::Unpaid => {
				log_error!(self.logger, "Melt failed for quote: {quote_id}");
				self.melt_failed(payment_id, payment_hash).await;
				true
			},
			state => {
				log_info!(self.logger, "Melt still {state} for quote: {quote_id}");
				false
			},
		}
	}

	async fn melt_succeeded(
		&self, quote_id: &str, payment_id: [u8; 32], payment_hash: Option<PaymentHash>,
		res: &FinalizedMelt,
	) {
		let first_report = self.payments.mark_completed(payment_id).await.unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to save payment success: {e}");
			true
		});
		let fee_paid_msat =
			convert_amount(res.fee_paid(), &self.unit).ok().map(|fee| fee.milli_sats());
		// Wake registered rebalances even when their public payment event is suppressed or
		// cannot be persisted. The watcher ignores a repeated report itself.
		if let Some(hash) = payment_hash {
			let receipt = ReceivedLightningPayment { id: payment_id, fee_paid_msat };
			self.event_queue.rebalance_watchers.sent(hash.0, Some(receipt));
		}
		if !first_report {
			log_debug!(self.logger, "Success of quote {quote_id} was already reported");
			return;
		}
		let payment_id = PaymentId::Trusted(payment_id);
		let is_rebalance = {
			let map = self.tx_metadata.read();
			map.get(&payment_id).is_some_and(|m| m.ty.is_rebalance())
		};
		if is_rebalance {
			return;
		}

		let preimage: Option<PaymentPreimage> = match res.payment_proof() {
			Some(str) => match FromHex::from_hex(str) {
				Ok(b) => Some(PaymentPreimage(b)),
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to decode preimage ({:?}) for quote {quote_id}: {e}",
						res.payment_proof()
					);
					None
				},
			},
			None => {
				// Expected for same-mint payments: when the melt's bolt11 destination is a
				// mint quote on this same mint, cdk-mintd settles internally. No Lightning
				// payment occurs, so there is no preimage to return. The success path below
				// already tolerates None (hash falls back to the invoice payment_hash).
				log_info!(
					self.logger,
					"Melt for quote {quote_id} settled without a preimage (internal/same-mint settlement)"
				);
				None
			},
		};

		let hash = match payment_hash {
			Some(hash) => hash,
			None => match preimage {
				Some(pre) => {
					let hash = Sha256::hash(&pre.0);
					PaymentHash(hash.to_byte_array())
				},
				None => {
					log_error!(
						self.logger,
						"Melt succeeded but no payment hash or preimage for quote: {quote_id}"
					);
					PaymentHash([0u8; 32]) // Placeholder, should not happen
				},
			},
		};

		let payment_preimage = preimage.unwrap_or(PaymentPreimage([0u8; 32]));

		if self.tx_metadata.set_preimage(payment_id, payment_preimage.0).await.is_err() {
			log_error!(self.logger, "Failed to set preimage for payment {payment_id:?}");
		}

		let _ = self
			.event_queue
			.add_event(Event::PaymentSuccessful {
				payment_id,
				payment_hash: hash,
				payment_preimage,
				fee_paid_msat,
			})
			.await;
	}

	async fn melt_failed(&self, payment_id: [u8; 32], payment_hash: Option<PaymentHash>) {
		let first_report = self.payments.mark_failed(payment_id).await.unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to save payment failure: {e}");
			true
		});
		if let Some(hash) = payment_hash {
			self.event_queue.rebalance_watchers.sent(hash.0, None);
		}
		if !first_report {
			log_debug!(self.logger, "Failure of payment {payment_id:?} was already reported");
			return;
		}
		let payment_id = PaymentId::Trusted(payment_id);
		let is_rebalance = {
			let map = self.tx_metadata.read();
			map.get(&payment_id).is_some_and(|m| m.ty.is_rebalance())
		};
		if !is_rebalance {
			let _ = self
				.event_queue
				.add_event(Event::PaymentFailed { payment_id, payment_hash, reason: None })
				.await;
		}
	}

	/// Resolves melts with an unknown outcome. Returns how many remain unresolved.
	///
	/// Interrupted melts are finalized through the CDK's saga log, which asks the mint and
	/// either recovers the change or releases the reserved proofs. Submitted payments the CDK
	/// has no saga for are checked against the mint directly. Melts running in this process
	/// are left alone so the two paths cannot race on the same quote.
	async fn reconcile(&self, wallet: &Wallet) -> usize {
		let mut unresolved = 0;
		let Ok(_gate) = self.gate.try_write() else {
			return 1;
		};
		match wallet.finalize_pending_melts().await {
			Ok(finalized) => {
				for melt in finalized {
					let quote_id = melt.quote_id().to_owned();
					let payment_id = Cashu::id_to_32_byte_array(&quote_id);
					let payment_hash = quote_payment_hash(wallet, &quote_id).await;
					if !self.handle_melt_outcome(&quote_id, payment_id, payment_hash, &melt).await {
						unresolved += 1;
					}
				}
			},
			Err(e) => {
				log_error!(self.logger, "Failed to finalize pending melts: {e}");
				unresolved += 1;
			},
		}

		let pending = self.payments.pending().await;
		if pending.is_empty() {
			return unresolved;
		}
		// Quotes with an incomplete saga belong to the CDK; the pass above settles those.
		let saga_quotes: HashSet<String> = match wallet.localstore.get_incomplete_sagas().await {
			Ok(sagas) => sagas.into_iter().filter_map(|saga| saga.quote_id).collect(),
			Err(e) => {
				log_error!(self.logger, "Failed to load incomplete sagas: {e}");
				return unresolved + pending.len();
			},
		};
		for (payment_id, reference) in pending {
			if self.in_flight.lock().unwrap().contains(&payment_id) {
				continue;
			}
			let Some(quote_id) = reference else {
				log_warn!(self.logger, "No melt quote for pending payment {payment_id:?}");
				unresolved += 1;
				continue;
			};
			if saga_quotes.contains(&quote_id) {
				unresolved += 1;
				continue;
			}
			let quote = match wallet.localstore.get_melt_quote(&quote_id).await {
				Ok(Some(quote)) => quote,
				Ok(None) => {
					log_warn!(self.logger, "Melt quote {quote_id} is missing");
					unresolved += 1;
					continue;
				},
				Err(e) => {
					log_warn!(self.logger, "Failed to load melt quote {quote_id}: {e}");
					unresolved += 1;
					continue;
				},
			};
			let payment_hash =
				Bolt11Invoice::from_str(&quote.request).ok().map(|i| i.payment_hash());
			let status = match wallet.check_melt_quote_status(&quote.id).await {
				Ok(status) => status,
				Err(e) => {
					log_warn!(self.logger, "Failed to check melt quote {}: {e}", quote.id);
					unresolved += 1;
					continue;
				},
			};
			let outcome = FinalizedMelt::new(
				quote.id.clone(),
				status.state,
				status.payment_preimage.clone(),
				status.amount,
				CdkAmount::ZERO,
				None,
			);
			if !self.handle_melt_outcome(&quote.id, payment_id, payment_hash, &outcome).await {
				unresolved += 1;
			}
		}
		unresolved
	}
}

/// The payment hash of the invoice a melt quote pays, when it is a BOLT 11 invoice.
async fn quote_payment_hash(wallet: &Wallet, quote_id: &str) -> Option<PaymentHash> {
	let quote = wallet.localstore.get_melt_quote(quote_id).await.ok().flatten()?;
	Bolt11Invoice::from_str(&quote.request).ok().map(|i| i.payment_hash())
}

fn convert_amount(cdk_amount: CdkAmount, unit: &CurrencyUnit) -> Result<Amount, TrustedError> {
	match unit {
		CurrencyUnit::Sat => {
			Amount::from_sats(cdk_amount.into()).map_err(|_| TrustedError::AmountError)
		},
		CurrencyUnit::Msat => {
			Amount::from_milli_sats(cdk_amount.into()).map_err(|_| TrustedError::AmountError)
		},
		unit => {
			Err(TrustedError::Other(format!("Unsupported currency unit {unit} for Cashu wallet")))
		},
	}
}

fn melt_was_rejected(error: &cdk::Error) -> bool {
	matches!(
		error,
		cdk::Error::PaymentFailed
			| cdk::Error::InsufficientFunds
			| cdk::Error::InvalidInvoice
			| cdk::Error::ExpiredQuote(_, _)
			| cdk::Error::AmountOutofLimitRange(_, _, _)
			| cdk::Error::MeltingDisabled
			| cdk::Error::UnsupportedUnit
			| cdk::Error::MaxFeeExceeded
	)
}

#[cfg(test)]
mod melt_error_tests {
	use super::*;

	#[test]
	fn ambiguous_melt_errors_are_not_failures() {
		assert!(melt_was_rejected(&cdk::Error::PaymentFailed));
		assert!(melt_was_rejected(&cdk::Error::InsufficientFunds));
		assert!(!melt_was_rejected(&cdk::Error::Timeout));
		assert!(!melt_was_rejected(&cdk::Error::PendingQuote));
		assert!(!melt_was_rejected(&cdk::Error::PaidQuote));
		assert!(!melt_was_rejected(&cdk::Error::Internal));
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::store::{TxMetadata, TxType};
	use crate::test_store::{TestStore, test_event_queue, test_logger, test_runtime};
	use std::sync::atomic::AtomicBool;

	#[tokio::test(flavor = "multi_thread")]
	async fn failed_melt_resolves_rebalance_wait_without_a_public_event() {
		let store = TestStore::default();
		let runtime = test_runtime();
		let tx_metadata = TxMetadataStore::new(store.shared()).await;
		let id = [1; 32];
		tx_metadata
			.insert(
				PaymentId::Trusted(id),
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
		let event_queue = test_event_queue(&store, tx_metadata.clone(), Arc::clone(&runtime)).await;
		let db =
			Arc::new(CashuKvDatabase::new(store.shared(), Arc::clone(&runtime)).await.unwrap());
		let wallet = Cashu {
			melt: Arc::new(MeltContext {
				payments: Arc::new(PaymentStore::new(store.shared(), test_logger()).await.unwrap()),
				in_flight: Mutex::new(HashSet::new()),
				gate: RwLock::new(()),
				reconcile: Notify::new(),
				event_queue: Arc::clone(&event_queue),
				tx_metadata,
				unit: CurrencyUnit::Sat,
				logger: test_logger(),
			}),
			cashu_wallet: Arc::new(
				Wallet::new("http://127.0.0.1:1", CurrencyUnit::Sat, db, [1; 64], None).unwrap(),
			),
			unit: CurrencyUnit::Sat,
			shutdown_sender: watch::channel(()).0,
			logger: test_logger(),

			supports_bolt12: Arc::new(AtomicBool::new(false)),
			supports_mpp: Arc::new(AtomicBool::new(false)),
			mint_quote_sender: mpsc::channel(1).0,
			runtime: Arc::clone(&runtime),
			npubcash_url: None,
			npub: None,
		};
		let result = event_queue.rebalance_watchers.register([2; 32]);
		// A missing quote fails in CDK before any network request.
		wallet.spawn_melt("missing-quote".into(), id, Some(PaymentHash([2; 32])));
		assert!(tokio::time::timeout(Duration::from_secs(2), result).await.unwrap().is_none());
		runtime.wait_on_background_tasks();
		assert!(event_queue.next_event().is_none());
	}
}
