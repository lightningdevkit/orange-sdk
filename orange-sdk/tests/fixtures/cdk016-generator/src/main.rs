//! Run against the original adapter, extracted from Orange commit a3aa1c59.
#![allow(dead_code)]
mod dyn_store {
	include!(env!("ORANGE_OLD_DYN_STORE"));
}
mod trusted_wallet {
	#[derive(Debug)]
	pub enum TrustedError {
		IOError(ldk_node::lightning::io::Error),
	}
	pub mod cashu_store {
		include!(env!("ORANGE_OLD_CASHU_STORE"));
	}
}
use cdk::{
	Amount,
	cdk_database::WalletDatabase,
	nuts::*,
	secret::Secret,
	types::ProofInfo,
	wallet::{MeltQuote, MintQuote, types::*},
};
use ldk_node::lightning::util::persist::KVStore;
use std::{str::FromStr, sync::Arc};
use trusted_wallet::cashu_store::CashuKvDatabase;
#[tokio::main]
async fn main() {
	let path = std::env::args().nth(1).expect("fresh SQLite directory");
	let store: Arc<dyn dyn_store::DynStore> = Arc::new(
		ldk_node::io::sqlite_store::SqliteStore::new(
			path.into(),
			Some("orange.sqlite".into()),
			None,
		)
		.unwrap(),
	);
	let db = CashuKvDatabase::new(store.clone()).await.unwrap();
	if let Some(url) = std::env::args().nth(2) {
		// Match Orange's Seed64 derivation so the new wallet can use production init.
		use ldk_node::bitcoin::hashes::{Hash, sha256};
		let hash = sha256::Hash::hash(&[7; 64]).to_byte_array();
		let mut seed = [0; 64];
		seed[..32].copy_from_slice(&hash);
		seed[32..].copy_from_slice(&hash);
		let wallet =
			cdk::wallet::Wallet::new(&url, CurrencyUnit::Sat, Arc::new(db), seed, None).unwrap();
		let quote =
			wallet.mint_quote(PaymentMethod::BOLT11, Some(64.into()), None, None).await.unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(30), async {
			loop {
				if wallet.check_mint_quote_status(&quote.id).await.unwrap().state
					== MintQuoteState::Paid
				{
					break;
				}
				tokio::time::sleep(std::time::Duration::from_millis(50)).await;
			}
		})
		.await
		.unwrap();
		wallet.mint(&quote.id, Default::default(), None).await.unwrap();
		assert_eq!(wallet.total_balance().await.unwrap(), Amount::from(64));
		let invoice = std::env::args().nth(3).expect("external fake invoice");
		let quote = wallet.melt_quote(PaymentMethod::BOLT11, invoice, None, None).await.unwrap();
		let prepared = wallet.prepare_melt(&quote.id, Default::default()).await.unwrap();
		println!("Prepared old-version melt {}", prepared.operation_id());
		if std::env::args().nth(4).as_deref() == Some("submit") {
			// The test server holds the response after processing the payment. The
			// parent kills this process there, bypassing wallet compensation/Drop.
			let result = prepared.confirm().await;
			panic!("test server unexpectedly delivered the melt response: {result:?}");
		}
		// A process exit bypasses any Drop cleanup, modelling a crash after reservation.
		std::process::exit(0);
	}
	let mint = cdk::mint_url::MintUrl::from_str("https://mint.example.com").unwrap();
	let keyset = Id::from_str("009a1f293253e41e").unwrap();
	db.add_mint(mint.clone(), Some(MintInfo::default())).await.unwrap();
	db.add_mint_keysets(
		mint.clone(),
		vec![KeySetInfo {
			id: keyset,
			unit: CurrencyUnit::Sat,
			active: true,
			input_fee_ppk: 100,
			final_expiry: None,
		}],
	)
	.await
	.unwrap();
	db.increment_keyset_counter(&keyset, 41).await.unwrap();
	let mut proofs = Vec::new();
	for (i, state) in
		[State::Unspent, State::Reserved, State::Pending, State::Spent].into_iter().enumerate()
	{
		let proof = Proof::new(
			Amount::from(1u64 << i),
			keyset,
			Secret::new(format!("fixture-only-secret-{i}")),
			PublicKey::from_str(
				"024369d2d22a80ecf78f3937da9d5f30c1b9f74f0c32684d583cca0fa6a61cdcfc",
			)
			.unwrap(),
		);
		proofs.push(ProofInfo::new(proof, mint.clone(), state, CurrencyUnit::Sat).unwrap());
	}
	db.update_proofs(proofs.clone(), vec![]).await.unwrap();
	let operation = uuid::Uuid::from_u128(1);
	db.reserve_proofs(vec![proofs[1].y], &operation).await.unwrap();
	for (i, saga_id) in [Some(operation), None].into_iter().enumerate() {
		db.add_transaction(Transaction {
			mint_url: mint.clone(),
			direction: TransactionDirection::Outgoing,
			amount: 10.into(),
			fee: 1.into(),
			unit: CurrencyUnit::Sat,
			ys: vec![proofs[i].y],
			timestamp: 42,
			memo: Some("fixture".into()),
			metadata: Default::default(),
			quote_id: Some(format!("quote-{i}")),
			payment_request: Some("fixture-invoice".into()),
			payment_proof: Some("fixture-preimage".into()),
			payment_method: Some(PaymentMethod::BOLT11),
			saga_id,
		})
		.await
		.unwrap();
	}
	let mut quote = MintQuote::new(
		"mint-quote".into(),
		mint.clone(),
		PaymentMethod::BOLT11,
		Some(10.into()),
		CurrencyUnit::Sat,
		"fixture-invoice".into(),
		123,
		Some(
			SecretKey::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
				.unwrap(),
		),
	);
	quote.amount_paid = 10.into();
	quote.state = MintQuoteState::Paid;
	db.add_mint_quote(quote).await.unwrap();
	db.add_melt_quote(MeltQuote {
		id: "melt-quote".into(),
		mint_url: Some(mint.clone()),
		unit: CurrencyUnit::Sat,
		amount: 10.into(),
		request: "fixture-invoice".into(),
		fee_reserve: 1.into(),
		state: MeltQuoteState::Pending,
		expiry: 123,
		payment_preimage: Some("fixture-preimage".into()),
		payment_method: PaymentMethod::BOLT11,
		used_by_operation: Some(operation.to_string()),
		version: 2,
	})
	.await
	.unwrap();
	let mut saga = WalletSaga::new(
		operation,
		WalletSagaState::Melt(MeltSagaState::PaymentPending),
		10.into(),
		mint,
		CurrencyUnit::Sat,
		OperationData::Melt(MeltOperationData {
			quote_id: "melt-quote".into(),
			amount: 10.into(),
			fee_reserve: 1.into(),
			counter_start: Some(40),
			counter_end: Some(41),
			change_amount: None,
			change_blinded_messages: None,
		}),
	);
	saga.created_at = 42;
	saga.updated_at = 43;
	db.add_saga(saga).await.unwrap();
	let mut records = Vec::new();
	for namespace in [
		"mints",
		"mint_keysets",
		"keysets_table",
		"keyset_u32_mapping",
		"keyset_counters",
		"proofs",
		"transactions",
		"mint_quotes",
		"melt_quotes",
		"sagas",
		"proof_reservations",
	] {
		let mut keys = KVStore::list(store.as_ref(), "cashu_wallet", namespace).await.unwrap();
		keys.sort();
		for key in keys {
			let value =
				KVStore::read(store.as_ref(), "cashu_wallet", namespace, &key).await.unwrap();
			records.push(serde_json::json!({"namespace": namespace, "key": key, "value": String::from_utf8(value).unwrap()}));
		}
	}
	println!("{}", serde_json::to_string_pretty(&records).unwrap());
}
