#!/bin/sh
set -eu

total_mebibytes="${1:-1024}"
runs="${2:-3}"
concurrencies="${SANDBOX_EGRESS_THROUGHPUT_CONCURRENCIES:-1 2 4 8 16 32}"
directions="${SANDBOX_EGRESS_THROUGHPUT_DIRECTIONS:-upload download}"

for concurrency in ${concurrencies}; do
    if [ $((total_mebibytes % concurrency)) -ne 0 ]; then
        echo "total MiB must be divisible by concurrency: ${total_mebibytes} % ${concurrency}" >&2
        exit 2
    fi
    mebibytes_per_tunnel=$((total_mebibytes / concurrency))
    for direction in ${directions}; do
        run=1
        while [ "${run}" -le "${runs}" ]; do
            echo "throughput-sweep concurrency=${concurrency} direction=${direction} run=${run}"
            ./scripts/measure-throughput.sh \
                "${mebibytes_per_tunnel}" "${concurrency}" "${direction}"
            run=$((run + 1))
        done
    done
done
