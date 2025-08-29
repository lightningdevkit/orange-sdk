use std::collections::HashMap;
use std::fmt::Debug;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use cdk::cdk_database::WalletDatabase;
use ldk_node::lightning::io;
use ldk_node::lightning::util::persist::KVStore;

use cdk::mint_url::MintUrl;
use cdk::nuts::{
	CurrencyUnit, Id, KeySet, KeySetInfo, Keys, MintInfo, PublicKey, SpendingConditions, State,
};
use cdk::types::ProofInfo;
use cdk::util::hex;
use cdk::wallet::{
	MeltQuote, MintQuote,
	types::{Transaction, TransactionDirection, TransactionId},
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

// Constants for organizing data in the KV store
const CASHU_PRIMARY_KEY: &str = "cashu_wallet";

// Secondary keys for different data types
const MINTS_KEY: &str = "mints";
const MINT_KEYSETS_KEY: &str = "mint_keysets";
const MINT_QUOTES_KEY: &str = "mint_quotes";
const MELT_QUOTES_KEY: &str = "melt_quotes";
const KEYS_KEY: &str = "keys";
const PROOFS_KEY: &str = "proofs";
const KEYSET_COUNTERS_KEY: &str = "keyset_counters";
const TRANSACTIONS_KEY: &str = "transactions";
const KEYSETS_TABLE_KEY: &str = "keysets_table";
const KEYSET_U32_MAPPING_KEY: &str = "keyset_u32_mapping";

/// Error type for database operations
#[derive(Debug)]
pub enum DatabaseError {
	/// Serialization error with details
	Serialization(String),
	/// I/O error from the underlying store
	Io(io::Error),
	/// Data not found in storage
	NotFound,
	/// Invalid data format encountered
	InvalidFormat,
	/// Duplicate entry error
	Duplicate,
}

impl std::fmt::Display for DatabaseError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			DatabaseError::Serialization(msg) => write!(f, "Serialization error: {}", msg),
			DatabaseError::Io(err) => write!(f, "IO error: {}", err),
			DatabaseError::NotFound => write!(f, "Data not found"),
			DatabaseError::InvalidFormat => write!(f, "Invalid data format"),
			DatabaseError::Duplicate => write!(f, "Duplicate entry"),
		}
	}
}

impl std::error::Error for DatabaseError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			DatabaseError::Io(err) => Some(err),
			_ => None,
		}
	}
}

impl From<io::Error> for DatabaseError {
	fn from(err: io::Error) -> Self {
		DatabaseError::Io(err)
	}
}

impl From<DatabaseError> for io::Error {
	fn from(err: DatabaseError) -> Self {
		match err {
			DatabaseError::Io(io_err) => io_err,
			other => io::Error::new(io::ErrorKind::Other, other.to_string()),
		}
	}
}

impl From<DatabaseError> for cdk::cdk_database::Error {
	fn from(err: DatabaseError) -> Self {
		match err {
			DatabaseError::Serialization(msg) => cdk::cdk_database::Error::Database(msg.into()),
			DatabaseError::Io(io_err) => {
				cdk::cdk_database::Error::Database(io_err.to_string().into())
			},
			DatabaseError::NotFound => {
				cdk::cdk_database::Error::Database("Data not found".to_string().into())
			},
			DatabaseError::InvalidFormat => {
				cdk::cdk_database::Error::Database("Invalid data format".to_string().into())
			},
			DatabaseError::Duplicate => cdk::cdk_database::Error::Duplicate,
		}
	}
}

/// A KV store-based implementation of the Cashu WalletDatabase trait
pub struct CashuKvDatabase {
	store: Arc<dyn KVStore + Send + Sync>,
	// In-memory caches for frequently accessed data
	mints_cache: Arc<RwLock<HashMap<MintUrl, Option<MintInfo>>>>,
	proofs_cache: Arc<RwLock<Vec<ProofInfo>>>,
}

