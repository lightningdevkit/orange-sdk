# CDK 0.16 upgrade fixtures

`../cdk016-wallet.json` is an export of the raw KV values written by Orange's
original `CashuKvDatabase` at commit `a3aa1c59`, compiled against CDK **0.16.0**.
Values are UTF-8 strings so tests can compare the exact persisted bytes. All
secrets, invoices and proofs in this static fixture are synthetic test values;
they do not represent funds. The generator calls the old adapter's write methods,
including its original proof-reservation implementation, rather than hand-writing
serialized records.

The standalone generator is excluded from the main Cargo workspace. It takes the
old adapter sources through build-time environment variables, preventing an
accidental test against the new adapter. Cargo's configured target/cache settings
still apply.

From the repository root, using the repository's Rust build environment:

```sh
scratch=$(mktemp -d /data/rust/tmp/orange-cdk016.XXXXXXXX)
jj file show -r a3aa1c59 orange-sdk/src/trusted_wallet/cashu/cashu_store.rs > "$scratch/cashu_store.rs"
jj file show -r a3aa1c59 orange-sdk/src/dyn_store.rs > "$scratch/dyn_store.rs"
# include! does not accept the old module's inner documentation comments.
sed -i '/^\/\/!/d' "$scratch/dyn_store.rs"
export ORANGE_OLD_CASHU_STORE="$scratch/cashu_store.rs"
export ORANGE_OLD_DYN_STORE="$scratch/dyn_store.rs"
cargo run --manifest-path orange-sdk/tests/fixtures/cdk016-generator/Cargo.toml -- "$scratch/database" > orange-sdk/tests/fixtures/cdk016-wallet.json
```

The generator also supports `DATABASE_DIR MINT_URL INVOICE [submit]`. It creates
an actual old-version wallet, mints 64 sats, and prepares an 8-sat melt. Without
`submit` it exits without destructor cleanup. With `submit`, it confirms the
payment; the integration test holds the mint's response and kills the process
before that response reaches the old wallet. These tests use real Cashu
signatures from a local CDK mint and a fake Lightning backend. Passing `issue`
instead of `INVOICE` lets the test hold the initial mint response and kill the
process with its issue saga still in `MintRequested`.

All upgrades go through Orange's production `Cashu::init`, with the seed-restore
flag already set and Orange's `Seed64([7; 64])` derivation. The suite verifies:

- A prepared melt releases its reservation, and the upgraded wallet can spend.
- A paid melt with a lost response recovers its change and exactly one completed
  transaction. Paying 8 sats plus a 1-sat fee leaves 55 sats; the 2-sat fee reserve
  requires recovering 1 sat of change.
- A failed melt returns the full 64-sat balance without a completed transaction.
- A pending melt keeps its 10 sats reserved across repeated startup attempts.
- HTTP 503 responses during recovery preserve the reservation, and reopening
  once the mint is available recovers the paid melt and change.
- An issue saga with a lost mint response does not block startup while the mint
  returns HTTP 503; reopening online recovers all 64 sats exactly once.

Repeated startup must neither resubmit the melt nor duplicate payment history.

```sh
bash ci/test-cashu-upgrade.sh
```

Run this from the repository root in the Rust development environment. It builds
the generator from the pinned old commit and discovers Cargo's actual executable
path, honoring configured target directories. It uses Git to read the pinned sources;
the Git checkout (including a colocated JJ checkout) must contain commit `a3aa1c592625c45314004987430498307b9a1819`. These
tests remain ignored in ordinary `cargo test` runs because they need that separate
generator, but the Cashu CI job explicitly runs this script on every build. CI caches
both Cargo workspaces, including the generator's separate CDK 0.16 dependency tree;
a cold build still needs to compile both versions.

The ordinary Cashu unit suite runs the exported-data and before/after I/O failure
tests without this executable or any external services. The asynchronous fault
wrapper covers lost acknowledgements, but is not a deployed VSS-server test.
The adapter requires one active instance per store; these tests do not establish
multi-process or multi-device transactional guarantees.
