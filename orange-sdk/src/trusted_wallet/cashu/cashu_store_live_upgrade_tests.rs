//! Funded old-version wallets, a real local mint, and a fake Lightning backend.
use super::*;
use axum::extract::{Request, State as HttpState};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use cdk::nuts::MeltQuoteState;
use cdk::wallet::types::{MeltSagaState, TransactionStatus, WalletSagaState};
use cdk_common::payment::{MintPayment, PaymentIdentifier};
use std::process::Stdio;
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct HttpControl {
	hold_next_melt: AtomicBool,
	response_ready: Notify,
	release_response: Notify,
	offline: AtomicBool,
	offline_requests: AtomicUsize,
	melt_requests: AtomicUsize,
	restore_requests: AtomicUsize,
}

async fn control_http(
	HttpState(control): HttpState<Arc<HttpControl>>, request: Request, next: Next,
) -> Response {
	if control.offline.load(Ordering::SeqCst) {
		control.offline_requests.fetch_add(1, Ordering::SeqCst);
		return StatusCode::SERVICE_UNAVAILABLE.into_response();
	}
	let is_melt = request.method() == Method::POST && request.uri().path() == "/v1/melt/bolt11";
	if is_melt {
		control.melt_requests.fetch_add(1, Ordering::SeqCst);
	}
	if request.method() == Method::POST && request.uri().path() == "/v1/restore" {
		control.restore_requests.fetch_add(1, Ordering::SeqCst);
	}
	let hold = is_melt && control.hold_next_melt.swap(false, Ordering::SeqCst);
	let response = next.run(request).await;
	if hold {
		// The mint has handled the submission, but the old wallet cannot see its
		// response. Kill the wallet here to test an actual lost acknowledgement.
		control.response_ready.notify_one();
		control.release_response.notified().await;
	}
	response
}

struct LiveMint {
	mint: Arc<cdk::mint::Mint>,
	backend: Arc<cdk_fake_wallet::FakeWallet>,
	url: String,
	control: Arc<HttpControl>,
	server: tokio::task::JoinHandle<()>,
}

impl LiveMint {
	async fn new() -> Self {
		let mint_db = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
		let backend = Arc::new(cdk_fake_wallet::FakeWallet::new(
			// FakeWallet charges one sat. Reserve two to require change recovery.
			cdk::types::FeeReserve { min_fee_reserve: 2.into(), percent_fee_reserve: 0.0 },
			Default::default(),
			Default::default(),
			0,
			CurrencyUnit::Sat,
		));
		let mut builder = cdk::mint::MintBuilder::new(mint_db.clone());
		builder.configure_unit(CurrencyUnit::Sat, cdk::mint::UnitConfig::default()).unwrap();
		builder
			.add_payment_processor(
				CurrencyUnit::Sat,
				cdk::nuts::PaymentMethod::BOLT11,
				cdk::mint::MintMeltLimits::new(0, u64::MAX),
				backend.clone(),
			)
			.await
			.unwrap();
		let mint = Arc::new(builder.build_with_seed(mint_db, &[42; 64]).await.unwrap());
		mint.start().await.unwrap();
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let control = Arc::new(HttpControl::default());
		let router = cdk_axum::create_mint_router(mint.clone(), vec!["bolt11".into()])
			.await
			.unwrap()
			.layer(axum::middleware::from_fn_with_state(control.clone(), control_http));
		let server = tokio::spawn(async move {
			axum::serve(listener, router).await.unwrap();
		});
		Self { mint, backend, url, control, server }
	}

	async fn old_wallet(
		&self, submitted: bool, outcome: MeltQuoteState,
	) -> (std::path::PathBuf, Arc<dyn DynStore>) {
		let generator = std::env::var_os("ORANGE_CDK016_GENERATOR")
			.expect("run ci/test-cashu-upgrade.sh to build the old-version generator");
		let description = cdk_fake_wallet::FakeInvoiceDescription {
			pay_invoice_state: outcome,
			check_payment_state: outcome,
			pay_err: false,
			check_err: false,
		};
		let invoice = cdk_fake_wallet::create_fake_invoice(
			8000,
			serde_json::to_string(&description).unwrap(),
		)
		.to_string();
		let (path, store) = migration_store();
		drop(store);
		let mut command = tokio::process::Command::new(generator);
		command
			.arg(&path)
			.arg(&self.url)
			.arg(invoice)
			.kill_on_drop(true)
			.stdout(Stdio::null())
			.stderr(Stdio::inherit());
		if submitted {
			self.control.hold_next_melt.store(true, Ordering::SeqCst);
			command.arg("submit");
		}
		let mut child = command.spawn().unwrap();
		if submitted {
			tokio::select! {
				_ = self.control.response_ready.notified() => {},
				status = child.wait() => panic!("old wallet exited before the response gate: {status:?}"),
			}
			child.kill().await.unwrap();
			assert!(!child.wait().await.unwrap().success());
			self.control.release_response.notify_one();
		} else {
			assert!(child.wait().await.unwrap().success());
		}
		let store: Arc<dyn DynStore> = Arc::new(
			ldk_node::io::sqlite_store::SqliteStore::new(
				path.clone(),
				Some("orange.sqlite".into()),
				None,
			)
			.unwrap(),
		);
		write_has_recovered(&store, true).await.unwrap();
		(path, store)
	}

