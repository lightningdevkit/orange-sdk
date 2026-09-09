//! Controlled in-memory storage for persistence and cache tests.

use crate::dyn_store::DynStore;
use crate::event::EventQueue;
use crate::logging::{Logger, LoggerType};
use crate::runtime::Runtime;
use crate::store::TxMetadataStore;
use ldk_node::io::sqlite_store::SqliteStore;
use ldk_node::lightning::io;
use ldk_node::lightning::util::persist::{
	KVStore, PageToken, PaginatedKVStore, PaginatedListResponse,
};
use std::collections::HashMap;
use std::future::ready;
use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

type Key = (String, String, String);

#[derive(Clone, Default)]
pub(crate) struct TestStore {
	data: Arc<Mutex<HashMap<Key, Vec<u8>>>>,
	pub reads: Arc<AtomicUsize>,
	pub lists: Arc<AtomicUsize>,
	pub writes: Arc<AtomicUsize>,
	pub fail_next_read: Arc<AtomicBool>,
	pub fail_next_write: Arc<AtomicBool>,
	gate: Arc<Mutex<Option<mpsc::UnboundedSender<oneshot::Sender<()>>>>>,
}

impl TestStore {
	pub fn shared(&self) -> Arc<dyn DynStore> {
		Arc::new(self.clone())
	}

	pub fn control_writes(&self) -> mpsc::UnboundedReceiver<oneshot::Sender<()>> {
		let (tx, rx) = mpsc::unbounded_channel();
		*self.gate.lock().unwrap() = Some(tx);
		rx
	}

	async fn mutate(&self, key: Key, value: Option<Vec<u8>>) -> Result<(), io::Error> {
		self.writes.fetch_add(1, Ordering::SeqCst);
		let gate = self.gate.lock().unwrap().clone();
		if let Some(gate) = gate {
			let (tx, rx) = oneshot::channel();
			gate.send(tx).unwrap();
			rx.await.unwrap();
		}
		if self.fail_next_write.swap(false, Ordering::SeqCst) {
			return Err(io::Error::new(io::ErrorKind::Other, "injected write failure"));
		}
		let mut data = self.data.lock().unwrap();
		if let Some(value) = value {
			data.insert(key, value);
		} else {
			data.remove(&key);
		}
		Ok(())
	}
}

impl KVStore for TestStore {
	fn read(
		&self, p: &str, s: &str, k: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + Send + 'static {
		self.reads.fetch_add(1, Ordering::SeqCst);
		let result = if self.fail_next_read.swap(false, Ordering::SeqCst) {
			Err(io::Error::new(io::ErrorKind::Other, "injected read failure"))
		} else {
			self.data
				.lock()
				.unwrap()
				.get(&(p.into(), s.into(), k.into()))
				.cloned()
				.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing key"))
		};
		ready(result)
	}

	fn write(
		&self, p: &str, s: &str, k: &str, value: Vec<u8>,
	) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
		let this = self.clone();
		let key = (p.into(), s.into(), k.into());
		async move { this.mutate(key, Some(value)).await }
	}

	fn remove(
		&self, p: &str, s: &str, k: &str, _: bool,
	) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
		let this = self.clone();
		let key = (p.into(), s.into(), k.into());
		async move { this.mutate(key, None).await }
	}

	fn list(
		&self, p: &str, s: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + Send + 'static {
		self.lists.fetch_add(1, Ordering::SeqCst);
		let mut keys: Vec<_> = self
			.data
			.lock()
			.unwrap()
			.keys()
			.filter(|(primary, secondary, _)| primary == p && secondary == s)
			.map(|(_, _, key)| key.clone())
			.collect();
		keys.sort();
		ready(Ok(keys))
	}
}

impl PaginatedKVStore for TestStore {
	fn list_paginated(
		&self, p: &str, s: &str, _: Option<PageToken>,
	) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + Send + 'static {
		let keys = self.list(p, s);
		async move { Ok(PaginatedListResponse { keys: keys.await?, next_page_token: None }) }
	}
}

pub(crate) fn test_logger() -> Arc<Logger> {
	Arc::new(Logger::new(&LoggerType::LogFacade).unwrap())
}

pub(crate) fn test_runtime() -> Arc<Runtime> {
	Arc::new(Runtime::new(test_logger()).unwrap())
}

/// Restores the queue persisted in `store`, as `Wallet` does at startup.
pub(crate) async fn test_event_queue(
	store: &TestStore, tx_metadata: TxMetadataStore, runtime: Arc<Runtime>,
) -> Arc<EventQueue> {
	let events = EventQueue::load_events(store).await.unwrap();
	Arc::new(EventQueue::new(store.shared(), events, tx_metadata, test_logger(), runtime))
}

/// A fresh SQLite store in a unique temporary directory, for tests that need ldk-node's
/// real storage format.
pub(crate) fn temp_sqlite_store() -> (PathBuf, Arc<dyn DynStore>) {
	let path = std::env::temp_dir().join(format!(
		"orange-sdk-test-{}",
		SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
	));
	let store = open_sqlite_store(&path);
	(path, store)
}

/// Opens the SQLite store at `path`, for example to simulate a restart.
pub(crate) fn open_sqlite_store(path: &Path) -> Arc<dyn DynStore> {
	Arc::new(
		SqliteStore::new(path.to_path_buf(), Some("orange.sqlite".to_string()), None)
			.expect("sqlite store"),
	)
}
