//! A implementation of `TrustedWalletInterface` using the Spark SDK.
use crate::logging::Logger;
use crate::trusted_wallet::{Error, Payment, TrustedPaymentId, TrustedWalletInterface};
use crate::{InitFailure, Seed, TxStatus, WalletConfig};

use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::hashes::sha256::Hash as Sha256;
use ldk_node::lightning::log_debug;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning_invoice::Bolt11Invoice;

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use spark_wallet::{
	DefaultSigner, PayLightningInvoiceResult, SparkWallet, SparkWalletConfig, TransferStatus,
};

use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

/// A wallet implementation using the Breez Spark SDK.
#[derive(Clone)]
pub struct Spark {
	spark_wallet: Arc<SparkWallet<DefaultSigner>>,
	logger: Arc<Logger>,
}

impl TrustedWalletInterface for Spark {
	type ExtraConfig = SparkWalletConfig;

	fn init(
		config: &WalletConfig<SparkWalletConfig>, logger: Arc<Logger>,
	) -> impl Future<Output = Result<Self, InitFailure>> + Send {
		async move {
			if config.network != config.extra_config.network.into() {
				Err(Error::InvalidNetwork)?
			}

			let signer = match &config.seed {
				Seed::Seed64(bytes) => {
					// hash the seed to make sure it does not conflict with the lightning keys
					let seed = Sha256::hash(bytes);
					DefaultSigner::new(&seed[..], config.extra_config.network)
						.expect("todo real error")
				},
				Seed::Mnemonic { mnemonic, passphrase } => {
					// We don't hash the seed here, as mnemonics are meant to be easily recoverable
					// and if we hashed them, then you could not recover your spark coins from the mnemonic
					// in separate wallets.
					let seed = mnemonic.to_seed(passphrase.as_deref().unwrap_or(""));
					DefaultSigner::new(&seed[..], config.extra_config.network)
						.expect("todo real error")
				},
			};

			let spark_wallet =
				Arc::new(SparkWallet::connect(config.extra_config.clone(), signer).await?);

			Ok(Spark { spark_wallet, logger })
		}
	}

	fn get_balance(&self) -> impl Future<Output = Result<Amount, Error>> + Send {
		async move {
			let bal = self.spark_wallet.get_balance().await.unwrap();
			Ok(Amount::from_sats(bal).expect("get_balance failed"))
		}
	}

	fn get_reusable_receive_uri(&self) -> impl Future<Output = Result<String, Error>> + Send {
		async move { todo!() }
	}

	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> impl Future<Output = Result<Bolt11Invoice, Error>> + Send {
		async move {
			// TODO: get upstream to let us be amount-less
			let res = self
				.spark_wallet
				.create_lightning_invoice(amount.unwrap_or(Amount::ZERO).sats_rounding_up(), None)
				.await?;

			Bolt11Invoice::from_str(&res.invoice)
				.map_err(|e| Error::Generic(format!("Failed to parse invoice: {e}")))
		}
	}

	fn list_payments(&self) -> impl Future<Output = Result<Vec<Payment>, Error>> + Send {
		async move {
			let our_pk = self.spark_wallet.get_spark_address().await?;
			let transfers = self.spark_wallet.list_transfers(None).await?;
			let mut res = Vec::with_capacity(transfers.len());
			for transfer in transfers {
				res.push(Payment {
					status: transfer.status.into(),
					id: TrustedPaymentId(uuid::Uuid::from_bytes(transfer.id.to_bytes())),
					amount: Amount::from_sats(transfer.total_value_sat).expect("invalid amount"),
					outbound: transfer.sender_id == our_pk.identity_public_key,
					fee: Amount::ZERO, // Currently everything is free
				});
			}
			Ok(res)
		}
	}

	fn estimate_fee(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<Amount, Error>> + Send {
		async move {
			if let PaymentMethod::LightningBolt11(invoice) = method {
				self.spark_wallet
					.fetch_lightning_send_fee_estimate(
						&invoice.to_string(),
						Some(amount.milli_sats()),
					)
					.await
					.map(|fees| Amount::from_sats(fees).expect("invalid amount"))
			} else {
				Err(Error::Generic("Only BOLT 11 is currently supported".to_owned()))
			}
		}
	}

	fn pay(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<TrustedPaymentId, Error>> + Send {
		async move {
			if let PaymentMethod::LightningBolt11(invoice) = method {
				let res = self
					.spark_wallet
					.pay_lightning_invoice(
						&invoice.to_string(),
						Some(amount.milli_sats()),
						None,
						true, // prefer spark to make things cheaper
					)
					.await?;

				match res {
					PayLightningInvoiceResult::LightningPayment(pay) => Ok(TrustedPaymentId(
						uuid::Uuid::from_str(pay.id.as_str()).expect("invalid id"),
					)),
					PayLightningInvoiceResult::Transfer(transfer) => Ok(TrustedPaymentId(
						uuid::Uuid::from_str(transfer.id.to_string().as_str()).expect("invalid id"),
					)),
				}
			} else {
				Err(Error::Generic("Only BOLT 11 is currently supported".to_owned()))
			}
		}
	}

	fn sync(&self) -> impl Future<Output = ()> + Send {
		async move {
			log_debug!(&self.logger, "Spark syncing...");
			let _ = self.spark_wallet.sync().await;
		}
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
