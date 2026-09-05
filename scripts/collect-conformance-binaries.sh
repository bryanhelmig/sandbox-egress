#!/bin/sh
set -eu

destination=${1:?usage: collect-conformance-binaries.sh DESTINATION}
mkdir -p "${destination}/bin"

copy_suite() {
    suite=$1
    target_kind=$2
    shift 2
    artifacts=$(cargo test --locked --no-run --message-format=json "$@" \
        | grep -F "\"kind\":[\"${target_kind}\"]" \
        | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p')
    artifact_count=$(printf '%s\n' "${artifacts}" | awk 'NF { count += 1 } END { print count + 0 }')
    if [ "${artifact_count}" -ne 1 ]; then
        echo "error: expected one ${suite} conformance binary, found ${artifact_count}" >&2
        exit 1
    fi
    cp "${artifacts}" "${destination}/bin/${suite}"
    strip "${destination}/bin/${suite}"
}

copy_suite sandbox_egress lib --lib
copy_suite benchmark_contract test --test benchmark_contract
copy_suite cli test --test cli
copy_suite concurrency test --test concurrency
copy_suite lifecycle test --test lifecycle
copy_suite tunneling test --test tunneling
cp target/debug/sandbox-egress "${destination}/sandbox-egress"
strip "${destination}/sandbox-egress"
cp scripts/run-container-conformance.sh "${destination}/run.sh"
