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
