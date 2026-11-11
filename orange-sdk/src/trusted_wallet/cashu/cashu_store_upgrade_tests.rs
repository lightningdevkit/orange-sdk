//! Upgrade tests exercise the public database contract and interrupted storage operations.
use super::tests::{migration_store, proof_info};
use super::*;
use ldk_node::lightning::util::persist::{PageToken, PaginatedListResponse};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

// Exercise the same initialization entry point as Wallet::new, without an LDK node.
async fn open_cashu(
	store: Arc<dyn DynStore>, url: &str,
) -> Result<crate::trusted_wallet::cashu::Cashu, crate::InitFailure> {
	use crate::trusted_wallet::cashu::{Cashu, CashuConfig};
	let cashu_config =
		CashuConfig { mint_url: url.into(), unit: CurrencyUnit::Sat, npubcash_url: None };
	let config = crate::WalletConfig {
		storage_config: crate::StorageConfig::LocalSQLite(String::new()),
		logger_type: crate::logging::LoggerType::LogFacade,
		chain_source: crate::ChainSource::Esplora {
			url: "http://127.0.0.1:1".into(),
			username: None,
			password: None,
		},
		lsp: (
			"127.0.0.1:1".parse().unwrap(),
			"024369d2d22a80ecf78f3937da9d5f30c1b9f74f0c32684d583cca0fa6a61cdcfc".parse().unwrap(),
			None,
		),
		scorer_url: None,
		rgs_url: None,
		network: ldk_node::bitcoin::Network::Regtest,
		seed: crate::Seed::Seed64([7; 64]),
		tunables: Default::default(),
		extra_config: crate::ExtraConfig::Cashu(cashu_config.clone()),
	};
	let logger = Arc::new(crate::logging::Logger::new(&config.logger_type).unwrap());
	let runtime = Arc::new(crate::runtime::Runtime::new(logger.clone()).unwrap());
	let metadata = crate::store::TxMetadataStore::new(store.clone()).await;
	let events = Arc::new(
		crate::EventQueue::new(store.clone(), metadata.clone(), logger.clone())
			.await
			.expect("event queue"),
	);
	Cashu::init(&config, cashu_config, store, events, metadata, logger, runtime).await
}

