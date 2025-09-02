//! A dummy implementation of `TrustedWalletInterface` for testing purposes.

use crate::EventQueue;
use crate::bitcoin::hashes::Hash;
use crate::store::{PaymentId, TxStatus};
use crate::trusted_wallet::{Payment, TrustedError, TrustedWalletInterface};
use bitcoin_payment_instructions::PaymentMethod;
use bitcoin_payment_instructions::amount::Amount;
use corepc_node::client::bitcoin::Network;
use corepc_node::{Node as Bitcoind, get_available_port};
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use ldk_node::{Event, Node};
use rand::RngCore;
use std::env::temp_dir;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::runtime::Runtime;
use uuid::Uuid;

/// A dummy implementation of `TrustedWalletInterface` for testing purposes.
/// This wallet uses a local LDK node to handle payments and simulates a custodial wallet
/// by keeping track of the balance and payments in memory.
#[derive(Clone)]
pub(crate) struct DummyTrustedWallet {
	current_bal_msats: Arc<AtomicU64>,
	payments: Arc<RwLock<Vec<Payment>>>,
	ldk_node: Arc<Node>,
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
	/// The runtime to use for async tasks
	pub rt: Arc<Runtime>,
}

impl DummyTrustedWallet {
	/// Creates a new `DummyTrustedWallet` instance.
	pub(crate) async fn new(
		uuid: Uuid, lsp: &Node, bitcoind: &Bitcoind, event_queue: Arc<EventQueue>, rt: Arc<Runtime>,
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

		let tmp = temp_dir().join(format!("orange-test-{uuid}-dummy-ldk"));
		builder.set_storage_dir_path(tmp.to_str().unwrap().to_string());

		let port = get_available_port().unwrap();
		let socket_addr = SocketAddress::TcpIpV4 { addr: [127, 0, 0, 1], port };
		builder.set_listening_addresses(vec![socket_addr.clone()]).unwrap();

		let ldk_node = Arc::new(builder.build().unwrap());

		ldk_node.start_with_runtime(Arc::clone(&rt)).unwrap();

		let current_bal_msats = Arc::new(AtomicU64::new(0));
		let payments: Arc<RwLock<Vec<Payment>>> = Arc::new(RwLock::new(vec![]));

		let events_ref = Arc::clone(&ldk_node);
		let bal = Arc::clone(&current_bal_msats);
		let pays = Arc::clone(&payments);
		rt.spawn(async move {
			loop {
				let event = events_ref.next_event_async().await;
				match event {
					Event::PaymentSuccessful { payment_id, fee_paid_msat, .. } => {
						// convert id
						let id = payment_id.unwrap().0;

						let mut payments = pays.write().unwrap();
						let item = payments.iter_mut().find(|p| p.id == id);
						if let Some(payment) = item {
							payment.status = TxStatus::Completed;
							if let Some(fee) = fee_paid_msat {
								payment.fee = Amount::from_milli_sats(fee).expect("valid fee")
							}
						}
					},
					Event::PaymentFailed { payment_id, .. } => {
						// convert id
						let id = payment_id.unwrap().0;

						let mut payments = pays.write().unwrap();
						let item = payments.iter().cloned().enumerate().find(|(_, p)| p.id == id);
						if let Some((idx, payment)) = item {
							payments.remove(idx);
							bal.fetch_sub(payment.amount.milli_sats(), Ordering::SeqCst);
						}
					},
					Event::PaymentReceived { payment_id, amount_msat, payment_hash, .. } => {
						// convert id
						let id = payment_id.unwrap().0;

						let mut payments = pays.write().unwrap();
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
							.unwrap();
					},
					Event::PaymentForwarded { .. } => {},
					Event::PaymentClaimable { .. } => {},
					Event::ChannelPending { .. } => {},
					Event::ChannelReady { .. } => {},
					Event::ChannelClosed { .. } => {},
				}
				println!("dummy: {event:?}");
				if let Err(e) = events_ref.event_handled() {
					eprintln!("Error handling dummy ldk event: {e}");
				}
			}
		});

		// have LSP open channel to node
		lsp.open_channel(ldk_node.node_id(), socket_addr, 1_000_000, Some(500_000_000), None)
			.unwrap();
		// wait for channel to be broadcast
		tokio::time::sleep(Duration::from_secs(1)).await;
		// confirm channel
		let addr = bitcoind.client.new_address().unwrap();
		bitcoind.client.generate_to_address(6, &addr).unwrap();

		// wait for sync/channel ready
		for _ in 0..10 {
			if ldk_node.list_channels().first().is_some_and(|c| c.is_usable) {
				break;
			}
			tokio::time::sleep(Duration::from_secs(1)).await;
		}

		let channels = ldk_node.list_channels();
		if !ldk_node.list_channels().first().is_some_and(|c| c.is_usable) {
			panic!("No usable channels found {channels:?}");
		}

		DummyTrustedWallet { current_bal_msats, payments, ldk_node }
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
		Box::pin(async move { Ok(self.payments.read().unwrap().clone()) })
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
					self.ldk_node
						.bolt11_payment()
						.send_using_amount(&inv, amount.milli_sats(), None)
						.unwrap()
						.0
				},
				PaymentMethod::LightningBolt12(offer) => {
					self.ldk_node
						.bolt12_payment()
						.send_using_amount(&offer, amount.milli_sats(), None, None)
						.unwrap()
						.0
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
			let mut list = self.payments.write().unwrap();
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

	fn stop(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		Box::pin(async move {
			let _ = self.ldk_node.stop();
		})
	}
}
