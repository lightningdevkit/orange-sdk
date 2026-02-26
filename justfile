default:
    @just --list

test *args:
    #!/usr/bin/env bash
    THREADS=$(($(nproc) / 2))
    if [ $THREADS -lt 1 ]; then THREADS=1; fi
    cargo test {{ args }} --features _test-utils -p orange-sdk -- --test-threads=$THREADS

test-cashu *args:
    #!/usr/bin/env bash
    THREADS=$(($(nproc) / 2))
    if [ $THREADS -lt 1 ]; then THREADS=1; fi
    cargo test {{ args }} --features _cashu-tests -p orange-sdk -- --test-threads=$THREADS

cli:
    cd examples/cli && cargo run

cli-cashu *args:
    cd examples/cli && cargo run -- --cashu --npubcash-url https://npubx.cash --mint-url {{ args }}

cli-logs:
    tail -n 50 -f examples/cli/wallet_data/bitcoin/wallet.log

build-android:
    ./scripts/uniffi_bindgen_generate_kotlin_android.sh
    cd bindings/kotlin/orange-sdk-android/ && ./gradlew build
    ./scripts/create_android_maven_package.sh
