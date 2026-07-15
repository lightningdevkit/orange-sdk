//! Object-safe wrapper around ldk-node's async `KVStore` trait.
//!
//! `lightning`'s `KVStore` returns `impl Future` from its methods, which makes the trait not
//! object-safe — orange-sdk can't share a backend across components as `Arc<dyn KVStore>`.
//!
//! This module defines `DynStore`, an object-safe trait covering the kv methods (returning
//! boxed futures), with a blanket impl over any concrete type that implements `KVStore`. The
//! whole crate stores backends as `Arc<dyn DynStore>`; conversion to a value ldk-node accepts
//! happens at the call site through a thin newtype that delegates back to `DynStore`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ldk_node::lightning::io;
use ldk_node::lightning::util::persist::KVStore;
use tokio::task::JoinSet;

/// Matches the connection capacity used by the VSS HTTP client. Keeping the
/// limit here also prevents large wallets from spawning one task per record.
const MAX_CONCURRENT_READS: usize = 10;

/// Object-safe view of a `KVStore` backend. Async methods return boxed futures so the trait
/// can be used through `dyn`.
pub(crate) trait DynStore: Send + Sync + 'static {
	fn read_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, io::Error>> + Send + 'static>>;

	fn write_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send + 'static>>;

	fn remove_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send + 'static>>;

	fn list_async(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<String>, io::Error>> + Send + 'static>>;
}

impl<T> DynStore for T
where
	T: KVStore + Send + Sync + 'static,
{
	fn read_async(
		&self, p: &str, s: &str, k: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, io::Error>> + Send + 'static>> {
		Box::pin(<T as KVStore>::read(self, p, s, k))
	}

	fn write_async(
		&self, p: &str, s: &str, k: &str, buf: Vec<u8>,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send + 'static>> {
		Box::pin(<T as KVStore>::write(self, p, s, k, buf))
	}

	fn remove_async(
		&self, p: &str, s: &str, k: &str, lazy: bool,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send + 'static>> {
		Box::pin(<T as KVStore>::remove(self, p, s, k, lazy))
	}

	fn list_async(
		&self, p: &str, s: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<String>, io::Error>> + Send + 'static>> {
		Box::pin(<T as KVStore>::list(self, p, s))
	}
}

// Make `dyn DynStore` itself implement `KVStore` so the same handle orange-sdk shares
// internally can be handed to ldk-node's `build_with_store`. We forward to the boxed-future
// variants on the trait. Via lightning's `Deref` blanket impl for `KVStore`, this gives us
// `Arc<dyn DynStore>: KVStore` too.
impl KVStore for dyn DynStore {
	fn read(
		&self, p: &str, s: &str, k: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + Send + 'static {
		self.read_async(p, s, k)
	}
	fn write(
		&self, p: &str, s: &str, k: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
		self.write_async(p, s, k, buf)
	}
	fn remove(
		&self, p: &str, s: &str, k: &str, lazy: bool,
	) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
		self.remove_async(p, s, k, lazy)
	}
	fn list(
		&self, p: &str, s: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + Send + 'static {
		self.list_async(p, s)
	}
}

/// Cloneable handle wrapping `Arc<dyn DynStore>` that satisfies ldk-node's
/// `KVStore + Send + Sync + 'static` bound on `build_with_store`. The trait impl just
/// forwards to the underlying `dyn DynStore`.
#[derive(Clone)]
pub(crate) struct LdkNodeStore(pub(crate) Arc<dyn DynStore>);

impl KVStore for LdkNodeStore {
	fn read(
		&self, p: &str, s: &str, k: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + Send + 'static {
		self.0.read_async(p, s, k)
	}
	fn write(
		&self, p: &str, s: &str, k: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
		self.0.write_async(p, s, k, buf)
	}
	fn remove(
		&self, p: &str, s: &str, k: &str, lazy: bool,
	) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
		self.0.remove_async(p, s, k, lazy)
	}
	fn list(
		&self, p: &str, s: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + Send + 'static {
		self.0.list_async(p, s)
	}
}

