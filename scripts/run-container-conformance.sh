#!/bin/sh
set -eu

./bin/sandbox_egress --test-threads=1
./bin/cli --test-threads=1
./bin/concurrency --test-threads=1
./bin/lifecycle --test-threads=1
./bin/tunneling --test-threads=1
