use spark_wallet::Network as SparkNetwork;
use spark_wallet::SparkWalletConfig as SparkSparkWalletConfig;

use crate::ffi::Network;
use crate::{impl_from_core_type, impl_into_core_type};

impl From<SparkNetwork> for Network {
	fn from(network: SparkNetwork) -> Self {
		match network {
			SparkNetwork::Mainnet => Network::Mainnet,
			SparkNetwork::Regtest => Network::Regtest,
			SparkNetwork::Testnet => Network::Testnet,
			SparkNetwork::Signet => Network::Signet,
		}
	}
}

impl From<Network> for SparkNetwork {
	fn from(network: Network) -> Self {
		match network {
			Network::Mainnet => SparkNetwork::Mainnet,
			Network::Regtest => SparkNetwork::Regtest,
			Network::Testnet => SparkNetwork::Testnet,
			Network::Signet => SparkNetwork::Signet,
		}
	}
}

#[derive(Clone, Debug, uniffi::Object)]
pub struct SparkWalletConfig(pub SparkSparkWalletConfig);

// TODO: For now just support the default configuration.
// In the future we will want to expose all of the Spark configuration objects
#[uniffi::export]
impl SparkWalletConfig {
	#[uniffi::constructor]
	pub fn default_config(network: Network) -> Self {
		SparkWalletConfig(SparkSparkWalletConfig::default_config(network.into()))
	}
}

impl_from_core_type!(SparkSparkWalletConfig, SparkWalletConfig);
impl_into_core_type!(SparkWalletConfig, SparkSparkWalletConfig);
