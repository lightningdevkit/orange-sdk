//! A dummy implementation of `TrustedWalletInterface` for testing purposes.

use crate::EventQueue;
use crate::bitcoin::hashes::Hash;
use crate::runtime::Runtime;
use crate::store::{PaymentId, TxMetadataStore, TxStatus};
use crate::trusted_wallet::{Payment, TrustedError, TrustedWalletInterface};
use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;
use corepc_node::client::bitcoin::Network;
use corepc_node::{Node as Bitcoind, get_available_port};
use graduated_rebalancer::ReceivedLightningPayment;
use ldk_node::lightning::ln::channelmanager;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use ldk_node::payment::{PaymentKind, PaymentStatus};
use ldk_node::{Event, Node};
use rand::RngCore;
use std::env::temp_dir;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{RwLock, watch};
use uuid::Uuid;

/// A dummy implementation of `TrustedWalletInterface` for testing purposes.
/// This wallet uses a local LDK node to handle payments and simulates a custodial wallet
/// by keeping track of the balance and payments in memory.
#[derive(Clone)]
pub(crate) struct DummyTrustedWallet {
	current_bal_msats: Arc<AtomicU64>,
	payments: Arc<RwLock<Vec<Payment>>>,
	ldk_node: Arc<Node>,
	payment_success_flag: watch::Receiver<()>,
}

#[derive(Clone)]
/// Extra configuration for the `DummyTrustedWallet`.
pub struct DummyTrustedWalletExtraConfig {
	/// The test uuid
	pub uuid: Uuid,
	/// The LSP node to connect to
	pub lsp: Arc<Node>,
	/// The Bitcoind node to connect to
	pub bitcoind: Arc<Bitcoind>,
}

