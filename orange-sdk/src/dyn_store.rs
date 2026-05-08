//! Object-safe wrapper around ldk-node's `KVStore` + `KVStoreSync` traits.
//!
//! `lightning`'s `KVStore` returns `impl Future` from its methods, which makes the trait not
//! object-safe — orange-sdk can't share a backend across components as `Arc<dyn KVStore>`. The
//! supertrait `SyncAndAsyncKVStore` that ldk-node exposes inherits the same problem, and even
//! if it didn't, no `Deref` blanket impl exists for `KVStoreSync`, so `Arc<...>` doesn't satisfy
//! it on its own.
//!
//! This module defines `DynStore`, an object-safe trait covering both sync and async kv
//! methods (async ones return boxed futures), with a blanket impl over any concrete type that
//! implements `KVStore + KVStoreSync`. The whole crate stores backends as
//! `Arc<dyn DynStore>`; conversion to a value ldk-node accepts happens at the call site
//! through a thin newtype that delegates both traits back to `DynStore`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ldk_node::lightning::io;
use ldk_node::lightning::util::persist::{KVStore, KVStoreSync};

/// Object-safe view of a `KVStore + KVStoreSync` backend. Async methods return boxed
/// futures so the trait can be used through `dyn`.
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

	fn read_sync(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Result<Vec<u8>, io::Error>;

	fn write_sync(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Result<(), io::Error>;

	fn remove_sync(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Result<(), io::Error>;

	fn list_sync(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Result<Vec<String>, io::Error>;
}

impl<T> DynStore for T
where
	T: KVStore + KVStoreSync + Send + Sync + 'static,
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

	fn read_sync(&self, p: &str, s: &str, k: &str) -> Result<Vec<u8>, io::Error> {
		<T as KVStoreSync>::read(self, p, s, k)
	}

	fn write_sync(&self, p: &str, s: &str, k: &str, buf: Vec<u8>) -> Result<(), io::Error> {
		<T as KVStoreSync>::write(self, p, s, k, buf)
	}

	fn remove_sync(&self, p: &str, s: &str, k: &str, lazy: bool) -> Result<(), io::Error> {
		<T as KVStoreSync>::remove(self, p, s, k, lazy)
	}

	fn list_sync(&self, p: &str, s: &str) -> Result<Vec<String>, io::Error> {
		<T as KVStoreSync>::list(self, p, s)
	}
}

// Make `Arc<dyn DynStore>` itself implement `KVStore` + `KVStoreSync` so the same handle
// orange-sdk shares internally can be handed to ldk-node's `build_with_store`. We give it
// `KVStore::read` etc. by forwarding to the boxed-future variants on the trait, and the
// sync trait by forwarding to the sync variants.
//
// Note: we impl on `dyn DynStore` (which is local), not `Arc` directly — that gives us
// `&dyn DynStore: KVStore + KVStoreSync` and, via lightning's `Deref` blanket impl for
// `KVStore`, `Arc<dyn DynStore>: KVStore`. For `KVStoreSync` (no `Deref` blanket exists)
// callers wrap the `Arc` in `LdkNodeStore` below before handing it to ldk-node.
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

impl KVStoreSync for dyn DynStore {
	fn read(&self, p: &str, s: &str, k: &str) -> Result<Vec<u8>, io::Error> {
		self.read_sync(p, s, k)
	}
	fn write(&self, p: &str, s: &str, k: &str, buf: Vec<u8>) -> Result<(), io::Error> {
		self.write_sync(p, s, k, buf)
	}
	fn remove(&self, p: &str, s: &str, k: &str, lazy: bool) -> Result<(), io::Error> {
		self.remove_sync(p, s, k, lazy)
	}
	fn list(&self, p: &str, s: &str) -> Result<Vec<String>, io::Error> {
		self.list_sync(p, s)
	}
}

/// Cloneable handle wrapping `Arc<dyn DynStore>` that satisfies ldk-node's
/// `SyncAndAsyncKVStore + Send + Sync + 'static` bound on `build_with_store`. Both trait
/// impls just forward to the underlying `dyn DynStore`.
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

impl KVStoreSync for LdkNodeStore {
	fn read(&self, p: &str, s: &str, k: &str) -> Result<Vec<u8>, io::Error> {
		self.0.read_sync(p, s, k)
	}
	fn write(&self, p: &str, s: &str, k: &str, buf: Vec<u8>) -> Result<(), io::Error> {
		self.0.write_sync(p, s, k, buf)
	}
	fn remove(&self, p: &str, s: &str, k: &str, lazy: bool) -> Result<(), io::Error> {
		self.0.remove_sync(p, s, k, lazy)
	}
	fn list(&self, p: &str, s: &str) -> Result<Vec<String>, io::Error> {
		self.0.list_sync(p, s)
	}
}