/// Reads a set of keys concurrently while preserving the input order.
///
/// Storage formats expose record collections as a list followed by individual
/// reads. For a remote store, performing those reads serially adds one network
/// round trip per record. This helper bounds that fan-out to the VSS connection
/// pool size and retains the same fail-fast I/O behavior as a serial loop.
pub(crate) async fn read_keys_bounded(
	store: Arc<dyn DynStore>, primary_namespace: &str, secondary_namespace: &str, keys: Vec<String>,
) -> Result<Vec<(String, Vec<u8>)>, io::Error> {
	if keys.is_empty() {
		return Ok(Vec::new());
	}

	type ReadResult = (usize, String, Result<Vec<u8>, io::Error>);

	let primary_namespace = primary_namespace.to_owned();
	let secondary_namespace = secondary_namespace.to_owned();
	let mut pending = keys.into_iter().enumerate();
	let mut reads: JoinSet<ReadResult> = JoinSet::new();
	let mut results = Vec::with_capacity(pending.len());
	results.resize_with(pending.len(), || None);

	let spawn_read = |reads: &mut JoinSet<ReadResult>, index, key: String| {
		let store = Arc::clone(&store);
		let primary_namespace = primary_namespace.clone();
		let secondary_namespace = secondary_namespace.clone();
		reads.spawn(async move {
			let data = store.read_async(&primary_namespace, &secondary_namespace, &key).await;
			(index, key, data)
		});
	};

	for _ in 0..MAX_CONCURRENT_READS {
		if let Some((index, key)) = pending.next() {
			spawn_read(&mut reads, index, key);
		} else {
			break;
		}
	}

	while let Some(result) = reads.join_next().await {
		let (index, key, data) = result.map_err(|e| {
			io::Error::new(io::ErrorKind::Other, format!("store read task failed: {e}"))
		})?;
		results[index] = Some((key, data?));

		if let Some((index, key)) = pending.next() {
			spawn_read(&mut reads, index, key);
		}
	}

	Ok(results
		.into_iter()
		.map(|result| result.expect("every bounded store read must complete"))
		.collect())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::future::ready;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use tokio::sync::{Semaphore, mpsc};

	struct ControlledStore {
		entered: mpsc::UnboundedSender<String>,
		release: Arc<Semaphore>,
		active: Arc<AtomicUsize>,
		max_active: Arc<AtomicUsize>,
		fail_key: Option<String>,
	}

	impl KVStore for ControlledStore {
		fn read(
			&self, _primary_namespace: &str, _secondary_namespace: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + Send + 'static {
			let key = key.to_owned();
			let entered = self.entered.clone();
			let release = Arc::clone(&self.release);
			let active = Arc::clone(&self.active);
			let max_active = Arc::clone(&self.max_active);
			let should_fail = self.fail_key.as_ref() == Some(&key);
			async move {
				let active_count = active.fetch_add(1, Ordering::SeqCst) + 1;
				max_active.fetch_max(active_count, Ordering::SeqCst);
				let _ = entered.send(key.clone());
				let _permit =
					release.acquire_owned().await.expect("test semaphore must remain open");
				active.fetch_sub(1, Ordering::SeqCst);

				if should_fail {
					Err(io::Error::new(io::ErrorKind::InvalidData, "controlled read failure"))
				} else {
					Ok(key.into_bytes())
				}
			}
		}

		fn write(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
			ready(Ok(()))
		}

		fn remove(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
			ready(Ok(()))
		}

		fn list(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + Send + 'static {
			ready(Ok(Vec::new()))
		}
	}

	#[tokio::test]
	async fn bounded_reads_limit_concurrency_and_preserve_order() {
		let (entered, mut entered_rx) = mpsc::unbounded_channel();
		let release = Arc::new(Semaphore::new(0));
		let max_active = Arc::new(AtomicUsize::new(0));
		let store: Arc<dyn DynStore> = Arc::new(ControlledStore {
			entered,
			release: Arc::clone(&release),
			active: Arc::new(AtomicUsize::new(0)),
			max_active: Arc::clone(&max_active),
			fail_key: None,
		});
		let keys: Vec<_> = (0..12).rev().map(|index| format!("key-{index:02}")).collect();

		let reads = tokio::spawn(read_keys_bounded(store, "primary", "secondary", keys.clone()));
		for _ in 0..MAX_CONCURRENT_READS {
			entered_rx.recv().await.expect("bounded read must start");
		}
		assert_eq!(max_active.load(Ordering::SeqCst), MAX_CONCURRENT_READS);
		assert!(entered_rx.try_recv().is_err());

		release.add_permits(keys.len());
		let records = reads.await.unwrap().unwrap();
		let result_keys: Vec<_> = records.iter().map(|(key, _)| key.clone()).collect();
		assert_eq!(result_keys, keys);
		assert!(records.iter().all(|(key, data)| key.as_bytes() == data));
		assert_eq!(max_active.load(Ordering::SeqCst), MAX_CONCURRENT_READS);
	}

	#[tokio::test]
	async fn bounded_reads_propagate_store_errors() {
		let (entered, _entered_rx) = mpsc::unbounded_channel();
		let store: Arc<dyn DynStore> = Arc::new(ControlledStore {
			entered,
			release: Arc::new(Semaphore::new(2)),
			active: Arc::new(AtomicUsize::new(0)),
			max_active: Arc::new(AtomicUsize::new(0)),
			fail_key: Some("bad".to_owned()),
		});

		let error = read_keys_bounded(
			store,
			"primary",
			"secondary",
			vec!["good".to_owned(), "bad".to_owned()],
		)
		.await
		.unwrap_err();
		assert_eq!(error.kind(), io::ErrorKind::InvalidData);
	}
}
