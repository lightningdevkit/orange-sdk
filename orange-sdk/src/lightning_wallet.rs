use crate::bitcoin::OutPoint;
use crate::event::{EventQueue, LdkEventHandler};
use crate::logging::Logger;
use crate::store::TxStatus;
use crate::{ChainSource, InitFailure, PaymentType, Seed, WalletConfig};

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use ldk_node::bitcoin::base64::Engine;
use ldk_node::bitcoin::base64::prelude::BASE64_STANDARD;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::{Address, Network, Script};
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::{log_debug, log_error};
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use ldk_node::payment::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};
use ldk_node::{NodeError, UserChannelId, lightning};

use graduated_rebalancer::{LightningBalance, ReceivedLightningPayment};

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;

use tokio::runtime::Runtime;
use tokio::sync::watch;

impl From<PaymentStatus> for TxStatus {
	fn from(o: PaymentStatus) -> TxStatus {
		match o {
			PaymentStatus::Pending => TxStatus::Pending,
			PaymentStatus::Succeeded => TxStatus::Completed,
			PaymentStatus::Failed => TxStatus::Failed,
		}
	}
}

impl From<&PaymentDetails> for PaymentType {
	fn from(d: &PaymentDetails) -> PaymentType {
		match (&d.kind, d.direction == PaymentDirection::Outbound) {
			(
				PaymentKind::Bolt11 { preimage, .. } | PaymentKind::Bolt11Jit { preimage, .. },
				true,
			) => {
				if d.status == PaymentStatus::Succeeded {
					debug_assert!(preimage.is_some());
				}
				PaymentType::OutgoingLightningBolt11 { payment_preimage: *preimage }
			},
			(PaymentKind::Bolt12Offer { preimage, .. }, true) => {
				PaymentType::OutgoingLightningBolt12 { payment_preimage: *preimage }
			},
			(
				PaymentKind::Bolt12Refund { preimage, .. }
				| PaymentKind::Spontaneous { preimage, .. },
				true,
			) => {
				debug_assert!(false);
				PaymentType::OutgoingLightningBolt12 { payment_preimage: *preimage }
			},
			(PaymentKind::Onchain { txid, .. }, true) => {
				PaymentType::OutgoingOnChain { txid: Some(*txid) }
			},
			(PaymentKind::Onchain { txid, .. }, false) => {
				PaymentType::IncomingOnChain { txid: Some(*txid) }
			},
			(_, false) => PaymentType::IncomingLightning {},
		}
	}
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LightningWalletBalance {
	pub(crate) lightning: Amount,
	pub(crate) onchain: Amount,
}

pub(crate) struct LightningWalletImpl {
	pub(crate) ldk_node: Arc<ldk_node::Node>,
	payment_receipt_flag: watch::Receiver<()>,
	channel_pending_receipt_flag: watch::Receiver<u128>,
	lsp_node_id: PublicKey,
	lsp_socket_addr: SocketAddress,
}

pub(crate) struct LightningWallet {
	pub(crate) inner: Arc<LightningWalletImpl>,
}

impl LightningWallet {
	pub(super) async fn init<E>(
		runtime: Arc<Runtime>, config: WalletConfig<E>, store: Arc<dyn KVStore + Sync + Send>,
		event_queue: Arc<EventQueue>, logger: Arc<Logger>,
	) -> Result<Self, InitFailure> {
		let anchor_channels_config = ldk_node::config::AnchorChannelsConfig {
			trusted_peers_no_reserve: vec![config.lsp.1],
			..Default::default()
		};
		let ldk_node_config = ldk_node::config::Config {
			anchor_channels_config: Some(anchor_channels_config),
			..Default::default()
		};
		let mut builder = ldk_node::Builder::from_config(ldk_node_config);
		builder.set_network(config.network);
		match config.seed {
			Seed::Seed64(seed) => {
				builder.set_entropy_seed_bytes(seed);
			},
			Seed::Mnemonic { mnemonic, passphrase } => {
				builder.set_entropy_bip39_mnemonic(mnemonic, passphrase);
			},
		}
		match config.network {
			Network::Bitcoin => {
				builder.set_gossip_source_rgs(
					"https://rapidsync.lightningdevkit.org/snapshot".to_string(),
				);
			},
			Network::Testnet => {
				builder.set_gossip_source_rgs(
					"https://rapidsync.lightningdevkit.org/testnet/snapshot".to_string(),
				);
			},
			Network::Testnet4 => {},
			Network::Signet => {},
			Network::Regtest => {
				// We don't want to run an RGS server in tests so just enable p2p gossip
				builder.set_gossip_source_p2p();
			},
		}
		let (lsp_socket_addr, lsp_node_id, lsp_token) = config.lsp;
		builder.set_liquidity_source_lsps2(lsp_node_id, lsp_socket_addr.clone(), lsp_token);
		match config.chain_source {
			ChainSource::Esplora { url, username, password } => match (&username, &password) {
				(Some(username), Some(password)) => {
					let mut headers = HashMap::with_capacity(1);
					headers.insert(
						"Authorization".to_string(),
						format!(
							"Basic {}",
							BASE64_STANDARD.encode(format!("{}:{}", username, password))
						),
					);
					builder.set_chain_source_esplora_with_headers(url, headers, None)
				},
				(None, None) => builder.set_chain_source_esplora(url, None),
				_ => {
					return Err(InitFailure::LdkNodeStartFailure(NodeError::WalletOperationFailed));
				},
			},
			ChainSource::Electrum(url) => builder.set_chain_source_electrum(url, None),
			ChainSource::BitcoindRPC { host, port, user, password } => {
				builder.set_chain_source_bitcoind_rpc(host, port, user, password)
			},
		};

		builder.set_custom_logger(Arc::clone(&logger) as Arc<dyn ldk_node::logger::LogWriter>);

		// download scorer and write to storage
		// todo switch to https://github.com/lightningdevkit/ldk-node/pull/449 once available
		if let Some(url) = config.scorer_url {
			let fetch = tokio::time::timeout(std::time::Duration::from_secs(10), reqwest::get(url));
			let res = fetch.await.map_err(|e| {
				log_error!(logger, "Timed out downloading scorer: {e}");
				InitFailure::LdkNodeStartFailure(NodeError::InvalidUri)
			})?;

			let req = res.map_err(|e| {
				log_error!(logger, "Failed to download scorer: {e}");
				InitFailure::LdkNodeStartFailure(NodeError::InvalidUri)
			})?;

			let bytes = req.bytes().await.map_err(|e| {
				log_debug!(logger, "Failed to read scorer bytes: {e}");
				InitFailure::LdkNodeStartFailure(NodeError::InvalidUri)
			})?;

			store.write(
				lightning::util::persist::SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
				lightning::util::persist::SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
				lightning::util::persist::SCORER_PERSISTENCE_KEY,
				bytes.as_ref(),
			)?;
		}

		let ldk_node = Arc::new(builder.build_with_store(Arc::clone(&store))?);
		let (payment_receipt_sender, payment_receipt_flag) = watch::channel(());
		let (channel_pending_sender, channel_pending_receipt_flag) = watch::channel(0);
		let ev_handler = Arc::new(LdkEventHandler {
			event_queue,
			ldk_node: Arc::clone(&ldk_node),
			payment_receipt_sender,
			channel_pending_sender,
			logger,
		});
		let inner = Arc::new(LightningWalletImpl {
			ldk_node,
			payment_receipt_flag,
			channel_pending_receipt_flag,
			lsp_node_id,
			lsp_socket_addr,
		});

		inner.ldk_node.start_with_runtime(Arc::clone(&runtime))?;

		runtime.spawn(async move {
			loop {
				let event = ev_handler.ldk_node.next_event_async().await;
				log_debug!(ev_handler.logger, "Got ldk-node event {:?}", event);
				ev_handler.handle_ldk_node_event(event);
			}
		});

		Ok(Self { inner })
	}

