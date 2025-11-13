# orange-sdk

`orange-sdk` is the simplest library for integrating Bitcoin and Lightning capabilities into your applications. It implements a graduated custody model that seamlessly combines trusted custody for small balances with self-custodial Lightning channels for larger amounts.

## Key Features

*   **Graduated Custody Model:** Small balances are held in a trusted wallet backend (e.g., Spark, Cashu) for instant, low-fee transactions, while larger balances automatically move to self-custodial Lightning channels
*   **Automatic Rebalancing:** Intelligently opens Lightning channels and moves funds into self-custody based on configurable thresholds
*   **Unified Balance:** Despite funds being in multiple places, everything is accessible through a single wallet API
*   **Smart Payment Routing:** Automatically selects the best funding source (trusted or Lightning) with fallbacks
*   **Event-Driven:** Asynchronous event system for payment notifications and channel updates

## How it Works

The wallet automatically manages fund distribution based on configurable thresholds:

1. **Receiving payments**: Small amounts go to the trusted wallet, larger amounts go to Lightning channels or on-chain addresses
2. **Automatic rebalancing**: When the trusted balance exceeds `trusted_balance_limit`, funds are automatically transferred to Lightning channels
3. **Sending payments**: The wallet intelligently selects the best funding source with automatic fallbacks

### Payment Flow

```text
                             ┌─────────────┐
                             │ Third Party │
                             └──────┬──────┘
                                    │
                ┌───────────────────┼───────────────────┐
                │                   │                   │
                │                   │                   │
   under trusted_balance_limit      │       over onchain_receive_threshold
                │           over trusted_balance_limit  │
                │                   │                   │
                │                   │                   │
                ▼                   ▼                   ▼
        ┌──────────────┐    ┌──────────────┐   ┌──────────────┐
        │   Trusted    │◄───│  LN Channel  │◄──│   Onchain    │
        │   Wallet     │    │              │   │   Wallet     │
        └──────────────┘    └──────┬───────┘   └──────────────┘
                                   │                   │
                                   │   splice in,      │
                        channel    │   when over       │
                        operations │   threshold       │
                        (bidir)    │                   │
                                   │                   │
                                   │                   │
                                   └───────────────────┘

Legend:
  - Payments from Third Party route based on amount thresholds
  - Onchain funds can splice into LN Channel when rebalance is enabled
  - Onchain funds below threshold go to Trusted Wallet
  - Channel force closes pause rebalances to avoid channel open and close loops
```

## Getting Started

Add orange-sdk to your `Cargo.toml`:

```toml
[dependencies]
orange-sdk = "0.1"
```

### Basic Example

```rust
use orange_sdk::bitcoin::Network;
use orange_sdk::bitcoin_payment_instructions::amount::Amount;
use orange_sdk::trusted_wallet::spark::SparkWalletConfig;
use orange_sdk::{ChainSource, StorageConfig, Tunables, Wallet, WalletConfig};
use orange_sdk::{ExtraConfig, LoggerType, Mnemonic, Seed};
use std::str::FromStr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure the wallet
    let config = WalletConfig {
        storage_config: StorageConfig::LocalSQLite("./wallet".to_string()),
        logger_type: LoggerType::LogFacade,
        chain_source: ChainSource::Electrum(
            "ssl://electrum.blockstream.info:60002".to_string(),
        ),
        lsp: ("127.0.0.1:9735".parse()?, "03abcd...".parse()?, None),
        scorer_url: None,
        rgs_url: None,
        network: Network::Bitcoin,
        seed: Seed::Mnemonic {
            mnemonic: Mnemonic::from_str("your mnemonic words here")?,
            passphrase: None,
        },
        tunables: Tunables {
            trusted_balance_limit: Amount::from_sats(100_000)?,
            rebalance_min: Amount::from_sats(5_000)?,
            onchain_receive_threshold: Amount::from_sats(10_000)?,
            enable_amountless_receive_on_chain: true,
        },
        extra_config: ExtraConfig::Spark(SparkWalletConfig::default()),
    };

    // Initialize the wallet
    let wallet = Wallet::new(config).await?;

    // Check balance
    let balance = wallet.get_balance().await?;
    println!("Available: {}", balance.available_balance());
    println!("Trusted: {}", balance.trusted);
    println!("Lightning: {}", balance.lightning);
    println!("Pending: {}", balance.pending_balance);

    // Generate a receive URI
    let uri = wallet.get_single_use_receive_uri(Some(Amount::from_sats(50_000)?)).await?;
    println!("Pay me: {}", uri);

    // Parse and pay an invoice
    let instructions = wallet.parse_payment_instructions("lnbc...").await?;
    let payment_info = orange_sdk::PaymentInfo::build(instructions, Some(Amount::from_sats(1_000)?))?;
    wallet.pay(&payment_info).await?;

    // Listen for events
    loop {
        let event = wallet.next_event_async().await;
        println!("Event: {:?}", event);
        wallet.event_handled()?;
    }
}
```

## Configuration

The wallet's behavior is controlled by `Tunables`:

- **`trusted_balance_limit`**: Maximum balance in trusted wallet before triggering rebalance (default: 100,000 sats)
- **`rebalance_min`**: Minimum amount to rebalance, avoids unnecessary small transfers (default: 5,000 sats)
- **`onchain_receive_threshold`**: Amount above which on-chain addresses are included in payment URIs (default: 10,000 sats)
- **`enable_amountless_receive_on_chain`**: Whether to include on-chain addresses for variable amount receives (default: true)

## Supported Backends

### Trusted Wallet Backends
- **Spark** - Custodial Lightning wallet (default)
- **Cashu** - Ecash-based wallet

### Chain Sources
- Electrum servers
- Esplora servers (with optional Basic auth)
- Bitcoin Core RPC

## Documentation

For detailed API documentation, run:

```bash
cargo doc --open
```

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

*(Specify the license for the project)*
