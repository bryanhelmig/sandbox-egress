#!/bin/sh
set -eu

required_version="0.9.0"

if ! version_output=$(cargo llvm-cov --version 2>/dev/null); then
    echo "error: cargo-llvm-cov ${required_version} is required" >&2
    echo "install: cargo install cargo-llvm-cov --version ${required_version} --locked" >&2
    echo "then: rustup component add llvm-tools-preview" >&2
    exit 2
fi

installed_version=$(printf '%s\n' "${version_output}" | awk '{print $2}')
if [ "${installed_version}" != "${required_version}" ]; then
    echo "error: cargo-llvm-cov ${required_version} is required; found ${installed_version}" >&2
    exit 2
fi

cargo llvm-cov --locked --workspace --all-features --summary-only