	pub(crate) async fn await_payment_receipt(&self) {
		let mut flag = self.inner.payment_receipt_flag.clone();
		flag.mark_unchanged();
		let _ = flag.changed().await;
	}

	pub(crate) async fn await_channel_pending(&self, channel_id: u128) {
		let mut flag = self.inner.channel_pending_receipt_flag.clone();
		flag.mark_unchanged();
		flag.wait_for(|t| t == &channel_id).await.expect("channel pending not received");
	}

	pub(crate) fn get_on_chain_address(&self) -> Result<Address, NodeError> {
		self.inner.ldk_node.onchain_payment().new_address()
	}

	pub(crate) async fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> Result<Bolt11Invoice, NodeError> {
		let desc = Bolt11InvoiceDescription::Direct(Description::empty());
		if let Some(amt) = amount {
			if self.estimate_receivable_balance() >= amt {
				self.inner.ldk_node.bolt11_payment().receive(amt.milli_sats(), &desc, 86400)
			} else {
				self.inner.ldk_node.bolt11_payment().receive_via_jit_channel(
					amt.milli_sats(),
					&desc,
					86400,
					None,
				)
			}
		} else if self.estimate_receivable_balance()
			>= Amount::from_sats(100_000).expect("valid amount")
		{
			self.inner.ldk_node.bolt11_payment().receive_variable_amount(&desc, 86400)
		} else {
			self.inner
				.ldk_node
				.bolt11_payment()
				.receive_variable_amount_via_jit_channel(&desc, 86400, None)
		}
	}

