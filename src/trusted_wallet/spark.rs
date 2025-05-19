//! A implementation of `TrustedWalletInterface` using the Spark SDK.
use crate::logging::Logger;
use crate::trusted_wallet::{Error, Payment, TrustedPaymentId, TrustedWalletInterface};
use crate::{InitFailure, WalletConfig};

use ldk_node::bitcoin::Network;
use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::hashes::sha256::Hash as Sha256;
use ldk_node::lightning_invoice::Bolt11Invoice;

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use spark_rust::signer::default_signer::DefaultSigner;
use spark_rust::signer::traits::SparkSigner;
use spark_rust::{SparkNetwork, SparkSdk};

use std::future::Future;
use std::sync::Arc;

/// A wallet implementation using the Spark SDK.
pub struct SparkWallet {
	spark_wallet: Arc<SparkSdk>,
	logger: Arc<Logger>,
}

impl SparkWallet {
	pub(crate) async fn sync(&self) {
		let _ = self.spark_wallet.sync_wallet().await;
	}
}

impl TrustedWalletInterface for SparkWallet {
	type ExtraConfig = ();

	fn init(
		config: &WalletConfig<()>, logger: Arc<Logger>,
	) -> impl Future<Output = Result<Self, InitFailure>> + Send {
		async move {
			let seed = Sha256::hash(&config.seed);
			let net = match config.network {
				Network::Bitcoin => SparkNetwork::Mainnet,
				Network::Regtest => SparkNetwork::Regtest,
				_ => Err(Error::General("Unsupported network".to_owned()))?,
			};
			let signer = DefaultSigner::from_master_seed(&seed[..], net).await?;
			let spark_wallet = Arc::new(SparkSdk::new(net, signer).await?);

			/*let spark_ref = Arc::clone(&spark_wallet);
			tokio::spawn(async move {
				loop {
					let _ = spark_ref.sync_wallet().await;
					tokio::time::sleep(Duration::from_secs(30)).await;
				}
			});*/

			Ok(SparkWallet { spark_wallet, logger })
		}
	}

	fn get_balance(&self) -> Amount {
		Amount::from_sats(self.spark_wallet.get_bitcoin_balance()).expect("get_balance failed")
	}

	fn get_reusable_receive_uri(&self) -> impl Future<Output = Result<String, Error>> + Send {
		async move { todo!() }
	}

	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> impl Future<Output = Result<Bolt11Invoice, Error>> + Send {
		async move {
			// TODO: get upstream to let us be amount-less
			self.spark_wallet
				.create_lightning_invoice(
					amount.unwrap_or(Amount::ZERO).sats_rounding_up(),
					None,
					None,
				)
				.await
		}
	}

	fn list_payments(&self) -> impl Future<Output = Result<Vec<Payment>, Error>> + Send {
		async move {
			let our_pk = self.spark_wallet.get_spark_address()?;
			let transfers = self.spark_wallet.get_all_transfers(None, None).await?.transfers;
			let mut res = Vec::with_capacity(transfers.len());
			for transfer in transfers {
				res.push(Payment {
					status: transfer.status.into(),
					id: TrustedPaymentId(transfer.id),
					amount: Amount::from_sats(transfer.total_value_sats).expect("invalid amount"),
					outbound: transfer.sender_identity_public_key == our_pk,
					fee: Amount::ZERO, // Currently everything is free
				});
			}
			Ok(res)
		}
	}

	fn estimate_fee(
		&self, method: &PaymentMethod, _amount: Amount,
	) -> impl Future<Output = Result<Amount, Error>> + Send {
		async move {
			if let PaymentMethod::LightningBolt11(invoice) = method {
				// todo doesn't handle amountless invoices
				self.spark_wallet
					.get_lightning_send_fee_estimate(invoice.to_string())
					.await
					.map(|fees| Amount::from_sats(fees.fees).expect("invalid amount"))
			} else {
				Err(Error::General("Only BOLT 11 is currently supported".to_owned()))
			}
		}
	}

	fn pay(
		&self,
		method: &PaymentMethod,
		_amount: Amount, // todo account for amountless invoices
	) -> impl Future<Output = Result<TrustedPaymentId, Error>> + Send {
		async move {
			if let PaymentMethod::LightningBolt11(invoice) = method {
				Ok(TrustedPaymentId(
					self.spark_wallet.pay_lightning_invoice(&invoice.to_string()).await?,
				))
			} else {
				Err(Error::General("Only BOLT 11 is currently supported".to_owned()))
			}
		}
	}

	fn sync(&self) -> impl Future<Output = ()> + Send {
		async move {
			let _ = self.spark_wallet.sync_wallet().await;
		}
	}
}
