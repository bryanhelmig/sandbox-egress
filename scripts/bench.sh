#!/bin/sh
set -eu

cargo bench --locked --bench lifecycle --bench connections
