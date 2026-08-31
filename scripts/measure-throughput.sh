#!/bin/sh
set -eu

mebibytes="${1:-32}"
concurrency="${2:-8}"
direction="${3:-both}"

run_direction() {
    SANDBOX_EGRESS_THROUGHPUT_MIB="${mebibytes}" \
    SANDBOX_EGRESS_THROUGHPUT_CONCURRENCY="${concurrency}" \
    SANDBOX_EGRESS_THROUGHPUT_DIRECTION="$1" \
        cargo test --release --test throughput -- --ignored --nocapture
}

case "${direction}" in
    upload|download) run_direction "${direction}" ;;
    both)
        run_direction upload
        run_direction download
        ;;
    *)
        echo "usage: $0 [MiB per tunnel] [concurrency] [upload|download|both]" >&2
        exit 2
        ;;
esac
