#!/bin/sh
set -eu

SANDBOX_EGRESS_LOAD_CONNECTIONS="${1:-5000}" \
SANDBOX_EGRESS_LOAD_CONCURRENCY="${2:-64}" \
SANDBOX_EGRESS_LOAD_DESTINATIONS="${3:-16}" \
    cargo test --locked --release --test load -- --ignored --nocapture