	async fn stop(self) {
		self.server.abort();
		let _ = self.server.await;
		self.mint.stop().await.unwrap();
	}
}

fn close_wallet(cashu: crate::trusted_wallet::cashu::Cashu) {
	cashu.runtime.abort_cancellable_background_tasks();
}

fn run_upgrade_test(test: impl Future<Output = std::path::PathBuf>) {
	let runtime =
		tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
	let path = runtime.block_on(async {
		tokio::time::timeout(Duration::from_secs(90), test)
			.await
			.expect("funded wallet upgrade timed out")
	});
	// Cancellation does not stop SQLite spawn_blocking work already in flight.
	// Drain it before deleting the database, rather than racing its last access.
	drop(runtime);
	std::fs::remove_dir_all(path).unwrap();
}

#[test]
#[ignore = "requires the CDK 0.16 fixture generator; run ci/test-cashu-upgrade.sh"]
fn cdk016_funded_wallet_recovers_interrupted_melt_and_spends_after_upgrade() {
	run_upgrade_test(async {
		let mint = LiveMint::new().await;
		let (path, store) = mint.old_wallet(false, MeltQuoteState::Paid).await;
		let cashu = open_cashu(store.clone(), &mint.url).await.unwrap();
		let wallet = &cashu.cashu_wallet;
		assert_eq!(wallet.total_balance().await.unwrap(), cdk::Amount::from(64));
		assert!(wallet.localstore.get_incomplete_sagas().await.unwrap().is_empty());
		assert!(wallet.recover_incomplete_sagas().await.unwrap().is_empty());
		let invoice =
			cdk_fake_wallet::create_fake_invoice(8000, "after upgrade".into()).to_string();
		let quote =
			wallet.melt_quote(cdk::nuts::PaymentMethod::BOLT11, invoice, None, None).await.unwrap();
		wallet.prepare_melt(&quote.id, Default::default()).await.unwrap().confirm().await.unwrap();
		assert_eq!(wallet.total_balance().await.unwrap(), cdk::Amount::from(55));
		close_wallet(cashu);
		let cashu = open_cashu(store.clone(), &mint.url).await.unwrap();
		assert_eq!(cashu.cashu_wallet.total_balance().await.unwrap(), cdk::Amount::from(55));
		assert_eq!(
			cashu
				.cashu_wallet
				.localstore
				.list_transactions(None, Some(TransactionDirection::Outgoing), None)
				.await
				.unwrap()
				.len(),
			1
		);
		close_wallet(cashu);
		assert_eq!(mint.control.melt_requests.load(Ordering::SeqCst), 1);
		drop(store);
		mint.stop().await;
		path
	});
}

