#![cfg(feature = "_test-utils")]

use bitcoin_payment_instructions::amount::Amount;
use corepc_node::client::bitcoin::Network;
use corepc_node::{Conf, Node as Bitcoind, get_available_port};
use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::liquidity::LSPS2ServiceConfig;
use ldk_node::payment::PaymentStatus;
use ldk_node::{Node, bitcoin};
use orange_sdk::trusted_wallet::dummy::{DummyTrustedWallet, DummyTrustedWalletExtraConfig};
use orange_sdk::{ChainSource, Seed, StorageConfig, Wallet, WalletConfig};
use rand::RngCore;
use std::env::temp_dir;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use uuid::Uuid;

/// Waits for an async condition to be true, polling at a specified interval until a timeout.
pub async fn wait_for_condition<F, Fut>(
	interval: Duration, iterations: usize, condition_name: &str, mut condition: F,
) -> Result<(), String>
where
	F: FnMut() -> Fut,
	Fut: Future<Output = bool>,
{
	for _ in 0..iterations {
		if condition().await {
			return Ok(());
		}
		tokio::time::sleep(interval).await;
	}
	Err(format!("Timeout waiting for condition: {condition_name}"))
}

/// Waits for the next event from the wallet, polling every second for up to 10 seconds.
/// If no event is received within this time, it panics.
/// This is useful for testing purposes to ensure that the wallet is responsive and events are being processed.
/// Otherwise, we can have the test hang indefinitely.
pub async fn wait_next_event(wallet: &Wallet<DummyTrustedWallet>) -> orange_sdk::Event {
	for _ in 0..10 {
		if let Some(event) = wallet.next_event() {
			return event;
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
	panic!("Timeout waiting for condition next event")
}

fn create_bitcoind() -> Bitcoind {
	let mut conf = Conf::default();
	conf.args.push("-txindex");
	conf.args.push("-rpcworkqueue=100");
	Bitcoind::with_conf(corepc_node::downloaded_exe_path().unwrap(), &conf).unwrap()
}

pub fn generate_blocks(bitcoind: &Bitcoind, num: usize) {
	let address = bitcoind.client.new_address().unwrap();
	let _block_hashes = bitcoind.client.generate_to_address(num, &address).unwrap();
}

fn create_lsp(rt: Arc<Runtime>, uuid: Uuid, bitcoind: &Bitcoind) -> Arc<Node> {
	let mut builder = ldk_node::Builder::new();
	builder.set_network(Network::Regtest);
	let mut seed: [u8; 64] = [0; 64];
	rand::thread_rng().fill_bytes(&mut seed);
	builder.set_entropy_seed_bytes(seed);
	builder.set_gossip_source_p2p();

	builder.set_node_alias("LSP".to_string()).unwrap();

	let cookie = bitcoind.params.get_cookie_values().unwrap().unwrap();
	builder.set_chain_source_bitcoind_rpc(
		"127.0.0.1".to_string(),
		bitcoind.params.rpc_socket.port(),
		cookie.user,
		cookie.password,
	);

	let tmp = temp_dir().join(format!("orange-test-{uuid}-lsp"));
	builder.set_storage_dir_path(tmp.to_str().unwrap().to_string());

	let lsps2_service_config = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm: 10_000,
		channel_over_provisioning_ppm: 100_000,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 10_000,
		min_channel_opening_fee_msat: 0,
		max_client_to_self_delay: 1024,
	};
	builder.set_liquidity_provider_lsps2(lsps2_service_config);

	let port = get_available_port().unwrap();
	let addr = SocketAddress::TcpIpV4 { addr: [127, 0, 0, 1], port };
	builder.set_listening_addresses(vec![addr.clone()]).unwrap();
	builder.set_listening_addresses(vec![addr]).unwrap();

	let ldk_node = Arc::new(builder.build().unwrap());

	ldk_node.start_with_runtime(Arc::clone(&rt)).unwrap();

	let events_ref = ldk_node.clone();
	rt.spawn(async move {
		loop {
			let event = events_ref.next_event_async().await;
			println!("LSP: {event:?}");
			if let Err(e) = events_ref.event_handled() {
				eprintln!("Error handling LSP ldk event: {e}");
			}
		}
	});

	ldk_node
}

fn create_third_party(rt: Arc<Runtime>, uuid: Uuid, bitcoind: &Bitcoind) -> Arc<Node> {
	let mut builder = ldk_node::Builder::new();
	builder.set_network(Network::Regtest);
	let mut seed: [u8; 64] = [0; 64];
	rand::thread_rng().fill_bytes(&mut seed);
	builder.set_entropy_seed_bytes(seed);
	builder.set_gossip_source_p2p();

	builder.set_node_alias("third-party".to_string()).unwrap();

	let cookie = bitcoind.params.get_cookie_values().unwrap().unwrap();
	builder.set_chain_source_bitcoind_rpc(
		"127.0.0.1".to_string(),
		bitcoind.params.rpc_socket.port(),
		cookie.user,
		cookie.password,
	);

	let tmp = temp_dir().join(format!("orange-test-{uuid}-payer"));
	builder.set_storage_dir_path(tmp.to_str().unwrap().to_string());

	let port = get_available_port().unwrap();
	let addr = SocketAddress::TcpIpV4 { addr: [127, 0, 0, 1], port };
	builder.set_listening_addresses(vec![addr.clone()]).unwrap();
	builder.set_listening_addresses(vec![addr]).unwrap();

	let ldk_node = Arc::new(builder.build().unwrap());

	ldk_node.start_with_runtime(Arc::clone(&rt)).unwrap();

	let events_ref = ldk_node.clone();
	rt.spawn(async move {
		loop {
			let event = events_ref.next_event_async().await;
			println!("3rd party: {event:?}");
			if let Err(e) = events_ref.event_handled() {
				eprintln!("Error handling LSP ldk event: {e}");
			}
		}
	});

	ldk_node
}

fn fund_node(node: &Node, bitcoind: &Bitcoind) {
	let addr = node.onchain_payment().new_address().unwrap();
	bitcoind.client.send_to_address(&addr, bitcoin::Amount::from_btc(1.0).unwrap()).unwrap();
	generate_blocks(bitcoind, 6);
}

fn build_wallet_config<E>(
	uuid: Uuid, bitcoind: &Bitcoind, lsp_pk: PublicKey, socket_addr: SocketAddress, extra_config: E,
) -> WalletConfig<E> {
	let tmp = temp_dir().join(format!("orange-test-{uuid}/ldk-node"));
	let cookie = bitcoind.params.get_cookie_values().unwrap().unwrap();
	let mut seed: [u8; 64] = [0; 64];
	rand::thread_rng().fill_bytes(&mut seed);
	WalletConfig {
		storage_config: StorageConfig::LocalSQLite(tmp.to_str().unwrap().to_string()),
		log_file: tmp.join("orange.log"),
		chain_source: ChainSource::BitcoindRPC {
			host: "127.0.0.1".to_string(),
			port: bitcoind.params.rpc_socket.port(),
			user: cookie.user,
			password: cookie.password,
		},
		lsp: (socket_addr, lsp_pk, None),
		network: Network::Regtest,
		seed: Seed::Seed64(seed),
		tunables: Default::default(),
		extra_config,
	}
}

pub struct TestParams {
	pub wallet: Wallet<DummyTrustedWallet>,
	pub lsp: Arc<Node>,
	pub third_party: Arc<Node>,
	pub bitcoind: Arc<Bitcoind>,
	pub rt: Arc<Runtime>,
}

pub fn build_test_nodes() -> TestParams {
	let bitcoind = Arc::new(create_bitcoind());
	generate_blocks(&bitcoind, 101);

	let rt = Arc::new(tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap());

	let test_id = Uuid::new_v4();

	let lsp = create_lsp(rt.clone(), test_id, &bitcoind);
	fund_node(&lsp, &bitcoind);
	let third_party = create_third_party(rt.clone(), test_id, &bitcoind);
	let start_bal = third_party.list_balances().total_onchain_balance_sats;
	fund_node(&third_party, &bitcoind);

	// wait for node to sync (needs blocking wait as we are not in async context here)
	let third = third_party.clone();
	rt.block_on(async move {
		wait_for_condition(
			Duration::from_secs(1),
			10,
			"third_party node sync after funding",
			|| {
				let res = third.list_balances().total_onchain_balance_sats > start_bal;
				async move { res }
			},
		)
		.await
		.expect("Third party node did not sync balance in time");
	});

	let lsp_listen = lsp.listening_addresses().unwrap().first().unwrap().clone();

	// open a channel from payer to LSP
	third_party.open_channel(lsp.node_id(), lsp_listen.clone(), 10_000_000, None, None).unwrap();
	// wait for tx to broadcast
	std::thread::sleep(Duration::from_secs(1)); // Keep short sleep for broadcast
	generate_blocks(&bitcoind, 10);

	// wait for channel ready (needs blocking wait as we are not in async context here)
	let third_party_clone = third_party.clone();
	rt.block_on(async move {
		wait_for_condition(Duration::from_secs(1), 10, "channel to become usable", || {
			let res = third_party_clone.list_channels().first().is_some_and(|c| c.is_usable);
			async move { res }
		})
		.await
		.expect("Channel did not become usable in time");
	});

	// make sure it actually became usable
	assert!(third_party.list_channels().first().unwrap().is_usable);

	let dummy_wallet_config = DummyTrustedWalletExtraConfig {
		uuid: test_id,
		lsp: lsp.clone(),
		bitcoind: bitcoind.clone(),
		rt: rt.clone(),
	};

	let config =
		build_wallet_config(test_id, &bitcoind, lsp.node_id(), lsp_listen, dummy_wallet_config);

	let rt_clone = rt.clone();
	let wallet = rt.block_on(async move { Wallet::new(rt_clone, config).await.unwrap() });

	TestParams { wallet, lsp, third_party, bitcoind, rt }
}

pub async fn open_channel_from_lsp(
	wallet: &Wallet<DummyTrustedWallet>, payer: Arc<Node>,
) -> Amount {
	let starting_bal = wallet.get_balance().await;

	// recv 2x the trusted balance limit to trigger a lightning channel
	let limit = wallet.get_tunables();
	let recv_amt = limit.trusted_balance_limit.saturating_add(limit.trusted_balance_limit);

	let uri = wallet.get_single_use_receive_uri(Some(recv_amt)).await.unwrap();
	let payment_id = payer.bolt11_payment().send(&uri.invoice, None).unwrap();

	// wait for payment success from payer side
	let p = payer.clone();
	wait_for_condition(Duration::from_secs(1), 10, "payer payment success", || {
		let res = p.payment(&payment_id).is_some_and(|p| p.status == PaymentStatus::Succeeded);
		async move { res }
	})
	.await
	.expect("Payer payment did not succeed in time");

	// check we received with a channel, wait for balance update on wallet side
	wait_for_condition(
		Duration::from_secs(1),
		10,
		"wallet balance update after channel open",
		|| async { wallet.get_balance().await.available_balance > starting_bal.available_balance },
	)
	.await
	.expect("Wallet balance did not update in time after channel open");

	let event = wait_next_event(&wallet).await;
	wallet.event_handled().unwrap();
	assert!(matches!(event, orange_sdk::Event::ChannelOpened { .. }));

	let event = wait_next_event(&wallet).await;
	wallet.event_handled().unwrap();
	match event {
		orange_sdk::Event::PaymentReceived { payment_hash, amount_msat, lsp_fee_msats, .. } => {
			assert!(lsp_fee_msats.is_some()); // we expect a fee to be paid for opening a channel
			assert_eq!(recv_amt.milli_sats(), amount_msat + lsp_fee_msats.unwrap_or(0)); // the fee will be deducted from the amount received
			assert_eq!(payment_hash.0, uri.invoice.payment_hash().to_byte_array());
		},
		_ => panic!("Expected PaymentReceived event"),
	}

	assert_eq!(wallet.next_event(), None);

	recv_amt
}
