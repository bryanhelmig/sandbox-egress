#!/bin/sh
set -eu

cargo test --lib dns_
cargo test --test lifecycle --test concurrency --test tunneling -- --test-threads=1
