#!/bin/sh
set -eu

if [ "$(uname -s)" != Linux ]; then
  echo "unsupported: Linux network namespaces are required" >&2
  exit 2
fi
if [ "$(id -u)" -ne 0 ]; then
  echo "unsupported: run as root in a disposable Linux host or privileged container" >&2
  exit 2
fi
for command in ip nft nc cargo ss timeout sha256sum; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "unsupported: missing $command" >&2
    exit 2
  fi
done

if [ "${SANDBOX_EGRESS_HOST_BOUNDED:-0}" != 1 ]; then
  exec timeout 90 env SANDBOX_EGRESS_HOST_BOUNDED=1 "$0" "$@"
fi

host_namespace="seh$$"
guest_namespace="seg$$"
host_link="seh$$"
guest_link="seg$$"
host_ip="198.19.0.1"
guest_ip="198.19.0.2"
if [ -n "${SANDBOX_EGRESS_HOST_FIXTURE:-}" ]; then
  fixture="$SANDBOX_EGRESS_HOST_FIXTURE"
  expected_hash="${SANDBOX_EGRESS_HOST_FIXTURE_SHA256:?prebuilt fixture requires its expected SHA-256}"
else
  fixture=$(./scripts/build-host-fixture.sh)
  expected_hash=$(sha256sum "$fixture" | cut -d ' ' -f 1)
fi
actual_hash=$(sha256sum "$fixture" | cut -d ' ' -f 1)
if [ "$actual_hash" != "$expected_hash" ]; then
  echo "error: host fixture SHA-256 mismatch" >&2
  exit 1
fi
printf 'HOST_FIXTURE sha256=%s executable=%s\n' "$actual_hash" "$fixture"
temporary="$(mktemp -d /tmp/sandbox-egress-host.XXXXXX)"
proxy_pid=""
client_pid=""
orphan_namespace="seo$$"

