#!/bin/sh
set -eu

cargo fmt --all -- --check
cargo check --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
cargo package --locked --allow-dirty >/dev/null

if cargo deny --version >/dev/null 2>&1; then
  cargo deny check
else
  echo "note: cargo-deny is not installed; dependency policy check skipped" >&2
fi
