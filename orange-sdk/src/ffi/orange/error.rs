use crate::InitFailure as OrangeInitFailure;
use crate::WalletError as OrangeWalletError;
use std::fmt::Display;

#[derive(Debug, uniffi::Error)]
pub enum ConfigError {
	InvalidEntropySize(u32),
	InvalidMnemonic,
	InvalidLspAddress(String),
	InvalidLspNodeId(String),
}

impl Display for ConfigError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			ConfigError::InvalidEntropySize(size) => {
				write!(f, "Invalid entropy size: {size}. Entropy must be 64 bytes.")
			},
			ConfigError::InvalidMnemonic => {
				write!(f, "Invalid mnemonic phrase")
			},
			ConfigError::InvalidLspAddress(address) => write!(f, "Invalid LSP address: {address}"),
			ConfigError::InvalidLspNodeId(node_id) => write!(f, "Invalid LSP node ID: {node_id}"),
		}
	}
}

/// Represents possible failures during wallet initialization.
#[derive(Debug, uniffi::Error)]
pub enum InitFailure {
	/// I/O error during initialization.
	IoError(String),
	ConfigError(String),
	/// Failure to build the LDK node.
	LdkNodeBuildFailure(String),
	/// Failure to start the LDK node.
	LdkNodeStartFailure(String),
	/// Failure in the trusted wallet implementation.
	TrustedFailure,
}

impl Display for InitFailure {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			InitFailure::IoError(e) => write!(f, "I/O error: {e}"),
			InitFailure::ConfigError(e) => write!(f, "Config error: {e}"),
			InitFailure::LdkNodeBuildFailure(e) => write!(f, "Failed to build the LDK node: {e}"),
			InitFailure::LdkNodeStartFailure(e) => write!(f, "Failed to start the LDK node: {e}"),
			InitFailure::TrustedFailure => write!(f, "Failed to create the trusted wallet"),
		}
	}
}

impl From<std::io::Error> for InitFailure {
	fn from(e: std::io::Error) -> Self {
		InitFailure::IoError(e.to_string())
	}
}

impl From<OrangeInitFailure> for InitFailure {
	fn from(e: OrangeInitFailure) -> Self {
		match e {
			OrangeInitFailure::IoError(e) => InitFailure::IoError(e.to_string()),
			OrangeInitFailure::LdkNodeBuildFailure(e) => {
				InitFailure::LdkNodeBuildFailure(e.to_string())
			},
			OrangeInitFailure::LdkNodeStartFailure(e) => {
				InitFailure::LdkNodeStartFailure(e.to_string())
			},
			OrangeInitFailure::TrustedFailure(_e) => InitFailure::TrustedFailure,
		}
	}
}

impl From<ConfigError> for InitFailure {
	fn from(e: ConfigError) -> Self {
		InitFailure::ConfigError(e.to_string())
	}
}

/// Represents possible errors during wallet operations.
#[derive(Debug, uniffi::Error)]
pub enum WalletError {
	/// Failure in the LDK node.
	LdkNodeFailure(String),
	/// Failure in the trusted wallet implementation.
	TrustedFailure,
	/// Failure to parse payment instructions.
	PaymentInstructionsParseError,
	/// Failure to build payment info.
	PaymentInfoBuildError,
}

impl Display for WalletError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			WalletError::LdkNodeFailure(e) => write!(f, "Failure from LDK node: {e}"),
			WalletError::TrustedFailure => write!(f, "Failure from trusted wallet"),
			WalletError::PaymentInstructionsParseError => {
				write!(f, "Failure to parse payment instructions")
			},
			WalletError::PaymentInfoBuildError => write!(f, "Failure to build payment info"),
		}
	}
}

impl From<OrangeWalletError> for WalletError {
	fn from(e: OrangeWalletError) -> Self {
		match e {
			OrangeWalletError::LdkNodeFailure(e) => WalletError::LdkNodeFailure(e.to_string()),
			OrangeWalletError::TrustedFailure(_e) => WalletError::TrustedFailure,
		}
	}
}

impl From<bitcoin_payment_instructions::ParseError> for WalletError {
	fn from(_e: bitcoin_payment_instructions::ParseError) -> Self {
		WalletError::PaymentInstructionsParseError
	}
}

impl From<crate::PaymentInfoBuildError> for WalletError {
	fn from(_e: crate::PaymentInfoBuildError) -> Self {
		WalletError::PaymentInfoBuildError
	}
}
