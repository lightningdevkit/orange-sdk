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

```rust,no_run
use orange_sdk::bitcoin::Network;
use orange_sdk::bitcoin_payment_instructions::amount::Amount;
use orange_sdk::trusted_wallet::spark::SparkWalletConfig;
use orange_sdk::{ChainSource, StorageConfig, Tunables, Wallet, WalletConfig};
use orange_sdk::{ExtraConfig, LoggerType, Mnemonic, Seed};
use std::str::FromStr;

#[tokio::main]
async fn main() {
    // Configure the wallet
    let config = WalletConfig {
        storage_config: StorageConfig::LocalSQLite("./wallet".to_string()),
        logger_type: LoggerType::LogFacade,
        chain_source: ChainSource::Electrum(
            "ssl://electrum.blockstream.info:60002".to_string(),
        ),
        lsp: ("127.0.0.1:9735".parse().unwrap(), "03abcd...".parse().unwrap(), None),
        scorer_url: None,
        rgs_url: None,
        network: Network::Bitcoin,
        seed: Seed::Mnemonic {
            mnemonic: Mnemonic::from_str("your mnemonic words here").unwrap(),
            passphrase: None,
        },
        tunables: Tunables {
            trusted_balance_limit: Amount::from_sats(100_000).unwrap(),
            rebalance_min: Amount::from_sats(5_000).unwrap(),
            onchain_receive_threshold: Amount::from_sats(10_000).unwrap(),
            enable_amountless_receive_on_chain: true,
        },
        extra_config: ExtraConfig::Spark(SparkWalletConfig::default()),
    };

    // Initialize the wallet
    let wallet = Wallet::new(config).await.unwrap();

    // Check balance
    let balance = wallet.get_balance().await.unwrap();
    println!("Available: {:?}", balance.available_balance());
    println!("Trusted: {:?}", balance.trusted);
    println!("Lightning: {:?}", balance.lightning);
    println!("Pending: {:?}", balance.pending_balance);

    // Generate a receive URI
    let uri = wallet.get_single_use_receive_uri(Some(Amount::from_sats(50_000).unwrap())).await.unwrap();
    println!("Pay me: {}", uri);

    // Parse and pay an invoice
    let instructions = wallet.parse_payment_instructions("lnbc...").await.unwrap();
    let payment_info = orange_sdk::PaymentInfo::build(instructions, Some(Amount::from_sats(1_000).unwrap())).unwrap();
    wallet.pay(&payment_info).await.unwrap();

    // Listen for events
    loop {
        let event = wallet.next_event_async().await;
        println!("Event: {:?}", event);
        wallet.event_handled().unwrap();
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

### Upgrading Cashu wallets from CDK 0.16 to 0.18.3

Orange stores Cashu wallet records in its own KV namespaces within `orange.sqlite`
(or the configured remote store), rather than CDK's SQL wallet tables. On startup,
it automatically rekeys existing saga transactions to CDK's new transaction IDs
before wallet recovery runs. The migration copies before deleting, resumes after
interruption, and rejects conflicting records without overwriting them. Orange's
public payment IDs remain based on quote IDs.

Existing proofs, quotes, keysets, and in-flight sagas remain readable. CDK supplies
serde defaults for the new fields and accepts the old `payment_preimage` field.
Existing transactions default to completed; new transactions preserve their
pending/completed/failed status. A separate persistent derivation counter is
created as needed, leaving existing keyset counters and quote signing keys intact.
Legacy proof reservation indexes remain readable during recovery.

Startup runs saga recovery before starting payment tasks, on every open (including
wallets already restored from seed). Database initialization errors or an unreadable
recovery queue stop initialization. Individual failed recoveries are logged and
startup continues, so a mint outage does not block access to the rest of the wallet.
Unresolved operations retain their reservations and are retried on the next startup.

Back up the complete store before upgrading and open it with only one active
wallet instance. Downgrading an upgraded store is not supported; restore the
pre-upgrade backup if a rollback is needed. The test mint uses an ephemeral CDK
SQLite database; Orange does not manage persistent mint databases or `cdk-mintd`
configuration. Operators of a separate mint must follow the upstream
[CDK migration guide](https://github.com/cashubtc/cdk/blob/v0.18.2/docs/migrations/v0.18.md).

See the [CDK 0.18.2 release notes](https://github.com/cashubtc/cdk/releases/tag/v0.18.2)
and the upstream [transaction rekey migration](https://github.com/cashubtc/cdk/blob/v0.18.2/crates/cdk-sql-common/src/wallet/migrations/sqlite/20260726000000_rekey_saga_transactions.sql).

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

See [LICENSE](LICENSE.md) for details.
