#!/usr/bin/env bash
set -euo pipefail

# Zero-fee commitment tests require Esplora's package broadcast endpoint.
# electrsd's downloadable Esplora revision does not implement it, so build the
# package-enabled revision used by ldk-node's zero-fee integration tests.
esplora_repo="https://github.com/tankyleo/blockstream-electrs.git"
esplora_tag="2026-05-26-electrum-submit-package"
esplora_rev="8c06d8010e43f793b1a65f83695ea846e5cd83ed"

host_platform="$(rustc --version --verbose | awk '/^host:/ { print $2 }')"
case "$host_platform" in
	*linux*|*darwin*) ;;
	*)
		echo "Unsupported platform: $host_platform" >&2
		exit 1
		;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output_path="${1:-$repo_root/target/package-esplora/electrs}"
build_root="$repo_root/target/package-esplora-build"
mkdir -p "$build_root"
build_dir="$(mktemp -d "$build_root/build.XXXXXXXX")"
trap 'rm -rf -- "$build_dir"' EXIT

git clone --branch "$esplora_tag" --depth 1 "$esplora_repo" "$build_dir/blockstream-electrs"

actual_rev="$(git -C "$build_dir/blockstream-electrs" rev-parse HEAD)"
if [[ "$actual_rev" != "$esplora_rev" ]]; then
	echo "Esplora revision mismatch: expected $esplora_rev, got $actual_rev" >&2
	exit 1
fi

RUSTFLAGS="" cargo build --release --manifest-path "$build_dir/blockstream-electrs/Cargo.toml"
mkdir -p "$(dirname "$output_path")"
install -m 755 "$build_dir/blockstream-electrs/target/release/electrs" "$output_path"
echo "Built package-enabled Esplora at $output_path"
