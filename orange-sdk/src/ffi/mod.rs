mod bitcoin_payment_instructions;
mod cashu;
mod ldk_node;
mod macros;
mod orange;
mod spark;

#[derive(Clone, Debug, Eq, PartialEq, uniffi::Enum)]
pub enum Network {
	Mainnet,
	Regtest,
	Testnet,
	Signet,
}
