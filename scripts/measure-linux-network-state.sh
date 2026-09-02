#!/bin/sh
set -eu

if [ "$(uname -s)" != Linux ]; then
  echo "unsupported: Linux procfs network counters are required" >&2
  exit 2
fi
if [ "$#" -eq 0 ]; then
  echo "usage: scripts/measure-linux-network-state.sh COMMAND [ARG ...]" >&2
  exit 2
fi
for path in \
  /proc/sys/net/netfilter/nf_conntrack_count \
  /proc/sys/net/netfilter/nf_conntrack_max \
  /proc/net/sockstat \
  /proc/sys/fs/file-nr
do
  if [ ! -r "$path" ]; then
    echo "unsupported: cannot read $path" >&2
    exit 2
  fi
done

sample() {
  conntrack=$(cat /proc/sys/net/netfilter/nf_conntrack_count)
  tcp_inuse=$(awk '$1 == "TCP:" { for (i = 2; i < NF; i += 2) if ($i == "inuse") print $(i + 1) }' /proc/net/sockstat)
  tcp_time_wait=$(awk '$1 == "TCP:" { for (i = 2; i < NF; i += 2) if ($i == "tw") print $(i + 1) }' /proc/net/sockstat)
  udp_inuse=$(awk '$1 == "UDP:" { for (i = 2; i < NF; i += 2) if ($i == "inuse") print $(i + 1) }' /proc/net/sockstat)
  files_allocated=$(awk '{ print $1 }' /proc/sys/fs/file-nr)
}

sample
baseline_conntrack=$conntrack
baseline_tcp=$tcp_inuse
baseline_time_wait=$tcp_time_wait
baseline_udp=$udp_inuse
baseline_files=$files_allocated
conntrack_max=$(cat /proc/sys/net/netfilter/nf_conntrack_max)
temporary=$(mktemp -d /tmp/sandbox-egress-kernel.XXXXXX)
command_pid=""
monitor_pid=""

cleanup() {
  set +e
  [ -z "$monitor_pid" ] || kill "$monitor_pid" 2>/dev/null
  [ -z "$command_pid" ] || kill "$command_pid" 2>/dev/null
  rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM

"$@" &
command_pid=$!
(
  peak_conntrack=$baseline_conntrack
  peak_tcp=$baseline_tcp
  peak_time_wait=$baseline_time_wait
  peak_udp=$baseline_udp
  peak_files=$baseline_files
  while kill -0 "$command_pid" 2>/dev/null; do
    sample
    [ "$conntrack" -le "$peak_conntrack" ] || peak_conntrack=$conntrack
    [ "$tcp_inuse" -le "$peak_tcp" ] || peak_tcp=$tcp_inuse
    [ "$tcp_time_wait" -le "$peak_time_wait" ] || peak_time_wait=$tcp_time_wait
    [ "$udp_inuse" -le "$peak_udp" ] || peak_udp=$udp_inuse
    [ "$files_allocated" -le "$peak_files" ] || peak_files=$files_allocated
    sleep 0.05
  done
  printf '%s %s %s %s %s\n' \
    "$peak_conntrack" "$peak_tcp" "$peak_time_wait" "$peak_udp" "$peak_files" \
    >"$temporary/peaks"
) &
monitor_pid=$!

set +e
wait "$command_pid"
command_status=$?
set -e
command_pid=""
wait "$monitor_pid"
monitor_pid=""
read -r peak_conntrack peak_tcp peak_time_wait peak_udp peak_files <"$temporary/peaks"

recovery_seconds=${SANDBOX_EGRESS_KERNEL_RECOVERY_SECONDS:-10}
recovery_slack=${SANDBOX_EGRESS_KERNEL_RECOVERY_SLACK:-0}
recovery_deadline=$(( $(date +%s) + recovery_seconds ))
recovered=false
while :; do
  sample
  if [ "$conntrack" -le $((baseline_conntrack + recovery_slack)) ]; then
    recovered=true
    break
  fi
  [ "$(date +%s)" -lt "$recovery_deadline" ] || break
  sleep 0.1
done

printf 'kernel_network_state command_exit=%s\n' "$command_status"
printf 'conntrack baseline=%s peak=%s final=%s max=%s recovered=%s\n' \
  "$baseline_conntrack" "$peak_conntrack" "$conntrack" "$conntrack_max" "$recovered"
printf 'tcp_inuse baseline=%s peak=%s final=%s\n' "$baseline_tcp" "$peak_tcp" "$tcp_inuse"
printf 'tcp_time_wait baseline=%s peak=%s final=%s\n' "$baseline_time_wait" "$peak_time_wait" "$tcp_time_wait"
printf 'udp_inuse baseline=%s peak=%s final=%s\n' "$baseline_udp" "$peak_udp" "$udp_inuse"
printf 'files_allocated baseline=%s peak=%s final=%s\n' "$baseline_files" "$peak_files" "$files_allocated"

if [ "${SANDBOX_EGRESS_REQUIRE_KERNEL_RECOVERY:-0}" = 1 ] && [ "$recovered" != true ]; then
  exit 1
fi
exit "$command_status"