#[tokio::test]
async fn missing_or_duplicate_proofs_never_partially_reserve() {
	let (path, store) = migration_store();
	let db = CashuKvDatabase::new(store.clone()).await.unwrap();
	let present = proof_info("present", 1);
	let missing = proof_info("missing", 2);
	db.update_proofs(vec![present.clone()], vec![]).await.unwrap();
	let operation = uuid::Uuid::from_u128(7);
	for ys in [vec![present.y, missing.y], vec![missing.y], vec![present.y, present.y]] {
		assert!(matches!(
			db.reserve_proofs(ys, &operation).await,
			Err(cdk::cdk_database::Error::ProofNotUnspent)
		));
		assert_eq!(db.get_proofs(None, None, None, None).await.unwrap(), vec![present.clone()]);
	}
	drop(db);
	let reopened = CashuKvDatabase::new(store.clone()).await.unwrap();
	assert_eq!(reopened.get_proofs(None, None, None, None).await.unwrap(), vec![present]);
	drop(reopened);
	drop(store);
	std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn startup_recovers_legacy_reservations_even_when_seed_restore_is_complete() {
	let runtime =
		tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
	let path = runtime.block_on(async {
		let (path, store) = migration_store();
		seed_legacy_store(&store).await;
		let db = CashuKvDatabase::new(store.clone()).await.unwrap();
		let mut saga = db.get_saga(&uuid::Uuid::from_u128(1)).await.unwrap().unwrap();
		saga.state = cdk::wallet::types::WalletSagaState::Melt(
			cdk::wallet::types::MeltSagaState::ProofsReserved,
		);
		db.add_saga(saga).await.unwrap();
		// Synthetic invoices should not be monitored; recovery itself needs no HTTP here.
		db.remove_mint_quote("mint-quote").await.unwrap();
		drop(db);
		write_has_recovered(&store, true).await.unwrap();
		for _ in 0..2 {
			let cashu = open_cashu(store.clone(), "https://mint.example.com").await.unwrap();
			let db = &cashu.cashu_wallet.localstore;
			assert_eq!(cashu.cashu_wallet.total_balance().await.unwrap(), cdk::Amount::from(3));
			assert!(db.get_incomplete_sagas().await.unwrap().is_empty());
			assert!(db.get_reserved_proofs(&uuid::Uuid::from_u128(1)).await.unwrap().is_empty());
			cashu.runtime.abort_cancellable_background_tasks();
		}
		drop(store);
		path
	});
	// Aborted wallet tasks may have already dispatched SQLite spawn_blocking
	// work. Runtime shutdown drains it before the directory is removed.
	drop(runtime);
	std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn startup_preserves_reservations_on_unreadable_or_failed_recovery() {
	let runtime =
		tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
	let paths = runtime.block_on(async {
		let mut paths = Vec::new();
		for malformed in [false, true] {
			let (path, store) = migration_store();
			seed_legacy_store(&store).await;
			let db = CashuKvDatabase::new(store.clone()).await.unwrap();
			let id = uuid::Uuid::from_u128(1);
			let reserved = db.get_reserved_proofs(&id).await.unwrap();
			// Avoid background requests for the synthetic invoice and seed restoration.
			db.remove_mint_quote("mint-quote").await.unwrap();
			write_has_recovered(&store, true).await.unwrap();
			if malformed {
				store
					.write_async(
						CASHU_PRIMARY_KEY,
						SAGAS_KEY,
						&id.to_string(),
						b"invalid JSON".to_vec(),
					)
					.await
					.unwrap();
			} else {
				let mut saga = db.get_saga(&id).await.unwrap().unwrap();
				// Valid serialization, but recovery cannot interpret melt data as a swap.
				saga.state = cdk::wallet::types::WalletSagaState::Swap(
					cdk::wallet::types::SwapSagaState::ProofsReserved,
				);
				db.add_saga(saga).await.unwrap();
			}
			drop(db);
			let result = open_cashu(store.clone(), "http://127.0.0.1:1").await;
			if malformed {
				assert!(matches!(
					result,
					Err(crate::InitFailure::TrustedFailure(TrustedError::WalletOperationFailed(_)))
				));
			} else {
				let cashu = result.expect("individual recovery failures must not block startup");
				assert_eq!(
					cashu.cashu_wallet.localstore.get_incomplete_sagas().await.unwrap().len(),
					1
				);
				cashu.runtime.abort_cancellable_background_tasks();
			}
			let db = CashuKvDatabase::new(store.clone()).await.unwrap();
			assert_eq!(db.get_reserved_proofs(&id).await.unwrap(), reserved);
			paths.push(path);
		}
		paths
	});
	drop(runtime);
	for path in paths {
		std::fs::remove_dir_all(path).unwrap();
	}
}

macro_rules! upstream_tests {
    ($($name:ident),* $(,)?) => { $(
        #[tokio::test]
        async fn $name() {
            let (path, store) = migration_store();
            let db = CashuKvDatabase::new(store.clone()).await.unwrap();
            cdk_common::database::wallet::test::$name(db).await;
            drop(store);
            std::fs::remove_dir_all(path).unwrap();
        }
    )* };
}

upstream_tests!(
	add_and_get_mint,
	add_mint_without_info,
	remove_mint,
	update_mint_url,
	add_and_get_keysets,
	get_keyset_by_id_in_transaction,
	add_and_get_keys,
	get_keys_in_transaction,
	remove_keys,
	add_and_get_mint_quote,
	get_mint_quote_in_transaction,
	remove_mint_quote,
	add_and_get_melt_quote,
	get_melt_quote_in_transaction,
	remove_melt_quote,
	add_mint_quote_optimistic_locking,
	add_melt_quote_optimistic_locking,
	add_and_get_proofs,
	get_proofs_in_transaction,
	update_proofs,
	update_proofs_state,
	filter_proofs_by_unit,
	filter_proofs_by_state,
	get_balance,
	get_balance_by_state,
	increment_keyset_counter,
	keyset_counter_isolation,
	increment_derivation_counter,
	derivation_counters_are_independent,
	derivation_counter_is_atomic,
	add_and_get_transaction,
	update_transaction_status,
	same_proofs_in_different_sagas,
	list_transactions,
	filter_transactions_by_mint,
	remove_transaction,
	kvstore_write_and_read,
	kvstore_list,
	kvstore_update,
	kvstore_remove,
	kvstore_namespace_isolation,
	add_and_get_saga,
	update_saga_optimistic_locking,
	delete_saga,
	get_incomplete_sagas,
	reserve_proofs,
	release_proofs,
	release_proofs_preserves_spent,
	get_reserved_proofs,
	reserve_proofs_already_reserved,
	reserve_proofs_is_atomic,
);

/// One-shot error before an operation or after it committed (lost acknowledgement).
/// Yielding every operation also models an asynchronous remote KV store.
struct FaultStore {
	inner: Arc<dyn DynStore>,
	calls: Arc<AtomicUsize>,
	fail_at: usize,
	after: bool,
	migration_only: bool,
}

type StoreFuture<T> = Pin<Box<dyn Future<Output = Result<T, io::Error>> + Send + 'static>>;
impl FaultStore {
	fn wrap<T: Send + 'static>(
		&self, namespace: &str, key: &str, future: impl FnOnce() -> StoreFuture<T> + Send + 'static,
	) -> StoreFuture<T> {
		let tracked = !self.migration_only
			|| namespace == TRANSACTIONS_KEY
			|| key == "cdk_018_transaction_ids";
		let fail = tracked && self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_at;
		let after = self.after;
		Box::pin(async move {
			tokio::task::yield_now().await;
			if fail && !after {
				return Err(io::Error::new(io::ErrorKind::Other, "injected before operation"));
			}
			let result = future().await;
			if fail && after {
				return Err(io::Error::new(io::ErrorKind::Other, "injected lost acknowledgement"));
			}
			result
		})
	}
}
impl DynStore for FaultStore {
	fn read_async(&self, p: &str, s: &str, k: &str) -> StoreFuture<Vec<u8>> {
		let (inner, p, ns, key) = (self.inner.clone(), p.to_owned(), s.to_owned(), k.to_owned());
		self.wrap(s, k, move || inner.read_async(&p, &ns, &key))
	}
	fn write_async(&self, p: &str, s: &str, k: &str, value: Vec<u8>) -> StoreFuture<()> {
		let (inner, p, ns, key) = (self.inner.clone(), p.to_owned(), s.to_owned(), k.to_owned());
		self.wrap(s, k, move || inner.write_async(&p, &ns, &key, value))
	}
	fn remove_async(&self, p: &str, s: &str, k: &str, lazy: bool) -> StoreFuture<()> {
		let (inner, p, ns, key) = (self.inner.clone(), p.to_owned(), s.to_owned(), k.to_owned());
		self.wrap(s, k, move || inner.remove_async(&p, &ns, &key, lazy))
	}
	fn list_async(&self, p: &str, s: &str) -> StoreFuture<Vec<String>> {
		let (inner, p, ns) = (self.inner.clone(), p.to_owned(), s.to_owned());
		self.wrap(s, "", move || inner.list_async(&p, &ns))
	}
	fn list_paginated_async(
		&self, p: &str, s: &str, token: Option<PageToken>,
	) -> StoreFuture<PaginatedListResponse> {
		let (inner, p, ns) = (self.inner.clone(), p.to_owned(), s.to_owned());
		self.wrap(s, "", move || inner.list_paginated_async(&p, &ns, token))
	}
}

#[derive(serde::Deserialize)]
struct LegacyRecord {
	namespace: String,
	key: String,
	value: String,
}

fn legacy_records() -> Vec<LegacyRecord> {
	serde_json::from_str(include_str!("../../../tests/fixtures/cdk016-wallet.json")).unwrap()
}

async fn seed_legacy_store(store: &Arc<dyn DynStore>) {
	for record in legacy_records() {
		store
			.write_async(
				CASHU_PRIMARY_KEY,
				&record.namespace,
				&record.key,
				record.value.into_bytes(),
			)
			.await
			.unwrap();
	}
}

async fn verify_upgraded_store(store: &Arc<dyn DynStore>) {
	let db = CashuKvDatabase::new(store.clone()).await.unwrap();
	for record in legacy_records() {
		let key = if record.namespace == TRANSACTIONS_KEY {
			let tx: Transaction = serde_json::from_slice(record.value.as_bytes()).unwrap();
			assert_eq!(db.get_transaction(tx.id()).await.unwrap(), Some(tx.clone()));
			assert_eq!(tx.status, cdk::wallet::types::TransactionStatus::Completed);
			if record.key != tx.id().to_string() {
				assert!(db.get_transaction(record.key.parse().unwrap()).await.unwrap().is_none());
			}
			tx.id().to_string()
		} else {
			record.key
		};
		assert_eq!(
			store.read_async(CASHU_PRIMARY_KEY, &record.namespace, &key).await.unwrap(),
			record.value.as_bytes()
		);
	}
	assert_eq!(db.list_transactions(None, None, None).await.unwrap().len(), 2);
	assert_eq!(db.get_balance(None, None, Some(vec![State::Unspent])).await.unwrap(), 1);
	let proofs = db.get_proofs(None, None, None, None).await.unwrap();
	assert_eq!(proofs.len(), 4);
	assert!(proofs.iter().all(|proof| proof.derivation_index.is_none()));
	assert_eq!(db.get_mints().await.unwrap().len(), 1);
	let quote = db.get_mint_quotes().await.unwrap().remove(0);
	assert_eq!(quote.secret_key.as_ref().unwrap().to_secret_hex(), format!("{:064x}", 1));
	assert_eq!(quote.updated_at, 0);
	assert_eq!(quote.version, 0);
	let quote = db.get_melt_quote("melt-quote").await.unwrap().unwrap();
	assert_eq!(quote.payment_proof.as_deref(), Some("fixture-preimage"));
	assert_eq!(quote.version, 2);
	let sagas = db.get_incomplete_sagas().await.unwrap();
	assert_eq!(sagas.len(), 1);
	assert_eq!(sagas[0].version, 0);
	assert_eq!(db.get_reserved_proofs(&uuid::Uuid::from_u128(1)).await.unwrap().len(), 1);
	assert_eq!(
		db.increment_keyset_counter(&Id::from_str("009a1f293253e41e").unwrap(), 0).await.unwrap(),
		41
	);
}

#[tokio::test]
async fn database_written_by_cdk016_reopens_without_losing_records() {
	let (path, store) = migration_store();
	seed_legacy_store(&store).await;
	verify_upgraded_store(&store).await;
	drop(store);
	let reopened: Arc<dyn DynStore> = Arc::new(
		ldk_node::io::sqlite_store::SqliteStore::new(
			path.clone(),
			Some("orange.sqlite".into()),
			None,
		)
		.unwrap(),
	);
	verify_upgraded_store(&reopened).await;
	drop(reopened);
	std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn migration_recovers_from_every_io_failure_before_and_after_commit() {
	let (path, store) = migration_store();
	seed_legacy_store(&store).await;
	let calls = Arc::new(AtomicUsize::new(0));
	let wrapper = Arc::new(FaultStore {
		inner: store.clone(),
		calls: calls.clone(),
		fail_at: usize::MAX,
		after: false,
		migration_only: true,
	});
	drop(CashuKvDatabase::new(wrapper).await.unwrap());
	let total = calls.load(Ordering::SeqCst);
	assert!(total >= 8, "must cover copying, deleting and committing the migration marker");
	drop(store);
	std::fs::remove_dir_all(path).unwrap();
	for after in [false, true] {
		for fail_at in 1..=total {
			let (path, store) = migration_store();
			seed_legacy_store(&store).await;
			let wrapper = Arc::new(FaultStore {
				inner: store.clone(),
				calls: Arc::new(AtomicUsize::new(0)),
				fail_at,
				after,
				migration_only: true,
			});
			assert!(
				CashuKvDatabase::new(wrapper).await.is_err(),
				"failure {fail_at}, after={after}"
			);
			// Drop every SQLite handle: recover using only the durable state.
			drop(store);
			let reopened: Arc<dyn DynStore> = Arc::new(
				ldk_node::io::sqlite_store::SqliteStore::new(
					path.clone(),
					Some("orange.sqlite".into()),
					None,
				)
				.unwrap(),
			);
			verify_upgraded_store(&reopened).await;
			verify_upgraded_store(&reopened).await;
			drop(reopened);
			std::fs::remove_dir_all(path).unwrap();
		}
	}
}

#[tokio::test]
async fn derivation_counter_errors_never_reset_or_reuse_a_committed_index() {
	for after in [false, true] {
		for fail_at in [1, 2] {
			let (path, store) = migration_store();
			let db = CashuKvDatabase::new(store.clone()).await.unwrap();
			assert_eq!(db.increment_derivation_counter("nut20_quote", 7).await.unwrap(), 7);
			drop(db);
			let calls = Arc::new(AtomicUsize::new(0));
			let wrapper = Arc::new(FaultStore {
				inner: store.clone(),
				calls: calls.clone(),
				fail_at: usize::MAX,
				after,
				migration_only: false,
			});
			let mut db = CashuKvDatabase::new(wrapper).await.unwrap();
			db.store = Arc::new(FaultStore {
				inner: store.clone(),
				calls: calls.clone(),
				fail_at,
				after,
				migration_only: false,
			});
			calls.store(0, Ordering::SeqCst);
			assert!(db.increment_derivation_counter("nut20_quote", 1).await.is_err());
			drop(db);
			let db = CashuKvDatabase::new(store.clone()).await.unwrap();
			let committed = if after && fail_at == 2 { 8 } else { 7 };
			assert_eq!(db.increment_derivation_counter("nut20_quote", 0).await.unwrap(), committed);
			assert_eq!(
				db.increment_derivation_counter("nut20_quote", 1).await.unwrap(),
				committed + 1
			);
			drop(db);
			drop(store);
			std::fs::remove_dir_all(path).unwrap();
		}
	}
}

#[tokio::test]
async fn reservation_write_failure_never_leaves_a_partial_proof_set_after_restart() {
	for after in [false, true] {
		let (path, store) = migration_store();
		let mut db = CashuKvDatabase::new(store.clone()).await.unwrap();
		let proofs = vec![proof_info("fault-proof-1", 1), proof_info("fault-proof-2", 2)];
		db.update_proofs(proofs.clone(), vec![]).await.unwrap();
		db.store = Arc::new(FaultStore {
			inner: store.clone(),
			calls: Arc::new(AtomicUsize::new(0)),
			fail_at: 1,
			after,
			migration_only: false,
		});
		let operation = uuid::Uuid::from_u128(1);
		assert!(db.reserve_proofs(proofs.iter().map(|p| p.y).collect(), &operation).await.is_err());
		// A lost acknowledgement must not leave this still-running adapter
		// presenting reserved proofs as spendable from its stale cache.
		assert_eq!(
			db.get_reserved_proofs(&operation).await.unwrap().len(),
			if after { 2 } else { 0 }
		);
		if after {
			assert!(db.reserve_proofs(vec![proofs[0].y], &uuid::Uuid::from_u128(2)).await.is_err());
		}
		drop(db);
		let db = CashuKvDatabase::new(store.clone()).await.unwrap();
		assert_eq!(
			db.get_reserved_proofs(&operation).await.unwrap().len(),
			if after { 2 } else { 0 }
		);
		assert_eq!(
			db.get_balance(None, None, Some(vec![State::Unspent])).await.unwrap(),
			if after { 0 } else { 3 }
		);
		drop(db);
		drop(store);
		std::fs::remove_dir_all(path).unwrap();
	}
}

#[tokio::test]
async fn upgraded_legacy_reservation_cannot_be_stolen_by_a_new_operation() {
	let (path, store) = migration_store();
	seed_legacy_store(&store).await;
	let db = CashuKvDatabase::new(store.clone()).await.unwrap();
	let owner = uuid::Uuid::from_u128(1);
	let reserved = db.get_reserved_proofs(&owner).await.unwrap();
	assert!(db.reserve_proofs(vec![reserved[0].y], &uuid::Uuid::from_u128(2)).await.is_err());
	assert_eq!(db.get_reserved_proofs(&owner).await.unwrap(), reserved);
	drop(db);
	drop(store);
	std::fs::remove_dir_all(path).unwrap();
}

#[cfg(feature = "_cashu-tests")]
#[path = "cashu_store_live_upgrade_tests.rs"]
mod live;

#[tokio::test]
async fn legacy_release_retries_safely_after_every_storage_failure() {
	for after in [false, true] {
		// Reservation-index read, proof snapshot write, reservation-index delete.
		for fail_at in 1..=3 {
			let (path, store) = migration_store();
			seed_legacy_store(&store).await;
			let mut db = CashuKvDatabase::new(store.clone()).await.unwrap();
			db.store = Arc::new(FaultStore {
				inner: store.clone(),
				calls: Arc::new(AtomicUsize::new(0)),
				fail_at,
				after,
				migration_only: false,
			});
			let operation = uuid::Uuid::from_u128(1);
			assert!(db.release_proofs(&operation).await.is_err());
			drop(db);
			let db = CashuKvDatabase::new(store.clone()).await.unwrap();
			db.release_proofs(&operation).await.unwrap();
			db.release_proofs(&operation).await.unwrap();
			assert_eq!(db.get_balance(None, None, Some(vec![State::Unspent])).await.unwrap(), 3);
			assert_eq!(db.get_balance(None, None, Some(vec![State::Pending])).await.unwrap(), 4);
			assert_eq!(db.get_balance(None, None, Some(vec![State::Spent])).await.unwrap(), 8);
			assert!(db.get_reserved_proofs(&operation).await.unwrap().is_empty());
			drop(db);
			drop(store);
			std::fs::remove_dir_all(path).unwrap();
		}
	}
}

#[tokio::test]
async fn concurrent_quote_and_saga_updates_have_one_winner() {
	let (path, store) = migration_store();
	seed_legacy_store(&store).await;
	let db = CashuKvDatabase::new(store.clone()).await.unwrap();
	let quote = db.get_mint_quote("mint-quote").await.unwrap().unwrap();
	let (a, b) = tokio::join!(db.add_mint_quote(quote.clone()), db.add_mint_quote(quote));
	assert_eq!([a.is_ok(), b.is_ok()].into_iter().filter(|ok| *ok).count(), 1);
	assert_eq!(db.get_mint_quote("mint-quote").await.unwrap().unwrap().version, 1);
	let mut quote = db.get_melt_quote("melt-quote").await.unwrap().unwrap();
	quote.payment_proof = None; // A later status response may omit the proof.
	let (a, b) = tokio::join!(db.add_melt_quote(quote.clone()), db.add_melt_quote(quote));
	assert_eq!([a.is_ok(), b.is_ok()].into_iter().filter(|ok| *ok).count(), 1);
	let quote = db.get_melt_quote("melt-quote").await.unwrap().unwrap();
	assert_eq!(quote.version, 3);
	assert_eq!(quote.payment_proof.as_deref(), Some("fixture-preimage"));
	let id = uuid::Uuid::from_u128(1);
	let mut saga = db.get_saga(&id).await.unwrap().unwrap();
	saga.version += 1;
	let (a, b) = tokio::join!(db.update_saga(saga.clone()), db.update_saga(saga));
	assert_ne!(a.unwrap(), b.unwrap());
	assert_eq!(db.get_saga(&id).await.unwrap().unwrap().version, 1);
	drop(db);
	drop(store);
	std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn legacy_keyset_counter_does_not_reset_on_read_error_or_overflow() {
	let (path, store) = migration_store();
	seed_legacy_store(&store).await;
	let mut db = CashuKvDatabase::new(store.clone()).await.unwrap();
	let keyset = Id::from_str("009a1f293253e41e").unwrap();
	db.store = Arc::new(FaultStore {
		inner: store.clone(),
		calls: Arc::new(AtomicUsize::new(0)),
		fail_at: 1,
		after: false,
		migration_only: false,
	});
	assert!(db.increment_keyset_counter(&keyset, 1).await.is_err());
	assert_eq!(db.increment_keyset_counter(&keyset, 0).await.unwrap(), 41);
	assert!(db.increment_keyset_counter(&keyset, u32::MAX).await.is_err());
	assert_eq!(db.increment_keyset_counter(&keyset, 0).await.unwrap(), 41);
	drop(db);
	drop(store);
	std::fs::remove_dir_all(path).unwrap();
}
