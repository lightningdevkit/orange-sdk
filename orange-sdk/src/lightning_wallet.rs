use crate::bitcoin::OutPoint;
use crate::bitcoin::hashes::Hash;
use crate::dyn_store::{DynStore, LdkNodeStore};
use crate::event::{EventQueue, LdkEventHandler};
use crate::logging::Logger;
use crate::runtime::Runtime;
use crate::store::{TxMetadataStore, TxStatus};
use crate::{ChainSource, InitFailure, PaymentType, WalletConfig, store};

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use ldk_node::bitcoin::base64::Engine;
use ldk_node::bitcoin::base64::prelude::BASE64_STANDARD;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::{Address, Network};
use ldk_node::config::{AsyncPaymentsRole, BackgroundSyncConfig, SyncTimeoutsConfig};
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::{log_debug, log_error, log_info};
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use ldk_node::payment::{
	ConfirmationStatus, PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus,
};
use ldk_node::{NodeError, UserChannelId};

use graduated_rebalancer::{LightningBalance, ReceivedLightningPayment};

use std::collections::{HashMap, VecDeque};
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::sync::{Notify, watch};

#[derive(Debug, Clone, Copy)]
pub(crate) struct LightningWalletBalance {
	pub(crate) lightning: Amount,
	pub(crate) onchain: Amount,
}

pub(crate) struct LightningWalletImpl {
	pub(crate) ldk_node: Arc<ldk_node::Node>,
	logger: Arc<Logger>,
	store: Arc<dyn DynStore>,
	payment_receipt_flag: watch::Receiver<()>,
	channel_pending_receipt_flag: watch::Receiver<u128>,
	splice_pending_inbox: Arc<SplicePendingInbox>,
	lsp_node_id: PublicKey,
	lsp_socket_addr: SocketAddress,
}

pub(crate) struct LightningWallet {
	pub(crate) inner: Arc<LightningWalletImpl>,
}

const DEFAULT_INVOICE_EXPIRY_SECS: u32 = 86_400; // 24 hours

