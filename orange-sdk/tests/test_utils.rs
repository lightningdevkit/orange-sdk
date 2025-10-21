#![cfg(feature = "_test-utils")]

use bitcoin_payment_instructions::amount::Amount;
#[cfg(feature = "_cashu-tests")]
use cdk::mint::{MintBuilder, MintMeltLimits};
#[cfg(feature = "_cashu-tests")]
use cdk::types::FeeReserve;
#[cfg(feature = "_cashu-tests")]
use cdk_ldk_node::{BitcoinRpcConfig, GossipSource};
use corepc_node::client::bitcoin::Network;
use corepc_node::{Conf, Node as Bitcoind, get_available_port};
use ldk_node::bitcoin::hashes::Hash;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::liquidity::LSPS2ServiceConfig;
use ldk_node::payment::PaymentStatus;
use ldk_node::{Node, bitcoin};
#[cfg(not(feature = "_cashu-tests"))]
use orange_sdk::trusted_wallet::dummy::DummyTrustedWalletExtraConfig;
use orange_sdk::{
	ChainSource, ExtraConfig, LoggerType, Seed, StorageConfig, Tunables, Wallet, WalletConfig,
};
use rand::RngCore;
use std::env::temp_dir;
use std::future::Future;
#[cfg(feature = "_cashu-tests")]
use std::net::SocketAddr;
#[cfg(feature = "_cashu-tests")]
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Waits for an async condition to be true, polling at a specified interval until a timeout.
pub async fn wait_for_condition<F, Fut>(condition_name: &str, mut condition: F)
where
	F: FnMut() -> Fut,
	Fut: Future<Output = bool>,
{
	// longer timeout if running in CI as things can be slower there
	let iterations = if std::env::var("CI").is_ok() { 120 } else { 20 };
	for _ in 0..iterations {
		if condition().await {
			return;
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
	panic!("Timeout waiting for condition: {condition_name}");
}

/// Waits for the next event from the wallet, polling every second for up to 20 seconds.
/// If no event is received within this time, it panics.
/// This is useful for testing purposes to ensure that the wallet is responsive and events are being processed.
/// Otherwise, we can have the test hang indefinitely.
pub async fn wait_next_event(wallet: &orange_sdk::Wallet) -> orange_sdk::Event {
	// longer timeout if running in CI as things can be slower there
	let timeout = if std::env::var("CI").is_ok() { 60 } else { 20 };
	let event = tokio::time::timeout(Duration::from_secs(timeout), wallet.next_event_async())
		.await
		.unwrap();
	wallet.event_handled().unwrap();
	event
}

fn create_bitcoind(uuid: Uuid) -> Bitcoind {
	let mut conf = Conf::default();
	conf.args.push("-txindex");
	conf.args.push("-rpcworkqueue=100");
	conf.staticdir = Some(temp_dir().join(format!("orange-test-{uuid}/bitcoind")));
	let bitcoind = Bitcoind::with_conf(corepc_node::downloaded_exe_path().unwrap(), &conf)
		.unwrap_or_else(|_| panic!("Failed to start bitcoind for test {uuid}"));

	// Wait for bitcoind to be ready before returning
	wait_for_bitcoind_ready(&bitcoind);

	// mine 101 blocks to get some spendable funds, split it up into multiple calls
	// to avoid potentially hitting RPC timeouts on slower CI systems
	let address = bitcoind.client.new_address().unwrap();
	for _ in 0..101 {
		let _block_hashes = bitcoind.client.generate_to_address(1, &address).unwrap();
	}

	bitcoind
}

fn wait_for_bitcoind_ready(bitcoind: &Bitcoind) {
	let max_attempts = 30;
	let delay = Duration::from_millis(500);

	for attempt in 0..max_attempts {
		match bitcoind.client.get_blockchain_info() {
			Ok(_) => {
				println!("bitcoind ready after {attempt} attempts");
				return;
			},
			Err(e) => {
				if attempt == max_attempts {
					panic!("bitcoind failed to become ready after {max_attempts} attempts: {e}");
				}
				println!("bitcoind not ready, attempt {attempt}/{max_attempts}: {e}");
				std::thread::sleep(delay);
			},
		}
	}
}

pub fn generate_blocks(bitcoind: &Bitcoind, num: usize) {
	let address = bitcoind.client.new_address().unwrap();
	let _block_hashes = bitcoind
		.client
		.generate_to_address(num, &address)
		.unwrap_or_else(|_| panic!("failed to generate {num} blocks"));
}

fn create_lsp(uuid: Uuid, bitcoind: &Bitcoind) -> Arc<Node> {
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

	let tmp = temp_dir().join(format!("orange-test-{uuid}/lsp"));
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

	ldk_node.start().unwrap();

	let events_ref = Arc::clone(&ldk_node);
	tokio::spawn(async move {
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

fn create_third_party(uuid: Uuid, bitcoind: &Bitcoind) -> Arc<Node> {
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

	let tmp = temp_dir().join(format!("orange-test-{uuid}/payer"));
	builder.set_storage_dir_path(tmp.to_str().unwrap().to_string());

	let port = get_available_port().unwrap();
	let addr = SocketAddress::TcpIpV4 { addr: [127, 0, 0, 1], port };
	builder.set_listening_addresses(vec![addr.clone()]).unwrap();
	builder.set_listening_addresses(vec![addr]).unwrap();

	let ldk_node = Arc::new(builder.build().unwrap());

	ldk_node.start().unwrap();

	let events_ref = Arc::clone(&ldk_node);
	tokio::spawn(async move {
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

pub struct TestParams {
	pub wallet: orange_sdk::Wallet,
	pub lsp: Arc<Node>,
	pub third_party: Arc<Node>,
	pub bitcoind: Arc<Bitcoind>,
	#[cfg(feature = "_cashu-tests")]
	pub _mint: Arc<cdk::Mint>,
}

pub async fn build_test_nodes() -> TestParams {
	let test_id = Uuid::now_v7();
	let bitcoind = Arc::new(create_bitcoind(test_id));

	let lsp = create_lsp(test_id, &bitcoind);
	fund_node(&lsp, &bitcoind);
	let third_party = create_third_party(test_id, &bitcoind);
	let start_bal = third_party.list_balances().total_onchain_balance_sats;
	fund_node(&third_party, &bitcoind);

	// wait for node to sync (needs blocking wait as we are not in async context here)
	let third = Arc::clone(&third_party);
	wait_for_condition("third_party node sync after funding", || {
		let res = third.list_balances().total_onchain_balance_sats > start_bal;
		async move { res }
	})
	.await;

	let lsp_listen = lsp.listening_addresses().unwrap().first().unwrap().clone();

	// open a channel from payer to LSP
	third_party.open_channel(lsp.node_id(), lsp_listen.clone(), 10_000_000, None, None).unwrap();
	wait_for_tx_broadcast(&bitcoind);
	generate_blocks(&bitcoind, 6);

	// wait for channel ready (needs blocking wait as we are not in async context here)
	let third_party_clone = Arc::clone(&third_party);
	wait_for_condition("channel to become usable", || {
		let res = third_party_clone.list_channels().first().is_some_and(|c| c.is_usable);
		async move { res }
	})
	.await;

	// make sure it actually became usable
	assert!(third_party.list_channels().first().unwrap().is_usable);

	let lsp_node_id = lsp.node_id();
	let mut seed: [u8; 64] = [0; 64];
	rand::thread_rng().fill_bytes(&mut seed);

	#[cfg(not(feature = "_cashu-tests"))]
	let wallet: orange_sdk::Wallet = {
		let dummy_wallet_config = DummyTrustedWalletExtraConfig {
			uuid: test_id,
			lsp: Arc::clone(&lsp),
			bitcoind: Arc::clone(&bitcoind),
		};

		let tmp = temp_dir().join(format!("orange-test-{test_id}/ldk-node"));
		let cookie = bitcoind.params.get_cookie_values().unwrap().unwrap();

		let bitcoind_port = bitcoind.params.rpc_socket.port();

		let wallet_config = WalletConfig {
			storage_config: StorageConfig::LocalSQLite(tmp.to_str().unwrap().to_string()),
			logger_type: LoggerType::LogFacade,
			scorer_url: None,
			rgs_url: None,
			tunables: Tunables::default(),
			chain_source: ChainSource::BitcoindRPC {
				host: "127.0.0.1".to_string(),
				port: bitcoind_port,
				user: cookie.user,
				password: cookie.password,
			},
			lsp: (lsp_listen, lsp_node_id, None),
			network: Network::Regtest,
			seed: Seed::Seed64(seed),
			extra_config: ExtraConfig::Dummy(dummy_wallet_config),
		};

		Wallet::new(wallet_config).await.unwrap()
	};

	#[cfg(feature = "_cashu-tests")]
	{
		let tmp = temp_dir().join(format!("orange-test-{test_id}/cashu-ldk-node"));
		let cookie = bitcoind.params.get_cookie_values().unwrap().unwrap();
		let bitcoind_port = bitcoind.params.rpc_socket.port();
		let cdk_port = {
			let t = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
			t.local_addr().unwrap().port()
		};
		let cdk_addr = SocketAddr::from_str(format!("127.0.0.1:{cdk_port}").as_str()).unwrap();
		let cdk = cdk_ldk_node::CdkLdkNode::new(
			Network::Regtest,
			cdk_ldk_node::ChainSource::BitcoinRpc(BitcoinRpcConfig {
				host: "127.0.0.1".to_string(),
				port: bitcoind_port,
				user: cookie.user.clone(),
				password: cookie.password.clone(),
			}),
			GossipSource::P2P,
			tmp.to_str().unwrap().to_string(),
			FeeReserve { min_fee_reserve: Default::default(), percent_fee_reserve: 0.0 },
			vec![cdk_addr.into()],
			None,
		)
		.unwrap();
		let cdk = Arc::new(cdk);

		let mint_addr = {
			let t = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
			let port = t.local_addr().unwrap().port();
			SocketAddr::from_str(format!("127.0.0.1:{port}").as_str()).unwrap()
		};

		let bitcoind_clone = Arc::clone(&bitcoind);
		let lsp_listen_clone = lsp_listen.clone();
		let mint = {
			// build mint
			let mem_db = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
			let mut mint_seed: [u8; 64] = [0; 64];
			rand::thread_rng().fill_bytes(&mut mint_seed);
			let mut builder = MintBuilder::new(mem_db.clone());

			builder
				.add_payment_processor(
					orange_sdk::CurrencyUnit::Sat,
					cdk::nuts::PaymentMethod::Bolt11,
					MintMeltLimits::new(0, u64::MAX),
					cdk.clone(),
				)
				.await
				.unwrap();

			builder
				.add_payment_processor(
					orange_sdk::CurrencyUnit::Sat,
					cdk::nuts::PaymentMethod::Bolt12,
					MintMeltLimits::new(0, u64::MAX),
					cdk.clone(),
				)
				.await
				.unwrap();

			let mint = Arc::new(builder.build_with_seed(mem_db, &mint_seed).await.unwrap());

			mint.start().await.unwrap();

			let listener = tokio::net::TcpListener::bind(mint_addr).await.unwrap();

			let v1_service = cdk_axum::create_mint_router(Arc::clone(&mint), true).await.unwrap();

			let axum_result = axum::serve(listener, v1_service);

			tokio::spawn(async move {
				if let Err(e) = axum_result.await {
					eprintln!("Error running mint axum server: {e}");
				}
			});

			// open channel from cashu ldk node to lsp
			let addr = cdk.node().onchain_payment().new_address().unwrap();
			bitcoind_clone
				.client
				.send_to_address(&addr, bitcoin::Amount::from_btc(1.0).unwrap())
				.unwrap();
			generate_blocks(&bitcoind_clone, 6);

			// wait for cdk node to sync
			wait_for_condition("cdk node sync after funding", || {
				let res = cdk.node().list_balances().total_onchain_balance_sats > 0;
				async move { res }
			})
			.await;

			cdk.node()
				.open_channel(lsp_node_id, lsp_listen_clone, 10_000_000, Some(5_000_000_000), None)
				.unwrap();
			wait_for_tx_broadcast(&bitcoind_clone);
			generate_blocks(&bitcoind_clone, 6);

			// wait for sync/channel ready
			wait_for_condition("cdk channel to become usable", || {
				let res = cdk.node().list_channels().first().is_some_and(|c| c.is_usable);
				async move { res }
			})
			.await;

			mint
		};

		let tmp = temp_dir().join(format!("orange-test-{test_id}/wallet"));
		let wallet_config = WalletConfig {
			storage_config: StorageConfig::LocalSQLite(tmp.to_str().unwrap().to_string()),
			logger_type: LoggerType::LogFacade,
			scorer_url: None,
			tunables: Tunables::default(),
			chain_source: ChainSource::BitcoindRPC {
				host: "127.0.0.1".to_string(),
				port: bitcoind_port,
				user: cookie.user,
				password: cookie.password,
			},
			lsp: (lsp_listen, lsp_node_id, None),
			rgs_url: None,
			network: Network::Regtest,
			seed: Seed::Seed64(seed),
			extra_config: ExtraConfig::Cashu(orange_sdk::CashuConfig {
				mint_url: format!("http://127.0.0.1:{}", mint_addr.port()),
				unit: orange_sdk::CurrencyUnit::Sat,
			}),
		};
		let wallet = Wallet::new(wallet_config).await.unwrap();

		return TestParams { wallet, lsp, third_party, bitcoind, _mint: mint };
	};

	#[cfg(not(feature = "_cashu-tests"))]
	TestParams { wallet, lsp, third_party, bitcoind }
}

pub async fn open_channel_from_lsp(wallet: &orange_sdk::Wallet, payer: Arc<Node>) -> Amount {
	let starting_bal = wallet.get_balance().await.unwrap();

	// recv 2x the trusted balance limit to trigger a lightning channel
	let limit = wallet.get_tunables();
	let recv_amt = limit.trusted_balance_limit.saturating_add(limit.trusted_balance_limit);

	let uri = wallet.get_single_use_receive_uri(Some(recv_amt)).await.unwrap();
	let payment_id = payer.bolt11_payment().send(&uri.invoice, None).unwrap();

	// wait for payment success from payer side
	let p = Arc::clone(&payer);
	wait_for_condition("payer payment success", || {
		let res = p.payment(&payment_id).is_some_and(|p| p.status == PaymentStatus::Succeeded);
		async move { res }
	})
	.await;

	// check we received with a channel, wait for balance update on wallet side
	wait_for_condition("wallet balance update after channel open", || async {
		wallet.get_balance().await.unwrap().available_balance() > starting_bal.available_balance()
	})
	.await;

	let event = wait_next_event(wallet).await;
	assert!(matches!(event, orange_sdk::Event::ChannelOpened { .. }));

	let event = wait_next_event(wallet).await;
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

fn wait_for_tx_broadcast(bitcoind: &Bitcoind) {
	let iterations = if std::env::var("CI").is_ok() { 120 } else { 10 };
	for _ in 0..iterations {
		let num_txs = bitcoind.client.get_mempool_info().unwrap().size;
		if num_txs > 0 {
			break;
		}
		std::thread::sleep(Duration::from_millis(250));
	}
}
