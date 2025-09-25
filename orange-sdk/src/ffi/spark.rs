use crate::SparkWalletConfig as OrangeSparkWalletConfig;
use crate::{impl_from_core_type, impl_into_core_type};

#[derive(Clone, Debug, uniffi::Object)]
pub struct SparkWalletConfig(pub OrangeSparkWalletConfig);

// TODO: For now just support the default configuration.
// In the future we will want to expose all of the Spark configuration objects
#[uniffi::export]
impl SparkWalletConfig {
	#[uniffi::constructor]
	pub fn default_config() -> Self {
		SparkWalletConfig(OrangeSparkWalletConfig::default())
	}
}

impl_from_core_type!(OrangeSparkWalletConfig, SparkWalletConfig);
impl_into_core_type!(SparkWalletConfig, OrangeSparkWalletConfig);
