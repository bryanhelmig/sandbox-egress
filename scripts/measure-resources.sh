#!/bin/sh
set -eu

runs_per_batch=${1:-2000}
batches=${2:-4}
idle_connections=${3:-128}
tls_connections=${4:-64}
terminal_runs_per_batch=${5:-500}
terminal_batches=${6:-4}
header_connections=${7:-128}
upstream_connections=${8:-128}

SANDBOX_EGRESS_SOAK_RUNS=$runs_per_batch \
SANDBOX_EGRESS_SOAK_BATCHES=$batches \
SANDBOX_EGRESS_IDLE_CONNECTIONS=$idle_connections \
SANDBOX_EGRESS_TLS_CONNECTIONS=$tls_connections \
SANDBOX_EGRESS_TERMINAL_RUNS=$terminal_runs_per_batch \
SANDBOX_EGRESS_TERMINAL_BATCHES=$terminal_batches \
SANDBOX_EGRESS_HEADER_CONNECTIONS=$header_connections \
SANDBOX_EGRESS_UPSTREAM_CONNECTIONS=$upstream_connections \
  cargo test --locked --release --test resource_soak -- --ignored --nocapture --test-threads=1
