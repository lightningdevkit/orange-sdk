use crate::trusted_wallet::cashu::CashuConfig as OrangeCashuConfig;
use cdk::nuts::CurrencyUnit as CdkCurrencyUnit;

#[derive(Debug, Clone, uniffi::Enum)]
pub enum CurrencyUnit {
	Sat,
	Msat,
	Usd,
	Eur,
	Auth,
	Custom(String),
}

impl From<CdkCurrencyUnit> for CurrencyUnit {
	fn from(unit: CdkCurrencyUnit) -> Self {
		match unit {
			CdkCurrencyUnit::Sat => CurrencyUnit::Sat,
			CdkCurrencyUnit::Msat => CurrencyUnit::Msat,
			CdkCurrencyUnit::Usd => CurrencyUnit::Usd,
			CdkCurrencyUnit::Eur => CurrencyUnit::Eur,
			CdkCurrencyUnit::Auth => CurrencyUnit::Auth,
			CdkCurrencyUnit::Custom(unit) => CurrencyUnit::Custom(unit),
			_ => CurrencyUnit::Sat,
		}
	}
}

impl From<CurrencyUnit> for CdkCurrencyUnit {
	fn from(unit: CurrencyUnit) -> Self {
		match unit {
			CurrencyUnit::Sat => CdkCurrencyUnit::Sat,
			CurrencyUnit::Msat => CdkCurrencyUnit::Msat,
			CurrencyUnit::Usd => CdkCurrencyUnit::Usd,
			CurrencyUnit::Eur => CdkCurrencyUnit::Eur,
			CurrencyUnit::Auth => CdkCurrencyUnit::Auth,
			CurrencyUnit::Custom(unit) => CdkCurrencyUnit::Custom(unit),
		}
	}
}

/// Configuration for the Cashu wallet
#[derive(Debug, Clone, uniffi::Object)]
pub struct CashuConfig {
	/// The mint URL to connect to
	pub mint_url: String,
	/// The currency unit to use (typically Sat)
	pub unit: CurrencyUnit,
}

#[uniffi::export]
impl CashuConfig {
	#[uniffi::constructor]
	pub fn new(mint_url: String, unit: CurrencyUnit) -> Self {
		CashuConfig { mint_url, unit }
	}
}

impl From<CashuConfig> for OrangeCashuConfig {
	fn from(config: CashuConfig) -> Self {
		OrangeCashuConfig { mint_url: config.mint_url, unit: config.unit.into() }
	}
}

impl From<OrangeCashuConfig> for CashuConfig {
	fn from(config: OrangeCashuConfig) -> Self {
		CashuConfig { mint_url: config.mint_url, unit: config.unit.into() }
	}
}