impl Debug for CashuKvDatabase {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("CashuKvDatabase")
			.field("store", &"<KVStore>")
			.field("mints_cache", &self.mints_cache)
			.field("proofs_cache", &self.proofs_cache)
			.finish()
	}
}

impl CashuKvDatabase {
	/// Creates a new CashuKvDatabase instance with the provided KV store.
	///
	/// This constructor initializes the database and loads any existing data from
	/// the persistent storage into in-memory caches for improved performance.
	///
	/// # Arguments
	///
	/// * `store` - The key-value store backend for persistent storage
	///
	/// # Returns
	///
	/// Returns a `Result` containing the initialized database or a `DatabaseError` if
	/// initialization fails.
	pub fn new(store: Arc<dyn KVStore + Send + Sync>) -> Result<Self, DatabaseError> {
		let database = Self {
			store,
			mints_cache: Arc::new(RwLock::new(HashMap::new())),
			proofs_cache: Arc::new(RwLock::new(Vec::new())),
		};

		// Initialize caches from persistent storage
		database.load_caches()?;

		Ok(database)
	}

	fn load_caches(&self) -> Result<(), DatabaseError> {
		// Load mints cache
		if let Ok(mints) = self.load_mints_from_store() {
			let mut cache = self.mints_cache.write().unwrap();
			*cache = mints;
		}

		// Load proofs cache
		if let Ok(proofs) = self.load_proofs_from_store() {
			let mut cache = self.proofs_cache.write().unwrap();
			*cache = proofs;
		}

		Ok(())
	}

	fn load_mints_from_store(&self) -> Result<HashMap<MintUrl, Option<MintInfo>>, DatabaseError> {
		let keys = self.store.list(CASHU_PRIMARY_KEY, MINTS_KEY).map_err(DatabaseError::Io)?;

		let mut mints = HashMap::new();
		for key in keys {
			let data =
				self.store.read(CASHU_PRIMARY_KEY, MINTS_KEY, &key).map_err(DatabaseError::Io)?;

			if !data.is_empty() {
				let mint_url: MintUrl = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

				// Try to load mint info
				let mint_info = self.load_mint_info(&mint_url).ok().flatten();
				mints.insert(mint_url, mint_info);
			}
		}

		Ok(mints)
	}

	fn load_proofs_from_store(&self) -> Result<Vec<ProofInfo>, DatabaseError> {
		let keys = self.store.list(CASHU_PRIMARY_KEY, PROOFS_KEY).map_err(DatabaseError::Io)?;

		let mut proofs = Vec::new();
		for key in keys {
			let data =
				self.store.read(CASHU_PRIMARY_KEY, PROOFS_KEY, &key).map_err(DatabaseError::Io)?;

			if !data.is_empty() {
				let proof: ProofInfo = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

				proofs.push(proof);
			}
		}

		Ok(proofs)
	}

	fn load_mint_info(&self, mint_url: &MintUrl) -> Result<Option<MintInfo>, DatabaseError> {
		let key = Self::generate_mint_info_key(mint_url);
		match self.store.read(CASHU_PRIMARY_KEY, MINTS_KEY, &key) {
			Ok(data) => {
				if data.is_empty() {
					return Ok(None);
				}
				let info: MintInfo = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
				Ok(Some(info))
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(e) => Err(DatabaseError::Io(e).into()),
		}
	}

	fn save_mint_info(
		&self, mint_url: &MintUrl, mint_info: &MintInfo,
	) -> Result<(), DatabaseError> {
		let key = Self::generate_mint_info_key(mint_url);
		let data = serde_json::to_vec(mint_info)
			.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

		self.store.write(CASHU_PRIMARY_KEY, MINTS_KEY, &key, &data).map_err(DatabaseError::Io)
	}

	fn generate_proof_key(proof: &ProofInfo) -> String {
		// Generate a unique key for the proof based on the Y coordinate
		format!("proof_{}", hex::encode(&proof.y.serialize()))
	}

	fn generate_mint_key(mint_url: &MintUrl) -> String {
		// Generate a deterministic hash of the mint URL for use as a key
		let mut hasher = DefaultHasher::new();
		mint_url.to_string().hash(&mut hasher);
		let hash = hasher.finish();
		format!("mint_{:x}", hash)
	}

	fn generate_mint_info_key(mint_url: &MintUrl) -> String {
		// Generate a deterministic hash for mint info key
		let mut hasher = DefaultHasher::new();
		mint_url.to_string().hash(&mut hasher);
		let hash = hasher.finish();
		format!("info_{:x}", hash)
	}

	fn generate_mint_keysets_key(mint_url: &MintUrl) -> String {
		// Generate a deterministic hash for mint keysets key
		let mut hasher = DefaultHasher::new();
		mint_url.to_string().hash(&mut hasher);
		let hash = hasher.finish();
		format!("keysets_{:x}", hash)
	}
}

#[async_trait]
impl WalletDatabase for CashuKvDatabase {
	type Err = cdk::cdk_database::Error;