	pub(crate) fn list_payments(&self) -> Vec<PaymentDetails> {
		self.inner.ldk_node.list_payments()
	}

	pub(crate) fn get_balance(&self) -> LightningWalletBalance {
		let balances = self.inner.ldk_node.list_balances();
		LightningWalletBalance {
			lightning: Amount::from_sats(balances.total_lightning_balance_sats)
				.expect("invalid amount"),
			onchain: Amount::from_sats(balances.total_onchain_balance_sats)
				.expect("invalid amount"),
		}
	}

	pub(crate) fn estimate_receivable_balance(&self) -> Amount {
		// Estimate the amount we can receive. Note that this is pretty rough and generally an
		// overestimate.
		let amt =
			self.inner.ldk_node.list_channels().iter().map(|chan| chan.inbound_capacity_msat).max();
		Amount::from_milli_sats(amt.unwrap_or(0)).expect("invalid amount")
	}

	pub(crate) async fn estimate_fee(
		&self, _method: &PaymentMethod, _amount: Amount,
	) -> Result<Amount, NodeError> {
		// TODO: Implement this in ldk-node!
		Ok(Amount::ZERO)
	}

	pub(crate) async fn pay(
		&self, method: &PaymentMethod, amount: Amount,
	) -> Result<PaymentId, NodeError> {
		match method {
			PaymentMethod::LightningBolt11(inv) => self
				.inner
				.ldk_node
				.bolt11_payment()
				.send_using_amount(inv, amount.milli_sats(), None),
			PaymentMethod::LightningBolt12(offer) => self
				.inner
				.ldk_node
				.bolt12_payment()
				.send_using_amount(offer, amount.milli_sats(), None, None),
			PaymentMethod::OnChain(address) => self
				.inner
				.ldk_node
				.onchain_payment()
				.send_to_address(address, amount.sats_rounding_up(), None)
				.map(|txid| PaymentId(*txid.as_ref())),
		}
	}

	pub(crate) async fn open_channel_with_lsp(&self) -> Result<UserChannelId, NodeError> {
		let bal = self.inner.ldk_node.list_balances().spendable_onchain_balance_sats;

		// need a dummy p2wsh address to estimate the fee, p2wsh is used for LN channels
		let fake_addr = Address::p2wsh(Script::new(), self.inner.ldk_node.config().network);

		let fee = self
			.inner
			.ldk_node
			.onchain_payment()
			.estimate_send_all_to_address(&fake_addr, true, None)?;

		let id = self.inner.ldk_node.open_channel(
			self.inner.lsp_node_id,
			self.inner.lsp_socket_addr.clone(),
			bal - fee.to_sat(),
			None,
			None,
		)?;

		Ok(id)
	}

