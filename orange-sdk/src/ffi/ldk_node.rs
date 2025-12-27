use crate::ffi::Network;
use crate::ffi::orange::error::ConfigError;
use crate::{impl_from_core_type, impl_into_core_type};
use ldk_node::ChannelDetails as LDKChannelDetails;
use ldk_node::bip39::Mnemonic as LDKMnemonic;
use ldk_node::bip39::rand_core::RngCore;
use ldk_node::bitcoin::Network as LDKNetwork;
use std::str::FromStr;

impl From<Network> for LDKNetwork {
	fn from(network: Network) -> Self {
		match network {
			Network::Mainnet => ldk_node::bitcoin::Network::Bitcoin,
			Network::Regtest => ldk_node::bitcoin::Network::Regtest,
			Network::Testnet => ldk_node::bitcoin::Network::Testnet,
			Network::Signet => ldk_node::bitcoin::Network::Signet,
		}
	}
}

#[derive(Debug, Clone, uniffi::Object)]
#[uniffi::export(Display)]
pub struct Mnemonic(pub LDKMnemonic);

#[uniffi::export]
impl Mnemonic {
	#[uniffi::constructor]
	pub fn from_entropy(entropy: Vec<u8>) -> Result<Self, ConfigError> {
		match LDKMnemonic::from_entropy(&entropy) {
			Ok(mnemonic) => Ok(Mnemonic(mnemonic)),
			Err(_) => Err(ConfigError::InvalidEntropySize(entropy.len() as u32)),
		}
	}

	#[uniffi::constructor]
	pub fn from_str(str: &str) -> Result<Self, ConfigError> {
		match LDKMnemonic::from_str(str) {
			Ok(mnemonic) => Ok(Mnemonic(mnemonic)),
			Err(_) => Err(ConfigError::InvalidMnemonic),
		}
	}

	#[uniffi::constructor]
	pub fn generate() -> Result<Self, ConfigError> {
		let mut entropy = [0u8; 16]; // 128 bits for 12-word mnemonic
		rand::thread_rng().fill_bytes(&mut entropy);
		Self::from_entropy(entropy.to_vec())
	}
}

impl std::fmt::Display for Mnemonic {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

impl_from_core_type!(LDKMnemonic, Mnemonic);
impl_into_core_type!(Mnemonic, LDKMnemonic);

#[derive(Debug, Clone, uniffi::Object)]
pub struct ChannelDetails(pub LDKChannelDetails);

#[uniffi::export]
impl ChannelDetails {
	/// The channel ID (used for identification purposes).
	pub fn channel_id(&self) -> Vec<u8> {
		self.0.channel_id.0.to_vec()
	}

	/// The node ID of the counterparty.
	pub fn counterparty_node_id(&self) -> String {
		self.0.counterparty_node_id.to_string()
	}

	/// The funding transaction output.
	pub fn funding_txo(&self) -> Option<String> {
		self.0.funding_txo.as_ref().map(|txo| format!("{}:{}", txo.txid, txo.vout))
	}

	/// The value, in satoshis, of this channel as it appears in the funding transaction.
	pub fn channel_value_sats(&self) -> u64 {
		self.0.channel_value_sats
	}

	/// The currently negotiated fee rate denominated in satoshi per 1000 weight units.
	pub fn feerate_sat_per_1000_weight(&self) -> u32 {
		self.0.feerate_sat_per_1000_weight
	}

	/// The available outbound capacity for sending HTLCs to the remote peer.
	pub fn outbound_capacity_msat(&self) -> u64 {
		self.0.outbound_capacity_msat
	}

	/// The available inbound capacity for receiving HTLCs from the remote peer.
	pub fn inbound_capacity_msat(&self) -> u64 {
		self.0.inbound_capacity_msat
	}

	/// The number of required confirmations on the funding transaction before the funding will be
	/// considered confirmed.
	pub fn confirmations_required(&self) -> Option<u32> {
		self.0.confirmations_required
	}

	/// The number of confirmations the funding transaction has.
	pub fn confirmations(&self) -> Option<u32> {
		self.0.confirmations
	}

	/// True if the channel was initiated (and thus funded) by us.
	pub fn is_outbound(&self) -> bool {
		self.0.is_outbound
	}

	/// True if the channel is confirmed, channel_ready messages have been exchanged, and the channel
	/// is not currently being shut down.
	pub fn is_channel_ready(&self) -> bool {
		self.0.is_channel_ready
	}

	/// True if the channel is (a) confirmed and channel_ready messages have been exchanged,
	/// and (b) the peer is connected and up-to-date.
	pub fn is_usable(&self) -> bool {
		self.0.is_usable
	}

	/// True if this channel is (or will be) publicly-announced.
	pub fn is_public(&self) -> bool {
		self.0.is_announced
	}

	/// The difference in the CLTV value between incoming HTLCs and an outbound HTLC forwarded over
	/// the channel.
	pub fn cltv_expiry_delta(&self) -> Option<u16> {
		self.0.cltv_expiry_delta
	}

	/// The value, in msat, that must always be held in the channel for us.
	/// This value ensures that if we close the channel, we will have some spendable balance.
	pub fn unspendable_punishment_reserve(&self) -> Option<u64> {
		self.0.unspendable_punishment_reserve
	}

	/// A unique u128 identifier for this channel.
	pub fn user_channel_id(&self) -> String {
		self.0.user_channel_id.0.to_string()
	}

	/// The short channel ID if the channel has been announced and is publicly-addressable.
	pub fn short_channel_id(&self) -> Option<u64> {
		self.0.short_channel_id
	}
}

impl_from_core_type!(LDKChannelDetails, ChannelDetails);
impl_into_core_type!(ChannelDetails, LDKChannelDetails);
