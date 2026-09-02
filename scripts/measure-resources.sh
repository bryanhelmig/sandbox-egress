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

run_lane() {
  SANDBOX_EGRESS_SOAK_RUNS=$runs_per_batch \
  SANDBOX_EGRESS_SOAK_BATCHES=$batches \
  SANDBOX_EGRESS_IDLE_CONNECTIONS=$idle_connections \
  SANDBOX_EGRESS_TLS_CONNECTIONS=$tls_connections \
  SANDBOX_EGRESS_TERMINAL_RUNS=$terminal_runs_per_batch \
  SANDBOX_EGRESS_TERMINAL_BATCHES=$terminal_batches \
  SANDBOX_EGRESS_HEADER_CONNECTIONS=$header_connections \
  SANDBOX_EGRESS_UPSTREAM_CONNECTIONS=$upstream_connections \
    cargo test --locked --release --test resource_soak "$1" -- \
      --ignored --nocapture --exact
}

# A fresh process gives every lane its own allocator and RSS high-water mark.
# Keep the calls serial so opt-in measurements do not compete for host resources.
run_lane identity_churn_has_bounded_process_resources
run_lane concurrent_management_churn_releases_process_resources
run_lane concurrent_idle_expiry_releases_process_resources
run_lane concurrent_partial_client_hellos_release_process_resources
run_lane concurrent_partial_headers_release_process_resources
run_lane concurrent_partial_upstream_responses_release_process_resources
run_lane repeated_bidirectional_backpressure_releases_process_resources
run_lane terminal_connection_churn_releases_process_resources