fn recover_submitted_melt(outcome: MeltQuoteState, offline_first: bool) {
	run_upgrade_test(async {
		let mint = LiveMint::new().await;
		let (path, store) = mint.old_wallet(true, outcome).await;
		let db = CashuKvDatabase::new(store.clone()).await.unwrap();
		let sagas = db.get_incomplete_sagas().await.unwrap();
		assert_eq!(sagas.len(), 1);
		assert_eq!(sagas[0].state, WalletSagaState::Melt(MeltSagaState::MeltRequested));
		let operation = sagas[0].id;
		let quote = db.get_melt_quotes().await.unwrap().remove(0);
		let quote_id = quote.id;
		let invoice: ldk_node::lightning_invoice::Bolt11Invoice = quote.request.parse().unwrap();
		let reserved = db.get_reserved_proofs(&operation).await.unwrap();
		assert!(!reserved.is_empty());
		assert!(reserved.iter().all(|proof| proof.state == State::Pending));
		assert_eq!(reserved.iter().map(|proof| u64::from(proof.proof.amount)).sum::<u64>(), 10);
		assert_eq!(db.get_balance(None, None, Some(vec![State::Unspent])).await.unwrap(), 54);
		drop(db);
		// Async melts can return PENDING before the fake backend finishes. Wait
		// for the mint's actual result, independently of the lost HTTP response.
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				let state =
					mint.mint.check_melt_quote(&quote_id.parse().unwrap()).await.unwrap().state();
				if state == outcome
					|| (outcome == MeltQuoteState::Failed && state == MeltQuoteState::Unpaid)
				{
					break;
				}
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
		})
		.await
		.expect("mint did not reach the requested outcome");
		// In particular, the failed case must reach the payment backend, rather
		// than passing because the mint rejected an invalid request up front.
		let payment = mint
			.backend
			.check_outgoing_payment(&PaymentIdentifier::PaymentHash(invoice.payment_hash().0))
			.await
			.unwrap();
		assert_eq!(payment.status, outcome);
		if offline_first {
			mint.control.offline.store(true, Ordering::SeqCst);
			let cashu = open_cashu(store.clone(), &mint.url).await.unwrap();
			assert_eq!(cashu.cashu_wallet.total_balance().await.unwrap(), cdk::Amount::from(54));
			assert_eq!(
				cashu.cashu_wallet.localstore.get_reserved_proofs(&operation).await.unwrap(),
				reserved
			);
			assert_eq!(
				cashu.cashu_wallet.localstore.get_incomplete_sagas().await.unwrap().len(),
				1
			);
			assert!(mint.control.offline_requests.load(Ordering::SeqCst) > 0);
			close_wallet(cashu);
			mint.control.offline.store(false, Ordering::SeqCst);
		}
		for _ in 0..2 {
			let cashu = open_cashu(store.clone(), &mint.url).await.unwrap();
			let wallet = &cashu.cashu_wallet;
			let db = &wallet.localstore;
			let transactions = db
				.list_transactions(None, Some(TransactionDirection::Outgoing), None)
				.await
				.unwrap();
			match outcome {
				MeltQuoteState::Paid => {
					assert_eq!(wallet.total_balance().await.unwrap(), cdk::Amount::from(55));
					assert!(db.get_incomplete_sagas().await.unwrap().is_empty());
					assert_eq!(transactions.len(), 1);
					assert_eq!(transactions[0].status, TransactionStatus::Completed);
					assert_eq!(transactions[0].quote_id.as_deref(), Some(quote_id.as_str()));
					assert_eq!(transactions[0].amount, cdk::Amount::from(8));
					assert_eq!(transactions[0].fee, cdk::Amount::from(1));
					let spent =
						db.get_proofs_by_ys(reserved.iter().map(|p| p.y).collect()).await.unwrap();
					assert_eq!(spent.len(), reserved.len());
					assert!(spent.iter().all(|p| p.state == State::Spent));
				},
				MeltQuoteState::Failed => {
					assert_eq!(wallet.total_balance().await.unwrap(), cdk::Amount::from(64));
					assert!(db.get_incomplete_sagas().await.unwrap().is_empty());
					assert!(db.get_reserved_proofs(&operation).await.unwrap().is_empty());
					assert!(
						transactions.len() <= 1
							&& transactions.iter().all(|t| t.status == TransactionStatus::Failed)
					);
				},
				MeltQuoteState::Pending => {
					assert_eq!(wallet.total_balance().await.unwrap(), cdk::Amount::from(54));
					assert_eq!(db.get_reserved_proofs(&operation).await.unwrap(), reserved);
					assert_eq!(db.get_incomplete_sagas().await.unwrap().len(), 1);
					assert!(
						transactions.len() <= 1
							&& transactions.iter().all(|t| t.status == TransactionStatus::Pending)
					);
				},
				_ => unreachable!(),
			}
			close_wallet(cashu);
			assert_eq!(
				mint.control.melt_requests.load(Ordering::SeqCst),
				1,
				"recovery must not submit another payment"
			);
		}
		if outcome == MeltQuoteState::Paid {
			assert!(
				mint.control.restore_requests.load(Ordering::SeqCst) > 0,
				"lost change must be restored from the mint"
			);
		}
		drop(store);
		mint.stop().await;
		path
	});
}

#[test]
#[ignore = "requires the CDK 0.16 fixture generator; run ci/test-cashu-upgrade.sh"]
fn cdk016_funded_wallet_recovers_paid_melt_and_change_after_lost_response() {
	recover_submitted_melt(MeltQuoteState::Paid, false);
}

#[test]
#[ignore = "requires the CDK 0.16 fixture generator; run ci/test-cashu-upgrade.sh"]
fn cdk016_funded_wallet_releases_failed_melt_after_lost_response() {
	recover_submitted_melt(MeltQuoteState::Failed, false);
}

#[test]
#[ignore = "requires the CDK 0.16 fixture generator; run ci/test-cashu-upgrade.sh"]
fn cdk016_funded_wallet_keeps_pending_melt_reserved_after_lost_response() {
	recover_submitted_melt(MeltQuoteState::Pending, false);
}

#[test]
#[ignore = "requires the CDK 0.16 fixture generator; run ci/test-cashu-upgrade.sh"]
fn cdk016_funded_wallet_retries_recovery_after_mint_unavailability() {
	recover_submitted_melt(MeltQuoteState::Paid, true);
}