impl DummyTrustedWallet {
	/// Creates a new `DummyTrustedWallet` instance.
	pub(crate) async fn new(
		uuid: Uuid, lsp: &Node, bitcoind: &Bitcoind, tx_metadata: TxMetadataStore,
		event_queue: Arc<EventQueue>, rt: Arc<Runtime>,
	) -> Self {
		let mut builder = ldk_node::Builder::new();
		builder.set_network(Network::Regtest);
		let mut seed: [u8; 64] = [0; 64];
		rand::thread_rng().fill_bytes(&mut seed);
		builder.set_entropy_seed_bytes(seed);
		builder.set_gossip_source_p2p();

		let cookie = bitcoind.params.get_cookie_values().unwrap().unwrap();
		builder.set_chain_source_bitcoind_rpc(
			"127.0.0.1".to_string(),
			bitcoind.params.rpc_socket.port(),
			cookie.user,
			cookie.password,
		);

		let tmp = temp_dir().join(format!("orange-test-{uuid}/dummy-ldk"));
		builder.set_storage_dir_path(tmp.to_str().unwrap().to_string());

		let port = get_available_port().unwrap();
		let socket_addr = SocketAddress::TcpIpV4 { addr: [127, 0, 0, 1], port };
		builder.set_listening_addresses(vec![socket_addr.clone()]).unwrap();

		let ldk_node = Arc::new(builder.build().unwrap());

		ldk_node.start().unwrap();

		let current_bal_msats = Arc::new(AtomicU64::new(0));
		let payments: Arc<RwLock<Vec<Payment>>> = Arc::new(RwLock::new(vec![]));

		let (payment_success_sender, payment_success_flag) = watch::channel(());

		let events_ref = Arc::clone(&ldk_node);
		let bal = Arc::clone(&current_bal_msats);
		let pays = Arc::clone(&payments);
		rt.spawn_cancellable_background_task(async move {
			loop {
				let event = events_ref.next_event_async().await;
				match event {
					Event::PaymentSuccessful {
						payment_id,
						fee_paid_msat,
						payment_hash,
						payment_preimage,
					} => {
						// convert id
						let id = mangle_payment_id(payment_id.unwrap().0);

						let mut payments = pays.write().await;
						let item = payments.iter_mut().find(|p| p.id == id);
						if let Some(payment) = item {
							payment.status = TxStatus::Completed;
							if let Some(fee) = fee_paid_msat {
								payment.fee = Amount::from_milli_sats(fee).expect("valid fee")
							}
						}

						let payment_id = PaymentId::Trusted(id);
						let is_rebalance = {
							let map = tx_metadata.read();
							map.get(&payment_id).is_some_and(|m| m.ty.is_rebalance())
						};

						// Send a PaymentSuccessful event if not a rebalance
						if !is_rebalance {
							if tx_metadata
								.set_preimage(payment_id, payment_preimage.unwrap().0)
								.is_err()
							{
								println!("Failed to set preimage for payment {payment_id:?}");
							}
							event_queue
								.add_event(crate::Event::PaymentSuccessful {
									payment_id,
									payment_hash,
									payment_preimage: payment_preimage.unwrap(), // safe
									fee_paid_msat,
								})
								.await
								.unwrap();
						}

						payment_success_sender.send(()).unwrap();
					},
					Event::PaymentFailed { payment_id, payment_hash, reason } => {
						// convert id
						let id = mangle_payment_id(payment_id.unwrap().0);

						let mut payments = pays.write().await;
						let item = payments.iter().cloned().enumerate().find(|(_, p)| p.id == id);
						if let Some((idx, payment)) = item {
							// remove from list and refund balance
							payments.remove(idx);
							bal.fetch_add(payment.amount.milli_sats(), Ordering::SeqCst);
						}

						let payment_id = PaymentId::Trusted(id);
						let is_rebalance = {
							let map = tx_metadata.read();
							map.get(&payment_id).is_some_and(|m| m.ty.is_rebalance())
						};

						// Send a PaymentFailed event if not a rebalance
						if !is_rebalance {
							event_queue
								.add_event(crate::Event::PaymentFailed {
									payment_id,
									payment_hash,
									reason,
								})
								.await
								.unwrap();
						}
					},
					Event::PaymentReceived { payment_id, amount_msat, payment_hash, .. } => {
						// convert id
						let id = mangle_payment_id(payment_id.unwrap().0);

						let mut payments = pays.write().await;
						// We create invoices on the fly without adding the payment to our list
						// We need to insert it into our payments list

						let now = std::time::SystemTime::now()
							.duration_since(std::time::UNIX_EPOCH)
							.unwrap_or_default()
							.as_secs();

						let new = Payment {
							id,
							amount: Amount::from_milli_sats(amount_msat).unwrap(),
							fee: Amount::ZERO,
							status: TxStatus::Completed,
							outbound: false,
							time_since_epoch: Duration::from_secs(now),
						};
						payments.push(new);
						bal.fetch_add(amount_msat, Ordering::SeqCst);

						// Send a PaymentReceived event
						event_queue
							.add_event(crate::Event::PaymentReceived {
								payment_id: PaymentId::Trusted(id),
								payment_hash,
								amount_msat,
								custom_records: vec![],
								lsp_fee_msats: None,
							})
							.await
							.unwrap();
					},
					Event::PaymentForwarded { .. } => {},
					Event::PaymentClaimable { .. } => {},
					Event::ChannelPending { .. } => {},
					Event::ChannelReady { .. } => {},
					Event::ChannelClosed { .. } => {},
					Event::SplicePending { .. } => {},
					Event::SpliceFailed { .. } => {},
				}
				println!("dummy: {event:?}");
				if let Err(e) = events_ref.event_handled() {
					eprintln!("Error handling dummy ldk event: {e}");
				}
			}
		});

		// wait for ldk to be ready
		let iterations = if std::env::var("CI").is_ok() { 120 } else { 10 };
		for _ in 0..iterations {
			if ldk_node.status().is_running {
				break;
			}
			tokio::time::sleep(Duration::from_secs(1)).await;
		}

		// have LSP open channel to node
		lsp.open_channel(ldk_node.node_id(), socket_addr, 1_000_000, Some(500_000_000), None)
			.unwrap();
		// wait for channel to be broadcast
		for _ in 0..iterations {
			let num_txs = bitcoind.client.get_mempool_info().unwrap().size;
			if num_txs > 0 {
				break;
			}
			tokio::time::sleep(Duration::from_millis(250)).await;
		}
		// confirm channel
		let addr = bitcoind.client.new_address().unwrap();
		bitcoind.client.generate_to_address(6, &addr).unwrap();

		// wait for sync/channel ready
		for _ in 0..iterations {
			if ldk_node.list_channels().first().is_some_and(|c| c.is_usable) {
				break;
			}
			tokio::time::sleep(Duration::from_secs(1)).await;
		}

		let channels = ldk_node.list_channels();
		if !ldk_node.list_channels().first().is_some_and(|c| c.is_usable) {
			panic!("No usable channels found {channels:?}");
		}

		DummyTrustedWallet { current_bal_msats, payments, ldk_node, payment_success_flag }
	}

	pub(crate) async fn await_payment_success(&self) {
		let mut flag = self.payment_success_flag.clone();
		flag.mark_unchanged();
		let _ = flag.changed().await;
	}
}

