#!/bin/sh
set -eu

runs_per_batch=${1:-2000}
batches=${2:-4}

SANDBOX_EGRESS_SOAK_RUNS=$runs_per_batch \
SANDBOX_EGRESS_SOAK_BATCHES=$batches \
  cargo test --release --test resource_soak -- --ignored --nocapture --test-threads=1
