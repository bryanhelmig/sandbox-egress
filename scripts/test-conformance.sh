#!/bin/sh
set -eu

cargo test --lib
cargo test --test cli --test lifecycle --test concurrency --test tunneling -- --test-threads=1
