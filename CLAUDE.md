# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`orange-sdk` is a Rust library for Bitcoin and Lightning wallets that implements a hybrid custody model. It starts with
a trusted wallet backend (currently Spark) for small balances and automatically transitions to self-custody Lightning
channels managed by LDK Node when balances exceed configurable thresholds.

## Development Commands

### Build and Test

- `cargo build` - Build the project
- `just test` - Run tests (equivalent to `cargo test --features _test-utils`)
- `just` - List available commands

### Lint and Format

- `./ci/check-lint.sh` - Run comprehensive linting with clippy
- `cargo fmt` - Format code using rustfmt
- `cargo doc --all-features` - Generate documentation

### Cargo Features

- `_test-utils` - Enable test utilities and dummy implementations (dev only)

## Architecture Overview

### Core Components

1. **Wallet (`src/lib.rs`)** - Main entry point that orchestrates both trusted and Lightning wallets
2. **LightningWallet (`src/lightning_wallet.rs`)** - Wraps LDK Node for Lightning/on-chain operations
3. **TrustedWalletInterface (`src/trusted_wallet/mod.rs`)** - Trait for trusted backends with Spark implementation
4. **EventHandler (`src/event.rs`)** - Manages wallet events and notifications
5. **Store (`src/store.rs`)** - Persistent storage for transaction metadata

### Key Design Patterns

- **Hybrid Architecture**: Seamlessly combines trusted and self-custody wallets
- **Automatic Rebalancing**: Background process moves funds from trusted to Lightning based on `Tunables`
- **Event-Driven**: Asynchronous event system for payment notifications and channel updates
- **Configurable Thresholds**: `Tunables` struct controls when to use trusted vs Lightning wallets

### Payment Flow

1. Small amounts (< `trusted_balance_limit`) use trusted wallet
2. Larger amounts use Lightning channels
3. Automatic rebalancing opens channels when trusted balance exceeds limits
4. On-chain fallback available for both receiving and sending

### Configuration

- `WalletConfig` defines network, LSP, chain source, and storage
- `Tunables` control balance thresholds and rebalancing behavior
- `ChainSource` supports Electrum, Esplora, and Bitcoin Core RPC
- `StorageConfig` currently supports local SQLite

## Testing

Integration tests in `tests/integration_tests.rs` use:

- `build_test_nodes()` for test setup with dummy trusted wallet
- `TestParams` struct containing wallet, LSP, bitcoind, and third-party nodes
- Feature flag `_test-utils` enables test-only code paths

## Key Dependencies

- `ldk-node` - Lightning Development Kit node implementation
- `spark-rust` - Spark trusted wallet backend
- `bitcoin-payment-instructions` - Payment instruction parsing
- `tokio` - Async runtime