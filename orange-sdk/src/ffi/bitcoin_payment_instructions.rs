use crate::{
	PaymentInfo as OrangePaymentInfo, PaymentInfoBuildError as OrangePaymentInfoBuildError,
	impl_from_core_type, impl_into_core_type,
};
use bitcoin_payment_instructions::amount::Amount as BPIAmount;
use bitcoin_payment_instructions::{
	ParseError as BPIParseError, PaymentInstructions as BPIPaymentInstructions,
};
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

#[derive(Debug, uniffi::Error)]
pub enum ParseError {
	InvalidBolt11(String),
	InvalidBolt12(String),
	InvalidOnChain(String),
	WrongNetwork,
	InconsistentInstructions(String),
	InvalidInstructions(String),
	UnknownPaymentInstructions,
	UnknownRequiredParameter,
	HrnResolutionError(String),
	InstructionsExpired,
	InvalidLnurl(String),
}

impl Display for ParseError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			ParseError::InvalidBolt11(e) => write!(f, "Invalid BOLT 11 invoice: {}", e),
			ParseError::InvalidBolt12(e) => write!(f, "Invalid BOLT 12 offer: {}", e),
			ParseError::InvalidOnChain(e) => write!(f, "Invalid on-chain address: {}", e),
			ParseError::WrongNetwork => write!(f, "Wrong network for payment instructions"),
			ParseError::InconsistentInstructions(e) => {
				write!(f, "Inconsistent payment instructions: {}", e)
			},
			ParseError::InvalidInstructions(e) => write!(f, "Invalid payment instructions: {}", e),
			ParseError::UnknownPaymentInstructions => {
				write!(f, "Unknown payment instruction format")
			},
			ParseError::UnknownRequiredParameter => {
				write!(f, "Unknown required parameter in BIP 21 URI")
			},
			ParseError::HrnResolutionError(e) => {
				write!(f, "Human readable name resolution error: {}", e)
			},
			ParseError::InstructionsExpired => write!(f, "Payment instructions have expired"),
			ParseError::InvalidLnurl(e) => write!(f, "Invalid LNURL: {}", e),
		}
	}
}

impl From<BPIParseError> for ParseError {
	fn from(error: BPIParseError) -> Self {
		match error {
			BPIParseError::InvalidBolt11(e) => ParseError::InvalidBolt11(format!("{:?}", e)),
			BPIParseError::InvalidBolt12(e) => ParseError::InvalidBolt12(format!("{:?}", e)),
			BPIParseError::InvalidOnChain(e) => ParseError::InvalidOnChain(format!("{:?}", e)),
			BPIParseError::WrongNetwork => ParseError::WrongNetwork,
			BPIParseError::InconsistentInstructions(msg) => {
				ParseError::InconsistentInstructions(msg.to_string())
			},
			BPIParseError::InvalidInstructions(msg) => {
				ParseError::InvalidInstructions(msg.to_string())
			},
			BPIParseError::UnknownPaymentInstructions => ParseError::UnknownPaymentInstructions,
			BPIParseError::UnknownRequiredParameter => ParseError::UnknownRequiredParameter,
			BPIParseError::HrnResolutionError(msg) => {
				ParseError::HrnResolutionError(msg.to_string())
			},
			BPIParseError::InstructionsExpired => ParseError::InstructionsExpired,
			BPIParseError::InvalidLnurl(msg) => ParseError::InvalidLnurl(msg.to_string()),
		}
	}
}

#[derive(Debug, Clone, uniffi::Object)]
pub struct PaymentInstructions(pub BPIPaymentInstructions);

#[uniffi::export]
impl PaymentInstructions {
	/// Check if these are configurable amount payment instructions
	pub fn is_configurable_amount(&self) -> bool {
		matches!(self.0, BPIPaymentInstructions::ConfigurableAmount(_))
	}

	/// Check if these are fixed amount payment instructions  
	pub fn is_fixed_amount(&self) -> bool {
		matches!(self.0, BPIPaymentInstructions::FixedAmount(_))
	}

	/// Get the recipient-provided description of the payment instructions
	pub fn recipient_description(&self) -> Option<String> {
		self.0.recipient_description().map(|s| s.to_string())
	}

	/// Get the proof-of-payment callback URI if available
	pub fn pop_callback(&self) -> Option<String> {
		self.0.pop_callback().map(|s| s.to_string())
	}

	/// Get the minimum amount for configurable amount payment instructions
	///
	/// Returns the minimum amount the recipient will accept, if specified.
	/// Only applies to configurable amount payment instructions.
	pub fn min_amount(&self) -> Option<Arc<Amount>> {
		match &self.0 {
			BPIPaymentInstructions::ConfigurableAmount(conf) => {
				conf.min_amt().map(|amt| Arc::new(amt.into()))
			},
			BPIPaymentInstructions::FixedAmount(_) => None,
		}
	}

	/// Get the maximum amount for payment instructions
	///
	/// For configurable amount instructions, returns the maximum allowed amount.
	/// For fixed amount instructions, returns the fixed payment amount.
	pub fn max_amount(&self) -> Option<Arc<Amount>> {
		match &self.0 {
			BPIPaymentInstructions::ConfigurableAmount(conf) => {
				conf.max_amt().map(|amt| Arc::new(amt.into()))
			},
			BPIPaymentInstructions::FixedAmount(fixed) => {
				fixed.max_amount().map(|amt| Arc::new(amt.into()))
			},
		}
	}