	async fn add_mint(
		&self, mint_url: MintUrl, mint_info: Option<MintInfo>,
	) -> Result<(), Self::Err> {
		// Save mint URL using hashed key
		let mint_key = Self::generate_mint_key(&mint_url);
		let mint_data = serde_json::to_vec(&mint_url)
			.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

		self.store
			.write(CASHU_PRIMARY_KEY, MINTS_KEY, &mint_key, &mint_data)
			.map_err(DatabaseError::Io)?;

		// Save mint info if provided
		if let Some(info) = &mint_info {
			self.save_mint_info(&mint_url, info)?;
		}

		// Update cache
		{
			let mut cache = self.mints_cache.write().unwrap();
			cache.insert(mint_url, mint_info);
		}

		Ok(())
	}

	async fn remove_mint(&self, mint_url: MintUrl) -> Result<(), Self::Err> {
		let mint_key = Self::generate_mint_key(&mint_url);

		// Remove mint URL by writing empty data
		self.store
			.write(CASHU_PRIMARY_KEY, MINTS_KEY, &mint_key, &[])
			.map_err(DatabaseError::Io)?;

		// Remove mint info
		let info_key = Self::generate_mint_info_key(&mint_url);
		self.store
			.write(CASHU_PRIMARY_KEY, MINTS_KEY, &info_key, &[])
			.map_err(DatabaseError::Io)?;

		// Remove mint keysets
		let keysets_key = Self::generate_mint_keysets_key(&mint_url);
		self.store
			.write(CASHU_PRIMARY_KEY, MINT_KEYSETS_KEY, &keysets_key, &[])
			.map_err(DatabaseError::Io)?;

		// Update cache
		{
			let mut cache = self.mints_cache.write().unwrap();
			cache.remove(&mint_url);
		}

		Ok(())
	}

	async fn get_mint(&self, mint_url: MintUrl) -> Result<Option<MintInfo>, Self::Err> {
		// Check cache first
		{
			let cache = self.mints_cache.read().unwrap();
			if let Some(mint_info) = cache.get(&mint_url) {
				return Ok(mint_info.clone());
			}
		}

		// Load from storage
		self.load_mint_info(&mint_url).map_err(Into::into)
	}

	async fn get_mints(&self) -> Result<HashMap<MintUrl, Option<MintInfo>>, Self::Err> {
		let cache = self.mints_cache.read().unwrap();
		Ok(cache.clone())
	}

	async fn update_mint_url(
		&self, old_mint_url: MintUrl, new_mint_url: MintUrl,
	) -> Result<(), Self::Err> {
		// Get the mint info from the old URL
		let mint_info = self.get_mint(old_mint_url.clone()).await?;

		// Get the mint keysets from the old URL
		let mint_keysets = self.get_mint_keysets(old_mint_url.clone()).await?;

		// Add with new URL
		self.add_mint(new_mint_url.clone(), mint_info).await?;

		// Add keysets if they exist
		if let Some(keysets) = mint_keysets {
			self.add_mint_keysets(new_mint_url, keysets).await?;
		}

		// Remove old URL (this will remove mint, mint_info, and mint_keysets)
		self.remove_mint(old_mint_url).await?;

		Ok(())
	}

