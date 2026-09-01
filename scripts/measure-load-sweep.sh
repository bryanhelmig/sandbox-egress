#!/bin/sh
set -eu

connections="${1:-10000}"
destinations="${2:-16}"
runs="${3:-3}"
concurrencies="${SANDBOX_EGRESS_LOAD_CONCURRENCIES:-1 8 32 64 128 256}"

for concurrency in ${concurrencies}; do
    run=1
    while [ "${run}" -le "${runs}" ]; do
        echo "sweep concurrency=${concurrency} run=${run}"
        ./scripts/measure-load.sh "${connections}" "${concurrency}" "${destinations}"
        run=$((run + 1))
    done
done
