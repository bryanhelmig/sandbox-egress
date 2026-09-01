#!/bin/sh
set -eu

mebibytes="${1:-32}"
concurrency="${2:-8}"
direction="${3:-both}"
idle_timeout_ms="${4:-0}"

run_direction() {
    SANDBOX_EGRESS_THROUGHPUT_MIB="${mebibytes}" \
    SANDBOX_EGRESS_THROUGHPUT_CONCURRENCY="${concurrency}" \
    SANDBOX_EGRESS_THROUGHPUT_DIRECTION="$1" \
    SANDBOX_EGRESS_THROUGHPUT_IDLE_MS="${idle_timeout_ms}" \
        cargo test --locked --release --test throughput -- --ignored --nocapture
}

case "${direction}" in
    upload|download) run_direction "${direction}" ;;
    both)
        run_direction upload
        run_direction download
        ;;
    *)
        echo "usage: $0 [MiB per tunnel] [concurrency] [upload|download|both] [idle timeout ms]" >&2
        exit 2
        ;;
esac