	async fn add_mint_keysets(
		&self, mint_url: MintUrl, keysets: Vec<KeySetInfo>,
	) -> Result<(), Self::Err> {
		let mut existing_u32 = false;
		let mut updated_keysets = Vec::new();

		for keyset in keysets {
			// Check if keyset already exists in individual keysets table
			let keyset_key = format!("keyset_{}", keyset.id.to_string());
			let existing_keyset =
				match self.store.read(CASHU_PRIMARY_KEY, KEYSETS_TABLE_KEY, &keyset_key) {
					Ok(data) if !data.is_empty() => {
						let existing: KeySetInfo = serde_json::from_slice(&data)
							.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
						Some(existing)
					},
					_ => None,
				};

			// Check u32 mapping for conflicts
			let u32_key = format!("u32_{}", u32::from(keyset.id));
			match self.store.read(CASHU_PRIMARY_KEY, KEYSET_U32_MAPPING_KEY, &u32_key) {
				Ok(data) if !data.is_empty() => {
					let existing_id_str = String::from_utf8(data.to_vec())
						.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
					let existing_id = Id::from_str(&existing_id_str)
						.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

					if existing_id != keyset.id {
						existing_u32 = true;
						break;
					}
				},
				_ => {
					// No existing mapping, create one
					let id_data = keyset.id.to_string().as_bytes().to_vec();
					self.store
						.write(CASHU_PRIMARY_KEY, KEYSET_U32_MAPPING_KEY, &u32_key, &id_data)
						.map_err(DatabaseError::Io)?;
				},
			}

			// Handle keyset data - merge or create new
			let final_keyset = if let Some(mut existing_keyset) = existing_keyset {
				// Update fields from new keyset
				existing_keyset.active = keyset.active;
				existing_keyset.input_fee_ppk = keyset.input_fee_ppk;
				existing_keyset
			} else {
				keyset.clone()
			};

			// Store individual keyset
			let keyset_data = serde_json::to_vec(&final_keyset)
				.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
			self.store
				.write(CASHU_PRIMARY_KEY, KEYSETS_TABLE_KEY, &keyset_key, &keyset_data)
				.map_err(DatabaseError::Io)?;

			updated_keysets.push(final_keyset);
		}

		// If there was a u32 conflict, rollback all changes
		if existing_u32 {
			// Log warning: Keyset already exists for keyset id
			return Err(DatabaseError::Duplicate.into());
		}

		// Update the mint keysets association
		let key = Self::generate_mint_keysets_key(&mint_url);

		// Get existing mint keysets
		let mut all_mint_keysets =
			self.get_mint_keysets(mint_url.clone()).await?.unwrap_or_default();

		// Merge with new keysets (replace existing ones with same ID)
		for new_keyset in updated_keysets {
			if let Some(existing_pos) = all_mint_keysets.iter().position(|k| k.id == new_keyset.id)
			{
				all_mint_keysets[existing_pos] = new_keyset;
			} else {
				all_mint_keysets.push(new_keyset);
			}
		}

		let data = serde_json::to_vec(&all_mint_keysets)
			.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

		self.store
			.write(CASHU_PRIMARY_KEY, MINT_KEYSETS_KEY, &key, &data)
			.map_err(DatabaseError::Io)?;

		Ok(())
	}