impl TrustedWalletInterface for DummyTrustedWallet {
	fn get_balance(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Amount, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let msats = self.current_bal_msats.load(Ordering::SeqCst);
			Ok(Amount::from_milli_sats(msats).expect("valid msats"))
		})
	}

	fn get_reusable_receive_uri(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<String, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			match self.ldk_node.bolt12_payment().receive_variable_amount("dummy offer", None) {
				Ok(offer) => Ok(offer.to_string()),
				Err(e) => Err(TrustedError::WalletOperationFailed(e.to_string())),
			}
		})
	}

	fn get_bolt11_invoice(
		&self, amount: Option<Amount>,
	) -> Pin<Box<dyn Future<Output = Result<Bolt11Invoice, TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let desc = Bolt11InvoiceDescription::Direct(Description::empty());
			match amount {
				Some(amt) => {
					let invoice = self
						.ldk_node
						.bolt11_payment()
						.receive(amt.milli_sats(), &desc, 300)
						.unwrap();
					Ok(invoice)
				},
				None => {
					let invoice =
						self.ldk_node.bolt11_payment().receive_variable_amount(&desc, 300).unwrap();
					Ok(invoice)
				},
			}
		})
	}

	fn list_payments(
		&self,
	) -> Pin<Box<dyn Future<Output = Result<Vec<Payment>, TrustedError>> + Send + '_>> {
		Box::pin(async move { Ok(self.payments.read().await.clone()) })
	}

	fn estimate_fee(
		&self, _method: PaymentMethod, _amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<Amount, TrustedError>> + Send + '_>> {
		Box::pin(async move { Ok(Amount::ZERO) })
	}

	fn pay(
		&self, method: PaymentMethod, amount: Amount,
	) -> Pin<Box<dyn Future<Output = Result<[u8; 32], TrustedError>> + Send + '_>> {
		Box::pin(async move {
			let id = match method {
				PaymentMethod::LightningBolt11(inv) => {
					let id = self
						.ldk_node
						.bolt11_payment()
						.send_using_amount(&inv, amount.milli_sats(), None)
						.unwrap()
						.0;

					mangle_payment_id(id)
				},
				PaymentMethod::LightningBolt12(offer) => {
					let id = self
						.ldk_node
						.bolt12_payment()
						.send_using_amount(&offer, amount.milli_sats(), None, None, None)
						.unwrap()
						.0;

					mangle_payment_id(id)
				},
				PaymentMethod::OnChain(address) => {
					let txid = self
						.ldk_node
						.onchain_payment()
						.send_to_address(&address, amount.sats_rounding_up(), None)
						.unwrap();
					txid.to_byte_array()
				},
			};

			// subtract from our balance
			self.current_bal_msats.fetch_sub(amount.milli_sats(), Ordering::SeqCst);

			let now = std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap_or_default()
				.as_secs();

			// add to payments
			let mut list = self.payments.write().await;
			list.push(Payment {
				id,
				amount,
				fee: Amount::ZERO,
				status: TxStatus::Pending,
				outbound: true,
				time_since_epoch: Duration::from_secs(now),
			});

			Ok(id)
		})
	}

	fn await_payment_success(
		&self, payment_hash: [u8; 32],
	) -> Pin<Box<dyn Future<Output = Option<ReceivedLightningPayment>> + Send + '_>> {
		Box::pin(async move {
			let id = channelmanager::PaymentId(payment_hash);
			loop {
				if let Some(payment) = self.ldk_node.payment(&id) {
					let counterparty_skimmed_fee_msat = match payment.kind {
						PaymentKind::Bolt11 { hash, .. } => {
							debug_assert!(hash.0 == payment_hash, "Payment Hash mismatch");
							None
						},
						PaymentKind::Bolt11Jit { hash, counterparty_skimmed_fee_msat, .. } => {
							debug_assert!(hash.0 == payment_hash, "Payment Hash mismatch");
							counterparty_skimmed_fee_msat
						},
						_ => return None, /* Ignore other payment kinds, we only care about the one we just sent. */
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
				self.await_payment_success().await;
			}
		})
	}

	fn stop(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		Box::pin(async move {
			let _ = self.ldk_node.stop();
		})
	}
}

// we don't want our payment ids to be the same as LDK's, this ended up not testing
// bad assumptions properly. So we mangle the payment id a bit here to avoid collisions.
fn mangle_payment_id(id: [u8; 32]) -> [u8; 32] {
	let mut mangled = id;
	for i in &mut mangled {
		*i = i.wrapping_add(0x42);
	}
	mangled
}