	/// Get the lightning payment amount for fixed amount instructions
	///
	/// Returns the specific amount required for lightning payments.
	/// Only applies to fixed amount payment instructions.
	pub fn ln_payment_amount(&self) -> Option<Arc<Amount>> {
		match &self.0 {
			BPIPaymentInstructions::ConfigurableAmount(_) => None,
			BPIPaymentInstructions::FixedAmount(fixed) => {
				fixed.ln_payment_amount().map(|amt| Arc::new(amt.into()))
			},
		}
	}

	/// Get the on-chain payment amount for fixed amount instructions
	///
	/// Returns the specific amount required for on-chain payments.
	/// Only applies to fixed amount payment instructions.
	pub fn onchain_payment_amount(&self) -> Option<Arc<Amount>> {
		match &self.0 {
			BPIPaymentInstructions::ConfigurableAmount(_) => None,
			BPIPaymentInstructions::FixedAmount(fixed) => {
				fixed.onchain_payment_amount().map(|amt| Arc::new(amt.into()))
			},
		}
	}
}

impl_from_core_type!(BPIPaymentInstructions, PaymentInstructions);
impl_into_core_type!(PaymentInstructions, BPIPaymentInstructions);

#[derive(Debug, uniffi::Error)]
pub enum PaymentInfoBuildError {
	AmountMismatch {
		given: Arc<Amount>,
		ln_amount: Option<Arc<Amount>>,
		onchain_amount: Option<Arc<Amount>>,
	},
	MissingAmount,
	AmountOutOfRange {
		given: Arc<Amount>,
		min: Option<Arc<Amount>>,
		max: Option<Arc<Amount>>,
	},
}

impl Display for PaymentInfoBuildError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			PaymentInfoBuildError::AmountMismatch { given, ln_amount, onchain_amount } => {
				write!(f, "Amount mismatch: given {}", given.milli_sats())?;
				if let Some(ln) = ln_amount {
					write!(f, ", lightning amount {}", ln.milli_sats())?;
				}
				if let Some(onchain) = onchain_amount {
					write!(f, ", on-chain amount {}", onchain.milli_sats())?;
				}
				Ok(())
			},
			PaymentInfoBuildError::MissingAmount => {
				write!(f, "Amount is required but was not provided")
			},
			PaymentInfoBuildError::AmountOutOfRange { given, min, max } => {
				write!(f, "Amount {} is out of range", given.milli_sats())?;
				if let Some(min_amt) = min {
					write!(f, ", minimum {}", min_amt.milli_sats())?;
				}
				if let Some(max_amt) = max {
					write!(f, ", maximum {}", max_amt.milli_sats())?;
				}
				Ok(())
			},
		}
	}
}

impl From<OrangePaymentInfoBuildError> for PaymentInfoBuildError {
	fn from(error: OrangePaymentInfoBuildError) -> Self {
		match error {
			OrangePaymentInfoBuildError::AmountMismatch { given, ln_amount, onchain_amount } => {
				PaymentInfoBuildError::AmountMismatch {
					given: Arc::new(given.into()),
					ln_amount: ln_amount.map(|amt| Arc::new(amt.into())),
					onchain_amount: onchain_amount.map(|amt| Arc::new(amt.into())),
				}
			},
			OrangePaymentInfoBuildError::MissingAmount => PaymentInfoBuildError::MissingAmount,
			OrangePaymentInfoBuildError::AmountOutOfRange { given, min, max } => {
				PaymentInfoBuildError::AmountOutOfRange {
					given: Arc::new(given.into()),
					min: min.map(|amt| Arc::new(amt.into())),
					max: max.map(|amt| Arc::new(amt.into())),
				}
			},
		}
	}
}

#[derive(Debug, Clone, uniffi::Object)]
pub struct PaymentInfo(pub OrangePaymentInfo);

#[uniffi::export]
impl PaymentInfo {
	/// Build a PaymentInfo from PaymentInstructions and an optional amount
	///
	/// For configurable amount instructions, an amount must be provided and must be within
	/// any specified range. For fixed amount instructions, the amount is optional and must
	/// match the fixed amount if provided.
	#[uniffi::constructor]
	pub fn build(
		instructions: Arc<PaymentInstructions>, amount: Option<Arc<Amount>>,
	) -> Result<Self, PaymentInfoBuildError> {
		let orange_amount = amount.map(|amt| amt.0);
		let result = OrangePaymentInfo::build(instructions.0.clone(), orange_amount)?;
		Ok(PaymentInfo(result))
	}

	/// Get the payment instructions
	pub fn instructions(&self) -> PaymentInstructions {
		self.0.instructions.clone().into()
	}

	/// Get the payment amount
	pub fn amount(&self) -> Amount {
		self.0.amount.into()
	}
}

impl_from_core_type!(OrangePaymentInfo, PaymentInfo);
impl_into_core_type!(PaymentInfo, OrangePaymentInfo);
