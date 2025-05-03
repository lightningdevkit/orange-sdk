use crate::logging::Logger;
use crate::{InitFailure, TxStatus, WalletConfig};

use ldk_node::bitcoin::Network;
use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::hashes::sha256::Hash as Sha256;
use ldk_node::bitcoin::io;
use ldk_node::lightning::ln::msgs::DecodeError;
use ldk_node::lightning::util::ser::{Readable, Writeable, Writer};
use ldk_node::lightning_invoice::Bolt11Invoice;

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use spark_rust::error::SparkSdkError;
use spark_rust::signer::default_signer::DefaultSigner;
use spark_rust::signer::traits::SparkSigner;
use spark_rust::{SparkNetwork, SparkSdk};

use spark_protos::spark::TransferStatus;

use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TrustedPaymentId(pub(crate) uuid::Uuid);
impl Readable for TrustedPaymentId {
	fn read<R: io::Read>(r: &mut R) -> Result<Self, DecodeError> {
		Ok(TrustedPaymentId(uuid::Uuid::from_bytes(Readable::read(r)?)))
	}
}

impl Writeable for TrustedPaymentId {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), io::Error> {
		self.0.as_bytes().write(w)
	}
}

impl fmt::Display for TrustedPaymentId {
	fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		self.0.fmt(f)
	}
}

impl FromStr for TrustedPaymentId {
	type Err = <uuid::Uuid as FromStr>::Err;
	fn from_str(s: &str) -> Result<Self, <uuid::Uuid as FromStr>::Err> {
		Ok(TrustedPaymentId(uuid::Uuid::from_str(s)?))
	}
}

pub(crate) type Error = SparkSdkError;

#[derive(Debug, Clone)]
pub(crate) struct Payment {
	pub(crate) id: TrustedPaymentId,
	pub(crate) amount: Amount,
	pub(crate) fee: Amount,
	pub(crate) status: TxStatus,
	pub(crate) outbound: bool,
}

impl From<TransferStatus> for TxStatus {
	fn from(o: TransferStatus) -> TxStatus {
		match o {
			TransferStatus::SenderInitiated
			| TransferStatus::SenderKeyTweakPending
			| TransferStatus::SenderKeyTweaked
			| TransferStatus::ReceiverKeyTweaked => TxStatus::Pending,
			TransferStatus::Completed => TxStatus::Completed,
			TransferStatus::Expired
			| TransferStatus::Returned
			| TransferStatus::TransferStatusrReceiverRefundSigned => TxStatus::Failed,
		}
	}
}

pub(crate) trait TrustedWalletInterface: Sized {
	fn init(
		config: &WalletConfig, logger: Arc<Logger>,
	) -> impl Future<Output = Result<Self, InitFailure>> + Send;
	fn get_balance(&self) -> Amount;
	fn get_reusable_receive_uri(&self) -> impl Future<Output = Result<String, Error>> + Send;
	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> impl Future<Output = Result<Bolt11Invoice, Error>> + Send;
	fn list_payments(&self) -> impl Future<Output = Result<Vec<Payment>, Error>> + Send;
	fn estimate_fee(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<Amount, Error>> + Send;
	fn pay(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<TrustedPaymentId, Error>> + Send;
}

pub(crate) struct SparkWallet {
	spark_wallet: Arc<SparkSdk>,
	logger: Arc<Logger>,
}

impl SparkWallet {
	pub(crate) async fn sync(&self) {
		let _ = self.spark_wallet.sync_wallet().await;
	}
}

impl TrustedWalletInterface for SparkWallet {
	fn init(
		config: &WalletConfig, logger: Arc<Logger>,
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
}
