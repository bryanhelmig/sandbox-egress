#!/bin/sh
set -eu

cargo bench --bench lifecycle --bench connections
