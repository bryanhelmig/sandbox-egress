#!/bin/sh
set -eu

runs_per_batch=${1:-2000}
batches=${2:-4}
idle_connections=${3:-128}
tls_connections=${4:-64}

SANDBOX_EGRESS_SOAK_RUNS=$runs_per_batch \
SANDBOX_EGRESS_SOAK_BATCHES=$batches \
SANDBOX_EGRESS_IDLE_CONNECTIONS=$idle_connections \
SANDBOX_EGRESS_TLS_CONNECTIONS=$tls_connections \
  cargo test --release --test resource_soak -- --ignored --nocapture --test-threads=1
