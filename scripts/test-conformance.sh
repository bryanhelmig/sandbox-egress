#!/bin/sh
set -eu

cargo test --test lifecycle --test concurrency -- --test-threads=1

