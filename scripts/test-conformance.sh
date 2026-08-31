#!/bin/sh
set -eu

cargo test --lib
cargo test --test lifecycle --test concurrency --test tunneling -- --test-threads=1
