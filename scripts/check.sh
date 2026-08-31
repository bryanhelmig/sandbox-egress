#!/bin/sh
set -eu

cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo package --allow-dirty --no-verify >/dev/null

if command -v cargo-deny >/dev/null 2>&1; then
  cargo deny check
else
  echo "note: cargo-deny is not installed; dependency policy check skipped" >&2
fi