cleanup() {
  set +e
  [ -z "$client_pid" ] || kill "$client_pid" 2>/dev/null
  [ -z "$proxy_pid" ] || kill "$proxy_pid" 2>/dev/null
  ip netns del "$orphan_namespace" 2>/dev/null
  ip netns del "$guest_namespace" 2>/dev/null
  ip netns del "$host_namespace" 2>/dev/null
  rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM

create_network() {
  ip netns add "$host_namespace"
  ip netns add "$guest_namespace"
  ip link add "$host_link" type veth peer name "$guest_link"
  ip link set "$host_link" netns "$host_namespace"
  ip link set "$guest_link" netns "$guest_namespace"
  ip -n "$host_namespace" link set lo up
  ip -n "$guest_namespace" link set lo up
  ip -n "$host_namespace" address add "$host_ip/30" dev "$host_link"
  ip -n "$guest_namespace" address add "$guest_ip/30" dev "$guest_link"
  ip -n "$host_namespace" link set "$host_link" up
  ip -n "$guest_namespace" link set "$guest_link" up
}

wait_for_line() {
  pattern="$1"
  file="$2"
  attempts=0
  while ! grep -q "^$pattern" "$file" 2>/dev/null; do
    attempts=$((attempts + 1))
    if [ "$attempts" -ge 100 ]; then
      echo "fixture did not report $pattern" >&2
      cat "$file" >&2 || true
      return 1
    fi
    sleep 0.05
  done
}

start_fixture() {
  control="$temporary/control"
  output="$temporary/proxy.log"
  mkfifo "$control"
  exec 9<>"$control"
  ip netns exec "$host_namespace" "$fixture" "$host_ip:0" "$guest_ip" <&9 >"$output" 2>&1 &
  proxy_pid=$!
  wait_for_line PROXY_ADDR= "$output"
  wait_for_line UPSTREAM_ADDR= "$output"
  wait_for_line BYPASS_ADDR= "$output"
  wait_for_line REPLACEMENT_ADDR= "$output"
  proxy_address=$(sed -n 's/^PROXY_ADDR=//p' "$output")
  upstream_address=$(sed -n 's/^UPSTREAM_ADDR=//p' "$output")
  replacement_address=$(sed -n 's/^REPLACEMENT_ADDR=//p' "$output")
  bypass_address=$(sed -n 's/^BYPASS_ADDR=//p' "$output")
  proxy_port=${proxy_address##*:}
  upstream_port=${upstream_address##*:}
  bypass_port=${bypass_address##*:}
}

install_deny_first_boundary() {
  ip netns exec "$host_namespace" nft add table inet sandbox_egress_test
  ip netns exec "$host_namespace" nft 'add chain inet sandbox_egress_test input { type filter hook input priority 0; policy drop; }'
  ip netns exec "$host_namespace" nft add rule inet sandbox_egress_test input iifname lo accept
  ip netns exec "$host_namespace" nft add rule inet sandbox_egress_test input ct state established,related accept
  ip netns exec "$host_namespace" nft add rule inet sandbox_egress_test input iifname "$host_link" ip saddr "$guest_ip" tcp dport "$proxy_port" accept
}

assert_proxy_path() {
  request="CONNECT 127.0.0.1:$upstream_port HTTP/1.1\r\nHost: 127.0.0.1:$upstream_port\r\n\r\nlease-proof"
  response=$(printf '%b' "$request" | ip netns exec "$guest_namespace" nc -w 2 "$host_ip" "$proxy_port")
  printf '%s' "$response" | grep -q '200 Connection Established'
  printf '%s' "$response" | grep -q 'lease-proof'
}

assert_direct_path_blocked() {
  if ip netns exec "$guest_namespace" nc -z -w 1 "$host_ip" "$bypass_port"; then
    echo "direct TCP bypass reached the host fixture" >&2
    return 1
  fi
}

create_network
start_fixture
install_deny_first_boundary
assert_direct_path_blocked
assert_proxy_path

# Hold one live tunnel, then fence the guest path before asking the lease to
# certify cleanup. This is the same ordering required before snapshot discard
# or source-address reuse.
old_control="$temporary/old-client"
mkfifo "$old_control"
exec 8<>"$old_control"
old_output="$temporary/old-client.log"
printf 'CONNECT 127.0.0.1:%s HTTP/1.1\r\nHost: 127.0.0.1:%s\r\n\r\n' "$upstream_port" "$upstream_port" >&8
ip netns exec "$guest_namespace" nc "$host_ip" "$proxy_port" <&8 >"$old_output" 2>&1 &
client_pid=$!
wait_for_line 'HTTP/1.1 200 Connection Established' "$old_output"

ip -n "$guest_namespace" link set "$guest_link" down
printf 'close\n' >&9
wait_for_line 'FINAL generation=1 ' "$output"
kill -0 "$proxy_pid"
if ! grep -q '^FINAL generation=1 .*active=0' "$output"; then
  echo "lease did not certify zero active connections" >&2
  cat "$output" >&2
  exit 1
fi

attempts=0
while ip netns exec "$host_namespace" ss -Hnt state established | grep -q "$guest_ip:"; do
  attempts=$((attempts + 1))
  if [ "$attempts" -ge 40 ]; then
    echo "proxy retained a host-side tunnel after certified close" >&2
    exit 1
  fi
  sleep 0.05
done

# Recreate the kernel boundary with the same source address while the shared
# proxy and unrelated tunnel remain alive. Only the run lease changes.
ip netns del "$guest_namespace"
ip -n "$host_namespace" link del "$host_link" 2>/dev/null || true
ip netns attach "$orphan_namespace" "$client_pid"
if ip -n "$orphan_namespace" link show "$guest_link" >/dev/null 2>&1; then
  echo "fenced old namespace retained its egress device" >&2
  exit 1
fi
ip netns del "$orphan_namespace"
kill "$client_pid" 2>/dev/null || true
wait "$client_pid" 2>/dev/null || true
client_pid=""
ip netns exec "$host_namespace" nft delete table inet sandbox_egress_test
ip netns add "$guest_namespace"
ip link add "$host_link" type veth peer name "$guest_link"
ip link set "$host_link" netns "$host_namespace"
ip link set "$guest_link" netns "$guest_namespace"
ip -n "$guest_namespace" link set lo up
ip -n "$host_namespace" address add "$host_ip/30" dev "$host_link"
ip -n "$guest_namespace" address add "$guest_ip/30" dev "$guest_link"
ip -n "$host_namespace" link set "$host_link" up
ip -n "$guest_namespace" link set "$guest_link" up

printf 'attach\n' >&9
wait_for_line 'ATTACHED generation=2 ' "$output"
kill -0 "$proxy_pid"
grep -q "^ATTACHED generation=2 endpoint=$proxy_address$" "$output"
install_deny_first_boundary
assert_direct_path_blocked
# The old grant must not survive the new policy on the same source address.
request="CONNECT 127.0.0.1:$upstream_port HTTP/1.1\r\nHost: 127.0.0.1:$upstream_port\r\n\r\n"
response=$(printf '%b' "$request" | ip netns exec "$guest_namespace" nc -w 2 "$host_ip" "$proxy_port")
printf '%s' "$response" | grep -q '403'
printf '%s' "$response" | grep -q 'port-denied'
upstream_port=${replacement_address##*:}
assert_proxy_path
ip -n "$guest_namespace" link set "$guest_link" down
printf 'finish\n' >&9
wait "$proxy_pid"
proxy_pid=""
grep -q '^FINAL generation=2 .*active=0' "$output"
grep -q '^RETRY ownership=retained identity=rejected' "$output"
grep -q '^BYSTANDER exchanges=' "$output"
cat "$output"

# Simulate supervisor restart reconciliation: named kernel resources remain
# deny-first until they are explicitly removed, then no stale name survives.
ip netns del "$guest_namespace"
ip netns del "$host_namespace"
if ip netns list | grep -Eq "(^|[[:space:]])($guest_namespace|$host_namespace)([[:space:]]|$)"; then
  echo "orphan reconciliation left a named namespace behind" >&2
  exit 1
fi

echo "linux host boundary: same-proxy policy replacement, unrelated tunnel continuity, fenced close, and orphan cleanup passed"
