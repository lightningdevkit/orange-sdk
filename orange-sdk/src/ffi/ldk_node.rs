use crate::ffi::Network;
use crate::ffi::orange::error::ConfigError;
use crate::{impl_from_core_type, impl_into_core_type};
use ldk_node::bip39::Mnemonic as LDKMnemonic;
use ldk_node::bitcoin::Network as LDKNetwork;

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
}

impl_from_core_type!(LDKMnemonic, Mnemonic);
impl_into_core_type!(Mnemonic, LDKMnemonic);
