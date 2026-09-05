#!/bin/sh
set -eu

cargo test --locked --lib
cargo test --locked --test benchmark_contract --test cli --test lifecycle --test concurrency --test tunneling -- --test-threads=1
