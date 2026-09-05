#!/bin/sh
# Always ask Cargo about freshness; stdout is only the selected executable.
set -eu
artifacts=$(CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/consumer}" \
  cargo build --locked --manifest-path tests/consumer/Cargo.toml --message-format=json)
fixture=$(printf '%s\n' "$artifacts" | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p')
count=$(printf '%s\n' "$fixture" | awk 'NF { n += 1 } END { print n + 0 }')
if [ "$count" -ne 1 ] || [ ! -x "$fixture" ]; then
  echo "error: expected one executable host consumer" >&2
  exit 1
fi
printf '%s\n' "$fixture"
