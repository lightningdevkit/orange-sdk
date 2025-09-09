#!/bin/sh
set -e
set -x

RUSTC_MINOR_VERSION=$(rustc --version | awk '{ split($2,a,"."); print a[2] }')

cargo clippy --features _test-utils -- -D warnings \
	`# We use this for sat groupings` \
	-A clippy::inconsistent-digit-grouping \
	`# Some stuff we do sometimes when its reasonable` \
	-A clippy::result-unit-err \
	-A clippy::large-enum-variant \
	-A clippy::if-same-then-else \
	-A clippy::drop-non-drop \
	-A clippy::manual-async-fn \
	-A clippy::needless-lifetimes \
	-A clippy::collapsible_if \
	`# This doesn't actually work sometimes` \
	-A clippy::option-as-ref-deref \
	`# TODO eventually remove this` \
	-A dead-code

# todo bring back RUSTDOCFLAGS="-D warnings" cargo doc --all --all-features --no-deps
