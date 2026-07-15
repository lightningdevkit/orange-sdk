vss_url := env_var_or_default("VSS_URL", "http://127.0.0.1:6754/vss")
cashu_test_threads := env_var_or_default("CASHU_TEST_THREADS", "4")
cashu_repro_receives := env_var_or_default("CASHU_REPRO_RECEIVES", "10")
cashu_repro_restarts := env_var_or_default("CASHU_REPRO_RESTARTS", "5")
cashu_repro_timeout_secs := env_var_or_default("CASHU_REPRO_TIMEOUT_SECS", "900")

default:
    @just --list

test *args:
    #!/usr/bin/env bash
    THREADS=$(($(nproc) / 2))
    if [ $THREADS -lt 1 ]; then THREADS=1; fi
    cargo test {{ args }} --features _test-utils -p orange-sdk -- --test-threads=$THREADS

# Run the integration tests against the local VSS server.
test-vss *args:
    #!/usr/bin/env bash
    THREADS=$(($(nproc) / 2))
    if [ $THREADS -lt 1 ]; then THREADS=1; fi
    ORANGE_TEST_VSS_URL={{ vss_url }} cargo test {{ args }} --features _test-utils -p orange-sdk -- --test-threads=$THREADS

test-cashu *args:
    cargo test {{ args }} --features _cashu-tests -p orange-sdk -- --test-threads={{ cashu_test_threads }}

# Run the Cashu integration tests against the local VSS server.
test-cashu-vss *args:
    ORANGE_TEST_VSS_URL={{ vss_url }} cargo test {{ args }} --features _cashu-tests -p orange-sdk -- --test-threads={{ cashu_test_threads }}

# Populate and restart a Cashu wallet using SQLite, printing performance measurements.
repro-cashu-cold-start:
    ORANGE_TEST_CASHU_RECEIVES={{ cashu_repro_receives }} ORANGE_TEST_CASHU_RESTARTS={{ cashu_repro_restarts }} ORANGE_TEST_TIMEOUT_SECS={{ cashu_repro_timeout_secs }} cargo test test_cashu_populated_wallet_cold_start --features _cashu-tests -p orange-sdk -- --ignored --nocapture --test-threads=1

# Populate and restart a Cashu wallet using VSS, printing performance measurements.
repro-cashu-cold-start-vss:
    ORANGE_TEST_VSS_URL={{ vss_url }} ORANGE_TEST_CASHU_RECEIVES={{ cashu_repro_receives }} ORANGE_TEST_CASHU_RESTARTS={{ cashu_repro_restarts }} ORANGE_TEST_TIMEOUT_SECS={{ cashu_repro_timeout_secs }} cargo test test_cashu_populated_wallet_cold_start --features _cashu-tests -p orange-sdk -- --ignored --nocapture --test-threads=1

cli:
    cd examples/cli && cargo run

cli-cashu *args:
    cd examples/cli && cargo run -- --cashu --npubcash-url https://npubx.cash --mint-url {{ args }}

# Run the Cashu CLI against the local VSS server.
cli-cashu-vss *args:
    cd examples/cli && cargo run -- --cashu --vss-url {{ vss_url }} --npubcash-url https://npubx.cash --mint-url {{ args }}

cli-logs:
    tail -n 50 -f examples/cli/wallet_data/bitcoin/wallet.log

# Start a local VSS server and its PostgreSQL database.
vss-server:
    docker compose --file docker-compose.vss.yml up --build

# Run the CLI against the local VSS server.
cli-vss *args:
    cd examples/cli && cargo run -- --vss-url {{ vss_url }} {{ args }}

build-android:
    ./scripts/uniffi_bindgen_generate_kotlin_android.sh
    cd bindings/kotlin/orange-sdk-android/ && ./gradlew build
    ./scripts/create_android_maven_package.sh
