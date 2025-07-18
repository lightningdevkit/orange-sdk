default:
    @just --list

test *args:
    cargo test {{args}} --features _test-utils -p orange-sdk
