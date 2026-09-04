#!/bin/sh
set -eu

if ! command -v scc >/dev/null 2>&1; then
    echo "error: scc 4.0.0 is required; see docs/complexity.md" >&2
    exit 2
fi

if [ "$#" -eq 0 ]; then
    set -- src tests benches
fi

scc_version=$(scc --version)
if [ "$scc_version" != "scc version 4.0.0" ]; then
    echo "error: scc 4.0.0 is required; found $scc_version" >&2
    exit 2
fi
printf '%s\n' "$scc_version"
echo "structural complexity estimate"
scc --ci --no-config --no-cocomo --by-file --sort complexity --include-ext rs "$@"
echo "cognitive complexity estimate"
scc --ci --no-config --no-cocomo --cognitive --by-file --sort complexity --include-ext rs "$@"
