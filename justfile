default:
    @just --list

test *args:
    cargo test {{args}} --features _test-utils -p orange-sdk

cli:
    cd examples/cli && cargo run

cli-logs:
    tail -n 50 -f examples/cli/wallet_data/bitcoin/wallet.log