	pub(crate) fn close_channels(&self) -> Result<(), NodeError> {
		let channels = self.inner.ldk_node.list_channels();
		for chan in channels {
			if chan.is_usable {
				self.inner
					.ldk_node
					.close_channel(&chan.user_channel_id, chan.counterparty_node_id)?;
			} else {
				self.inner.ldk_node.force_close_channel(
					&chan.user_channel_id,
					chan.counterparty_node_id,
					None,
				)?;
			}
		}
		Ok(())
	}

	/// Check if the wallet is currently connected to the LSP.
	pub(crate) fn is_connected_to_lsp(&self) -> bool {
		self.inner
			.ldk_node
			.list_peers()
			.into_iter()
			.find(|p| p.node_id == self.inner.lsp_node_id)
			.is_some_and(|p| p.is_connected)
	}

	pub(crate) fn stop(&self) {
		let _ = self.inner.ldk_node.stop();
	}
}

impl graduated_rebalancer::LightningWallet for LightningWallet {
	type Error = NodeError;

	fn get_balance(&self) -> LightningBalance {
		let bal = self.get_balance();
		LightningBalance { lightning: bal.lightning, onchain: bal.onchain }
	}

	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> impl Future<Output = Result<Bolt11Invoice, Self::Error>> + Send {
		async move { self.get_bolt11_invoice(amount).await }
	}

	fn pay(
		&self, method: &PaymentMethod, amount: Amount,
	) -> impl Future<Output = Result<[u8; 32], Self::Error>> + Send {
		async move { self.pay(method, amount).await.map(|p| p.0) }
	}

	fn await_payment_receipt(
		&self, payment_hash: [u8; 32],
	) -> impl Future<Output = Option<ReceivedLightningPayment>> + Send {
		async move {
			let id = PaymentId(payment_hash);
			loop {
				if let Some(payment) = self.inner.ldk_node.payment(&id) {
					let counterparty_skimmed_fee_msat = match payment.kind {
						PaymentKind::Bolt11 { hash, .. } => {
							debug_assert!(hash.0 == payment_hash, "Payment Hash mismatch");
							None
						},
						PaymentKind::Bolt11Jit { hash, counterparty_skimmed_fee_msat, .. } => {
							debug_assert!(hash.0 == payment_hash, "Payment Hash mismatch");
							counterparty_skimmed_fee_msat
						},
						_ => return None, // Ignore other payment kinds, we only care about the one we just sent.
					};
					match payment.status {
						PaymentStatus::Succeeded => {
							return Some(ReceivedLightningPayment {
								id: payment.id.0,
								fee_paid_msat: counterparty_skimmed_fee_msat,
							});
						},
						PaymentStatus::Pending => {},
						PaymentStatus::Failed => return None,
					}
				}
				self.await_payment_receipt().await;
			}
		}
	}

	fn open_channel_with_lsp(
		&self, _amt: Amount,
	) -> impl Future<Output = Result<u128, Self::Error>> + Send {
		async move {
			// we don't use the amount and just use our full spendable balance in open_channel_with_lsp
			self.open_channel_with_lsp().await.map(|c| c.0)
		}
	}

	fn await_channel_pending(&self, channel_id: u128) -> impl Future<Output = OutPoint> + Send {
		async move {
			loop {
				let channels = self.inner.ldk_node.list_channels();
				let chan = channels
					.into_iter()
					.find(|c| c.user_channel_id.0 == channel_id && c.funding_txo.is_some());
				match chan {
					Some(c) => return c.funding_txo.expect("channel has no funding txo"),
					None => {
						self.await_channel_pending(channel_id).await;
						// Wait for the next channel pending event
					},
				}
			}
		}
	}
}