impl LightningWallet {
	pub(super) async fn init(
		runtime: Arc<Runtime>, config: WalletConfig, store: Arc<dyn DynStore>,
		event_queue: Arc<EventQueue>, tx_metadata: TxMetadataStore, logger: Arc<Logger>,
	) -> Result<Self, InitFailure> {
		log_info!(logger, "Creating LDK node...");
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
		let node_entropy = config.seed.to_node_entropy();

		match config.rgs_url {
			Some(url) => {
				builder.set_gossip_source_rgs(url);
			},
			None => {
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
			},
		}

		let (lsp_socket_addr, lsp_node_id, lsp_token) = config.lsp;
		builder.add_liquidity_source(lsp_node_id, lsp_socket_addr.clone(), lsp_token, true);
		match config.chain_source {
			ChainSource::Esplora { url, username, password } => {
				let sync_config = if config.network == Network::Regtest {
					ldk_node::config::EsploraSyncConfig {
						background_sync_config: Some(BackgroundSyncConfig {
							onchain_wallet_sync_interval_secs: 2,
							lightning_wallet_sync_interval_secs: 2,
							fee_rate_cache_update_interval_secs: 30,
						}),
						timeouts_config: SyncTimeoutsConfig::default(),
					}
				} else {
					ldk_node::config::EsploraSyncConfig::default()
				};

				match (&username, &password) {
					(Some(username), Some(password)) => {
						let mut headers = HashMap::with_capacity(1);
						headers.insert(
							"Authorization".to_string(),
							format!(
								"Basic {}",
								BASE64_STANDARD.encode(format!("{username}:{password}"))
							),
						);
						builder.set_chain_source_esplora_with_headers(
							url,
							headers,
							Some(sync_config),
						)
					},
					(None, None) => builder.set_chain_source_esplora(url, Some(sync_config)),
					_ => {
						return Err(InitFailure::LdkNodeStartFailure(
							NodeError::WalletOperationFailed,
						));
					},
				}
			},
			ChainSource::Electrum(url) => {
				let sync_config = if config.network == Network::Regtest {
					Some(ldk_node::config::ElectrumSyncConfig {
						timeouts_config: SyncTimeoutsConfig::default(),
						background_sync_config: Some(BackgroundSyncConfig {
							onchain_wallet_sync_interval_secs: 2,
							lightning_wallet_sync_interval_secs: 2,
							fee_rate_cache_update_interval_secs: 30,
						}),
					})
				} else {
					None
				};
				builder.set_chain_source_electrum(url, sync_config)
			},
			ChainSource::BitcoindRPC { host, port, user, password } => {
				builder.set_chain_source_bitcoind_rpc(host, port, user, password)
			},
		};

		builder.set_async_payments_role(Some(AsyncPaymentsRole::Client))?;

		builder.set_custom_logger(Arc::clone(&logger) as Arc<dyn ldk_node::logger::LogWriter>);

		builder.set_runtime(runtime.get_handle());

		if let Some(url) = config.scorer_url {
			builder.set_pathfinding_scores_source(url);
		}

		let ldk_node =
			Arc::new(builder.build_with_store(node_entropy, LdkNodeStore(Arc::clone(&store)))?);
		let (payment_receipt_sender, payment_receipt_flag) = watch::channel(());
		let (channel_pending_sender, channel_pending_receipt_flag) = watch::channel(0);
		let splice_pending_inbox = Arc::new(SplicePendingInbox {
			pending: Mutex::new(HashMap::new()),
			notify: Notify::new(),
		});
		let ev_handler = Arc::new(LdkEventHandler {
			event_queue,
			ldk_node: Arc::clone(&ldk_node),
			tx_metadata,
			payment_receipt_sender,
			channel_pending_sender,
			splice_pending_inbox: Arc::clone(&splice_pending_inbox),
			logger: Arc::clone(&logger),
		});
		let inner = Arc::new(LightningWalletImpl {
			ldk_node,
			logger,
			store,
			payment_receipt_flag,
			channel_pending_receipt_flag,
			splice_pending_inbox,
			lsp_node_id,
			lsp_socket_addr,
		});

		inner.ldk_node.start()?;

		runtime.spawn_cancellable_background_task(async move {
			loop {
				let event = ev_handler.ldk_node.next_event_async().await;
				log_debug!(ev_handler.logger, "Got ldk-node event {event:?}");
				ev_handler.handle_ldk_node_event(event).await;
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

	pub(crate) async fn await_splice_pending(&self, channel_id: u128) -> OutPoint {
		self.inner.splice_pending_inbox.wait_for(channel_id).await
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
				self.inner.ldk_node.bolt11_payment().receive(
					amt.milli_sats(),
					&desc,
					DEFAULT_INVOICE_EXPIRY_SECS,
				)
			} else {
				self.inner.ldk_node.bolt11_payment().receive_via_jit_channel(
					amt.milli_sats(),
					&desc,
					DEFAULT_INVOICE_EXPIRY_SECS,
					None,
				)
			}
		} else if self.estimate_receivable_balance() // if we can receive at least 100k sats, don't use JIT
			>= Amount::from_sats(100_000).expect("valid amount")
		{
			self.inner
				.ldk_node
				.bolt11_payment()
				.receive_variable_amount(&desc, DEFAULT_INVOICE_EXPIRY_SECS)
		} else {
			self.inner.ldk_node.bolt11_payment().receive_variable_amount_via_jit_channel(
				&desc,
				DEFAULT_INVOICE_EXPIRY_SECS,
				None,
			)
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
				.send_using_amount(offer, amount.milli_sats(), None, None, None),
			PaymentMethod::OnChain(address) => {
				let amount_sats = amount.sats().map_err(|_| NodeError::InvalidAmount)?;

				let balance = self.inner.ldk_node.list_balances();

				// if we have enough onchain balance, send onchain
				if balance.spendable_onchain_balance_sats > amount_sats {
					self.inner
						.ldk_node
						.onchain_payment()
						.send_to_address(address, amount_sats, None)
						.map(|txid| PaymentId(*txid.as_ref()))
				} else {
					// otherwise try to pay via splice out

					// find existing channel to splice out of
					let channels = self.inner.ldk_node.list_channels();
					let channel =
						channels.iter().find(|c| c.counterparty.node_id == self.inner.lsp_node_id);

					match channel {
						None => {
							log_error!(self.inner.logger, "No existing channel to splice out of");
							Err(NodeError::InsufficientFunds)
						},
						Some(chan) => {
							self.inner.ldk_node.splice_out(
								&chan.user_channel_id,
								chan.counterparty.node_id,
								address,
								amount_sats,
							)?;

							let funding_txo =
								self.await_splice_pending(chan.user_channel_id.0).await;

							let id = PaymentId(funding_txo.txid.to_byte_array());
							let details = PaymentDetails {
								id,
								kind: PaymentKind::Onchain {
									txid: funding_txo.txid,
									status: ConfirmationStatus::Unconfirmed, // todo how do we update this?
								},
								amount_msat: Some(amount_sats * 1_000),
								fee_paid_msat: Some(69), // todo get real fee
								direction: PaymentDirection::Outbound,
								status: PaymentStatus::Succeeded,
								latest_update_timestamp: SystemTime::now()
									.duration_since(SystemTime::UNIX_EPOCH)
									.unwrap()
									.as_secs(),
							};

							store::write_splice_out(self.inner.store.as_ref(), &details).await;
							Ok(id)
						},
					}
				}
			},
		}
	}

	pub(crate) async fn splice_all_into_channel(&self) -> Result<UserChannelId, NodeError> {
		// find existing channel to splice into
		let channels = self.inner.ldk_node.list_channels();
		let channel = channels.iter().find(|c| c.counterparty.node_id == self.inner.lsp_node_id);

		match channel {
			Some(chan) => {
				self.inner
					.ldk_node
					.splice_in_with_all(&chan.user_channel_id, chan.counterparty.node_id)?;
				Ok(chan.user_channel_id)
			},
			None => {
				log_error!(self.inner.logger, "No existing channel to splice into");
				Err(NodeError::WalletOperationFailed)
			},
		}
	}

	pub(crate) async fn open_channel_with_lsp(&self) -> Result<UserChannelId, NodeError> {
		let id = self.inner.ldk_node.open_channel_with_all(
			self.inner.lsp_node_id,
			self.inner.lsp_socket_addr.clone(),
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
					.close_channel(&chan.user_channel_id, chan.counterparty.node_id)?;
			} else {
				self.inner.ldk_node.force_close_channel(
					&chan.user_channel_id,
					chan.counterparty.node_id,
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
	) -> Pin<Box<dyn Future<Output = Result<Bolt11Invoice, Self::Error>> + Send + '_>> {
		Box::pin(async move { self.get_bolt11_invoice(amount).await })
	}

	fn pay(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<[u8; 32], Self::Error>> + Send + '_>> {
		Box::pin(async move { self.pay(&method, amount).await.map(|p| p.0) })
	}

	fn await_payment_receipt(
		&self, payment_hash: [u8; 32],
	) -> Pin<Box<dyn Future<Output = Option<ReceivedLightningPayment>> + Send + '_>> {
		Box::pin(async move {
			let id = PaymentId(payment_hash);
			loop {
				if let Some(payment) = self.inner.ldk_node.payment(&id) {
					let counterparty_skimmed_fee_msat = match payment.kind {
						PaymentKind::Bolt11 { hash, counterparty_skimmed_fee_msat, .. } => {
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
		})
	}

	fn has_channel_with_lsp(&self) -> bool {
		let channels = self.inner.ldk_node.list_channels();
		channels.iter().any(|c| c.counterparty.node_id == self.inner.lsp_node_id)
	}

	fn open_channel_with_lsp(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<u128, Self::Error>> + Send + '_>> {
		Box::pin(async move { self.open_channel_with_lsp().await.map(|c| c.0) })
	}

	fn await_channel_pending(
		&self, channel_id: u128,
	) -> Pin<Box<dyn Future<Output = OutPoint> + Send + '_>> {
		Box::pin(async move {
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
		})
	}

	fn splice_to_lsp_channel(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<u128, Self::Error>> + Send + '_>> {
		Box::pin(async move { self.splice_all_into_channel().await.map(|c| c.0) })
	}

	fn await_splice_pending(
		&self, channel_id: u128,
	) -> Pin<Box<dyn Future<Output = OutPoint> + Send + '_>> {
		// `ChannelDetails.funding_txo` from `list_channels` still reports the old funding
		// outpoint between `SplicePending` and `SpliceLocked`, so we return the new outpoint
		// from the event itself rather than reading it back from the channel.
		Box::pin(async move { self.await_splice_pending(channel_id).await })
	}
}

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
			(PaymentKind::Bolt11 { preimage, .. }, true) => {
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

/// Pending `SplicePending` events keyed by `user_channel_id`, consumed by
/// `await_splice_pending`. A per-channel queue rather than a `watch` so each consumer takes its own
/// event: `watch` would let a second splice on the same channel observe the previous splice's stale
/// outpoint instead of waiting for the new `SplicePending`.
pub(crate) struct SplicePendingInbox {
	pub(crate) pending: Mutex<HashMap<u128, VecDeque<OutPoint>>>,
	pub(crate) notify: Notify,
}

impl SplicePendingInbox {
	pub(crate) fn deliver(&self, channel_id: u128, funding_txo: OutPoint) {
		self.pending.lock().unwrap().entry(channel_id).or_default().push_back(funding_txo);
		self.notify.notify_waiters();
	}

	pub(crate) async fn wait_for(&self, channel_id: u128) -> OutPoint {
		loop {
			// Register interest BEFORE checking the queue so a `notify_waiters` racing with the
			// check still wakes us up.
			let notified = self.notify.notified();
			tokio::pin!(notified);
			notified.as_mut().enable();
			if let Some(txo) = {
				let mut pending = self.pending.lock().unwrap();
				if let Some(queue) = pending.get_mut(&channel_id) {
					let txo = queue.pop_front();
					if queue.is_empty() {
						pending.remove(&channel_id);
					}
					txo
				} else {
					None
				}
			} {
				return txo;
			}
			notified.await;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use ldk_node::bitcoin::Txid;

	fn dummy_outpoint(seed: u8) -> OutPoint {
		let bytes = [seed; 32];
		OutPoint { txid: Txid::from_byte_array(bytes), vout: seed as u32 }
	}

	#[test]
	fn splice_pending_inbox_preserves_distinct_events_for_same_channel() {
		let inbox =
			SplicePendingInbox { pending: Mutex::new(HashMap::new()), notify: Notify::new() };
		let channel_id = 7;
		let first = dummy_outpoint(0xaa);
		let second = dummy_outpoint(0xbb);

		inbox.deliver(channel_id, first);
		inbox.deliver(channel_id, second);

		let mut pending = inbox.pending.lock().unwrap();
		let queue = pending.get_mut(&channel_id).expect("channel should have pending events");

		assert_eq!(queue.pop_front(), Some(first));
		assert_eq!(queue.pop_front(), Some(second));
		assert!(queue.is_empty());
	}
}
