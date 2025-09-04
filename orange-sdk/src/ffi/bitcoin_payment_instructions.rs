use crate::{impl_from_core_type, impl_into_core_type};
use bitcoin_payment_instructions::amount::Amount as BPIAmount;
use std::fmt::Display;
use std::sync::Arc;

#[derive(Debug, uniffi::Error)]
pub enum AmountError {
	FractionalAmountOfSats(u64),
	MoreThanTwentyOneMillionBitcoin(u64),
}

impl Display for AmountError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			AmountError::FractionalAmountOfSats(sats) => {
				write!(f, "Not exactly a whole number of sats: {}", sats)
			},
			AmountError::MoreThanTwentyOneMillionBitcoin(sats) => {
				write!(f, "More than 21 million bitcoin: {}", sats)
			},
		}
	}
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, uniffi::Object)]
pub struct Amount(pub BPIAmount);

#[uniffi::export]
impl Amount {
	#[uniffi::constructor]
	pub fn from_sats(sats: u64) -> Result<Self, AmountError> {
		let amount = BPIAmount::from_sats(sats)
			.map_err(|()| AmountError::MoreThanTwentyOneMillionBitcoin(sats))?;
		Ok(Amount(amount))
	}

	#[uniffi::constructor]
	pub fn from_milli_sats(msats: u64) -> Result<Self, AmountError> {
		let amount = BPIAmount::from_milli_sats(msats)
			.map_err(|()| AmountError::MoreThanTwentyOneMillionBitcoin(msats))?;
		Ok(Amount(amount))
	}

	pub fn milli_sats(&self) -> u64 {
		self.0.milli_sats()
	}

	pub fn sats(&self) -> Result<u64, AmountError> {
		let sats =
			self.0.sats().map_err(|()| AmountError::FractionalAmountOfSats(self.0.milli_sats()))?;
		Ok(sats)
	}

	pub fn sats_rounding_up(&self) -> u64 {
		self.0.sats_rounding_up()
	}

	pub fn saturating_add(self, rhs: Arc<Amount>) -> Amount {
		Amount(self.0.saturating_add(rhs.0))
	}

	pub fn saturating_sub(self, rhs: Arc<Amount>) -> Amount {
		Amount(self.0.saturating_sub(rhs.0))
	}
}

impl_from_core_type!(BPIAmount, Amount);
impl_into_core_type!(Amount, BPIAmount);