	async fn get_mint_keysets(
		&self, mint_url: MintUrl,
	) -> Result<Option<Vec<KeySetInfo>>, Self::Err> {
		let key = Self::generate_mint_keysets_key(&mint_url);

		match self.store.read(CASHU_PRIMARY_KEY, MINT_KEYSETS_KEY, &key) {
			Ok(data) => {
				if data.is_empty() {
					return Ok(None);
				}
				let keysets: Vec<KeySetInfo> = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
				Ok(Some(keysets))
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(e) => Err(DatabaseError::Io(e).into()),
		}
	}

	async fn get_keyset_by_id(&self, keyset_id: &Id) -> Result<Option<KeySetInfo>, Self::Err> {
		// Get all mint keysets and search for the one with matching ID
		let keys =
			self.store.list(CASHU_PRIMARY_KEY, MINT_KEYSETS_KEY).map_err(DatabaseError::Io)?;

		for key in keys {
			let data = self
				.store
				.read(CASHU_PRIMARY_KEY, MINT_KEYSETS_KEY, &key)
				.map_err(DatabaseError::Io)?;

			if !data.is_empty() {
				let keysets: Vec<KeySetInfo> = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

				for keyset in keysets {
					if &keyset.id == keyset_id {
						return Ok(Some(keyset));
					}
				}
			}
		}

		Ok(None)
	}

	async fn add_mint_quote(&self, quote: MintQuote) -> Result<(), Self::Err> {
		let key = quote.id.clone();
		let data =
			serde_json::to_vec(&quote).map_err(|e| DatabaseError::Serialization(e.to_string()))?;

		self.store
			.write(CASHU_PRIMARY_KEY, MINT_QUOTES_KEY, &key, &data)
			.map_err(DatabaseError::Io)?;

		Ok(())
	}

	async fn get_mint_quote(&self, quote_id: &str) -> Result<Option<MintQuote>, Self::Err> {
		match self.store.read(CASHU_PRIMARY_KEY, MINT_QUOTES_KEY, quote_id) {
			Ok(data) => {
				if data.is_empty() {
					return Ok(None);
				}
				let quote: MintQuote = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
				Ok(Some(quote))
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(e) => Err(DatabaseError::Io(e).into()),
		}
	}

	async fn get_mint_quotes(&self) -> Result<Vec<MintQuote>, Self::Err> {
		let keys =
			self.store.list(CASHU_PRIMARY_KEY, MINT_QUOTES_KEY).map_err(DatabaseError::Io)?;

		let mut quotes = Vec::new();
		for key in keys {
			let data = self
				.store
				.read(CASHU_PRIMARY_KEY, MINT_QUOTES_KEY, &key)
				.map_err(DatabaseError::Io)?;

			if !data.is_empty() {
				let quote: MintQuote = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
				quotes.push(quote);
			}
		}

		Ok(quotes)
	}

	async fn remove_mint_quote(&self, quote_id: &str) -> Result<(), Self::Err> {
		// Mark as removed by writing empty data
		self.store
			.write(CASHU_PRIMARY_KEY, MINT_QUOTES_KEY, quote_id, &[])
			.map_err(DatabaseError::Io)?;

		Ok(())
	}

	async fn add_melt_quote(&self, quote: MeltQuote) -> Result<(), Self::Err> {
		let key = quote.id.clone();
		let data =
			serde_json::to_vec(&quote).map_err(|e| DatabaseError::Serialization(e.to_string()))?;

		self.store
			.write(CASHU_PRIMARY_KEY, MELT_QUOTES_KEY, &key, &data)
			.map_err(DatabaseError::Io)?;

		Ok(())
	}

	async fn get_melt_quote(&self, quote_id: &str) -> Result<Option<MeltQuote>, Self::Err> {
		match self.store.read(CASHU_PRIMARY_KEY, MELT_QUOTES_KEY, quote_id) {
			Ok(data) => {
				if data.is_empty() {
					return Ok(None);
				}
				let quote: MeltQuote = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
				Ok(Some(quote))
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(e) => Err(DatabaseError::Io(e).into()),
		}
	}

	async fn get_melt_quotes(&self) -> Result<Vec<MeltQuote>, Self::Err> {
		let keys =
			self.store.list(CASHU_PRIMARY_KEY, MELT_QUOTES_KEY).map_err(DatabaseError::Io)?;

		let mut quotes = Vec::new();
		for key in keys {
			let data = self
				.store
				.read(CASHU_PRIMARY_KEY, MELT_QUOTES_KEY, &key)
				.map_err(DatabaseError::Io)?;

			if !data.is_empty() {
				let quote: MeltQuote = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
				quotes.push(quote);
			}
		}

		Ok(quotes)
	}

	async fn remove_melt_quote(&self, quote_id: &str) -> Result<(), Self::Err> {
		self.store
			.write(CASHU_PRIMARY_KEY, MELT_QUOTES_KEY, quote_id, &[])
			.map_err(DatabaseError::Io)?;

		Ok(())
	}

	async fn add_keys(&self, keyset: KeySet) -> Result<(), Self::Err> {
		if self.get_keys(&keyset.id).await?.is_some() {
			return Ok(());
		}

		let key = keyset.id.to_string();
		let data =
			serde_json::to_vec(&keyset).map_err(|e| DatabaseError::Serialization(e.to_string()))?;

		self.store.write(CASHU_PRIMARY_KEY, KEYS_KEY, &key, &data).map_err(DatabaseError::Io)?;

		Ok(())
	}

	async fn get_keys(&self, id: &Id) -> Result<Option<Keys>, Self::Err> {
		let key = id.to_string();

		match self.store.read(CASHU_PRIMARY_KEY, KEYS_KEY, &key) {
			Ok(data) => {
				if data.is_empty() {
					return Ok(None);
				}
				let keyset: KeySet = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
				Ok(Some(keyset.keys))
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(e) => Err(DatabaseError::Io(e).into()),
		}
	}

	async fn remove_keys(&self, id: &Id) -> Result<(), Self::Err> {
		let key = id.to_string();

		self.store.write(CASHU_PRIMARY_KEY, KEYS_KEY, &key, &[]).map_err(DatabaseError::Io)?;

		Ok(())
	}

	async fn update_proofs(
		&self, added: Vec<ProofInfo>, removed_ys: Vec<PublicKey>,
	) -> Result<(), Self::Err> {
		// Add new proofs
		for proof in &added {
			let key = Self::generate_proof_key(proof);
			let data = serde_json::to_vec(proof)
				.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

			self.store
				.write(CASHU_PRIMARY_KEY, PROOFS_KEY, &key, &data)
				.map_err(DatabaseError::Io)?;
		}

		// Remove proofs by Y values
		for y in &removed_ys {
			let key = format!("proof_{}", hex::encode(&y.serialize()));

			self.store
				.write(CASHU_PRIMARY_KEY, PROOFS_KEY, &key, &[])
				.map_err(DatabaseError::Io)?;
		}

		// Update cache
		{
			let mut cache = self.proofs_cache.write().unwrap();

			// Add new proofs
			cache.extend(added);

			// Remove proofs with matching Y values
			cache.retain(|proof| !removed_ys.contains(&proof.y));
		}

		Ok(())
	}

	async fn get_proofs(
		&self, mint_url: Option<MintUrl>, unit: Option<CurrencyUnit>, state: Option<Vec<State>>,
		spending_conditions: Option<Vec<SpendingConditions>>,
	) -> Result<Vec<ProofInfo>, Self::Err> {
		let cache = self.proofs_cache.read().unwrap();
		let mut filtered_proofs = cache.clone();

		// Apply filters
		if let Some(mint_url) = mint_url {
			filtered_proofs.retain(|proof| proof.mint_url == mint_url);
		}

		if let Some(unit) = unit {
			filtered_proofs.retain(|proof| proof.unit == unit);
		}

		if let Some(states) = state {
			filtered_proofs.retain(|proof| states.contains(&proof.state));
		}

		if let Some(conditions) = spending_conditions {
			filtered_proofs.retain(|proof| {
				proof
					.spending_condition
					.as_ref()
					.map(|sc| conditions.contains(sc))
					.unwrap_or(conditions.is_empty())
			});
		}

		Ok(filtered_proofs)
	}

	async fn update_proofs_state(&self, ys: Vec<PublicKey>, state: State) -> Result<(), Self::Err> {
		// Update proofs in storage and cache
		for y in &ys {
			let key = format!("proof_{}", hex::encode(&y.serialize()));

			// Read existing proof
			match self.store.read(CASHU_PRIMARY_KEY, PROOFS_KEY, &key) {
				Ok(data) if !data.is_empty() => {
					let mut proof: ProofInfo = serde_json::from_slice(&data)
						.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

					// Update state
					proof.state = state;

					// Write back
					let updated_data = serde_json::to_vec(&proof)
						.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

					self.store
						.write(CASHU_PRIMARY_KEY, PROOFS_KEY, &key, &updated_data)
						.map_err(DatabaseError::Io)?;
				},
				_ => continue, // Proof not found, skip
			}
		}

		// Update cache
		{
			let mut cache = self.proofs_cache.write().unwrap();
			for proof in cache.iter_mut() {
				if ys.contains(&proof.y) {
					proof.state = state;
				}
			}
		}

		Ok(())
	}

	async fn increment_keyset_counter(&self, keyset_id: &Id, count: u32) -> Result<u32, Self::Err> {
		let key = keyset_id.to_string();

		// Read current counter
		let current_count = match self.store.read(CASHU_PRIMARY_KEY, KEYSET_COUNTERS_KEY, &key) {
			Ok(data) if !data.is_empty() => serde_json::from_slice::<u32>(&data)
				.map_err(|e| DatabaseError::Serialization(e.to_string()))?,
			_ => 0, // Default to 0 if not found
		};

		let new_count = current_count + count;

		// Write back updated counter
		let data = serde_json::to_vec(&new_count)
			.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

		self.store
			.write(CASHU_PRIMARY_KEY, KEYSET_COUNTERS_KEY, &key, &data)
			.map_err(DatabaseError::Io)?;

		Ok(new_count)
	}

	async fn add_transaction(&self, transaction: Transaction) -> Result<(), Self::Err> {
		let key = transaction.id().to_string();
		let data = serde_json::to_vec(&transaction)
			.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

		self.store
			.write(CASHU_PRIMARY_KEY, TRANSACTIONS_KEY, &key, &data)
			.map_err(DatabaseError::Io)?;

		Ok(())
	}

	async fn get_transaction(
		&self, transaction_id: TransactionId,
	) -> Result<Option<Transaction>, Self::Err> {
		let key = transaction_id.to_string();

		match self.store.read(CASHU_PRIMARY_KEY, TRANSACTIONS_KEY, &key) {
			Ok(data) => {
				if data.is_empty() {
					return Ok(None);
				}
				let transaction: Transaction = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;
				Ok(Some(transaction))
			},
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(e) => Err(DatabaseError::Io(e).into()),
		}
	}

	async fn list_transactions(
		&self, mint_url: Option<MintUrl>, direction: Option<TransactionDirection>,
		unit: Option<CurrencyUnit>,
	) -> Result<Vec<Transaction>, Self::Err> {
		let keys =
			self.store.list(CASHU_PRIMARY_KEY, TRANSACTIONS_KEY).map_err(DatabaseError::Io)?;

		let mut transactions = Vec::new();
		for key in keys {
			let data = self
				.store
				.read(CASHU_PRIMARY_KEY, TRANSACTIONS_KEY, &key)
				.map_err(DatabaseError::Io)?;

			if !data.is_empty() {
				let transaction: Transaction = serde_json::from_slice(&data)
					.map_err(|e| DatabaseError::Serialization(e.to_string()))?;

				// Apply filters
				let mut include = true;

				if let Some(mint_url) = &mint_url {
					if transaction.mint_url != *mint_url {
						include = false;
					}
				}

				if let Some(direction) = direction {
					if transaction.direction != direction {
						include = false;
					}
				}

				if let Some(ref unit) = unit {
					if transaction.unit != *unit {
						include = false;
					}
				}

				if include {
					transactions.push(transaction);
				}
			}
		}

		Ok(transactions)
	}

	async fn remove_transaction(&self, transaction_id: TransactionId) -> Result<(), Self::Err> {
		let key = transaction_id.to_string();

		self.store
			.write(CASHU_PRIMARY_KEY, TRANSACTIONS_KEY, &key, &[])
			.map_err(DatabaseError::Io)?;

		Ok(())
	}
}
