use crate::logging::Logger;
use crate::{ChainSource, InitFailure, PaymentType, Seed, TxStatus, WalletConfig};
use std::fmt::Debug;

use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;

use ldk_node::bitcoin::{Address, Network};
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::util::persist::KVStore;
use ldk_node::lightning::{log_debug, log_error};
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use ldk_node::payment::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};
use ldk_node::{Event, NodeError};

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

struct LightningWalletImpl {
	ldk_node: ldk_node::Node,
	payment_receipt_flag: watch::Receiver<()>,
}

pub(crate) struct LightningWallet {
	inner: Arc<LightningWalletImpl>,
}

impl LightningWallet {
	pub(super) fn init<E>(
		runtime: Arc<Runtime>, config: WalletConfig<E>, store: Arc<dyn KVStore + Sync + Send>,
		logger: Arc<Logger>,
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
			_ => unreachable!("Unknown network"),
		}
		builder.set_liquidity_source_lsps2(config.lsp.1, config.lsp.0, config.lsp.2);
		match config.chain_source {
			ChainSource::Esplora(url) => builder.set_chain_source_esplora(url, None),
			ChainSource::Electrum(url) => builder.set_chain_source_electrum(url, None),
			ChainSource::BitcoindRPC { host, port, user, password } => {
				builder.set_chain_source_bitcoind_rpc(host, port, user, password)
			},
		};

		builder.set_custom_logger(Arc::clone(&logger) as Arc<dyn ldk_node::logger::LogWriter>);

		let ldk_node = builder.build_with_store(store)?;
		let (payment_receipt_sender, payment_receipt_flag) = watch::channel(());
		let inner = Arc::new(LightningWalletImpl { ldk_node, payment_receipt_flag });

		inner.ldk_node.start_with_runtime(Arc::clone(&runtime))?;

		let events_ref = Arc::clone(&inner);
		runtime.spawn(async move {
			loop {
				let event = events_ref.ldk_node.next_event_async().await;
				log_debug!(logger, "Got ldk-node event {:?}", event);
				match event {
					Event::PaymentSuccessful { .. } => {},
					Event::PaymentFailed { .. } => {},
					Event::PaymentReceived { .. } => {
						let _ = payment_receipt_sender.send(());
					},
					Event::PaymentForwarded { .. } => {},
					Event::PaymentClaimable { .. } => {},
					Event::ChannelPending { .. } => {},
					Event::ChannelReady { .. } => {},
					Event::ChannelClosed { .. } => {
						// TODO: Oof! Probably open a new channel with our LSP
					},
				}
				if let Err(e) = events_ref.ldk_node.event_handled() {
					log_error!(logger, "Failed to handle event: {e:?}");
				}
			}
		});

		Ok(Self { inner })
	}

	pub(crate) async fn await_payment_receipt(&self) {
		let mut flag = self.inner.payment_receipt_flag.clone();
		flag.mark_unchanged();
		let _ = flag.changed().await;
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

	pub(crate) fn stop(&self) {
		let _ = self.inner.ldk_node.stop();
	}
}
