#!/bin/sh
set -eu

container_rustc_version=$(rustc --version)
echo "container factory: ${container_rustc_version}"
case "${container_rustc_version}" in
    "rustc 1.88.0 "*) ;;
    *)
        echo "error: container factory must run rustc 1.88.0" >&2
        exit 1
        ;;
esac

./scripts/check.sh
./scripts/measure-resources.sh 250 2
