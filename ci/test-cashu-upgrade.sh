#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
scratch=$(mktemp -d "${TMPDIR:-/tmp}/orange-cdk016.XXXXXXXX")
trap 'rm -rf "$scratch"' EXIT

# Always compile the old adapter, never the adapter under test. CI must fetch
# history containing this revision (actions/checkout with fetch-depth: 0).
old_revision=a3aa1c592625c45314004987430498307b9a1819
for source in cashu_store dyn_store; do
    if [[ "$source" == cashu_store ]]; then
        source_path=orange-sdk/src/trusted_wallet/cashu/cashu_store.rs
    else
        source_path=orange-sdk/src/dyn_store.rs
    fi
    if [[ -d .jj ]]; then
        jj --ignore-working-copy file show -r "$old_revision" "$source_path" > "$scratch/$source.rs"
    else
        git show "$old_revision:$source_path" > "$scratch/$source.rs"
    fi
done
# include! does not accept inner module documentation comments.
sed -i '/^\/\/!/d' "$scratch/dyn_store.rs"
export ORANGE_OLD_CASHU_STORE="$scratch/cashu_store.rs"
export ORANGE_OLD_DYN_STORE="$scratch/dyn_store.rs"

cargo build --manifest-path orange-sdk/tests/fixtures/cdk016-generator/Cargo.toml \
    --message-format=json > "$scratch/build.json"
# Cargo reports the actual executable, honoring configured target directories.
ORANGE_CDK016_GENERATOR=$(python3 - "$scratch/build.json" <<'PY'
import json
import sys

with open(sys.argv[1]) as output:
    binaries = [message['executable'] for line in output
                if (message := json.loads(line)).get('reason') == 'compiler-artifact'
                and message.get('executable')
                and message['target']['name'] == 'orange-cdk016-fixture']
assert len(binaries) == 1, f'Expected one fixture generator, got {binaries}'
print(binaries[0])
PY
)
export ORANGE_CDK016_GENERATOR
test -x "$ORANGE_CDK016_GENERATOR"
cargo test -p orange-sdk --no-default-features --features _cashu-tests \
    --lib cdk016_funded_wallet -- --ignored --nocapture --test-threads=2
