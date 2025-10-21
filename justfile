default:
    @just --list

test *args:
    cargo test {{ args }} --features _test-utils -p orange-sdk -- --nocapture

test-cashu *args:
    cargo test {{ args }} --features _cashu-tests -p orange-sdk

cli:
    cd examples/cli && cargo run

cli-logs:
    tail -n 50 -f examples/cli/wallet_data/bitcoin/wallet.log

build-android:
    ./scripts/uniffi_bindgen_generate_kotlin_android.sh
    cd bindings/kotlin/orange-sdk-android/ && ./gradlew build
    ./scripts/create_android_maven_package.sh
