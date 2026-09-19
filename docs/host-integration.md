# Host integration: a clean slot for every run

The library owns a lease. Your supervisor owns the network slot around it.
A slot is the namespace/interfaces, source IP, firewall/NAT state, and guest
attachment that your host assigns to a run. It may be disposable or pooled.

The rule is the same for both:

**Fence → close → clean → verify → reuse.**

## Ownership and startup

Keep one authoritative slot record with a generation, the source IP observed
by the proxy, kernel resource identifiers, conntrack namespace/zone, guest
owner, and lease owner. A pooled slot can retain a fixed name; its ownership
must still change atomically with a fresh generation. Keep it reserved until
cleanup succeeds. Reconcile orphaned records and kernel resources after a
supervisor restart; a missing in-process lease is not evidence of cleanup.

Install the network path deny-first. Prove that the proxy is reachable and
that direct TCP/UDP/DNS, host services, neighboring slots, and alternate proxy
paths are blocked. Attach the policy before launching or resuming the guest.
The attached IP is what the listener observes, including any host SNAT, not
necessarily the address configured inside the VM.

Do not restore a lease from a VM snapshot. Clones receive a new generation and
policy, with host state prepared before resume. Connections saved in guest
memory must reconnect. Host identity, conntrack, and NAT allocations are not
snapshot authority.

## Shutdown: two boundaries

1. **Fence:** stop the old VM/process and block every old packet-producing
   path. Account for queued packets in the host adapter. Keep the slot locked.
2. **Close:** call `Lease::close(deadline)`. Success means all library-owned
   tasks and socket handles are gone and counters are final. On failure,
   retain `error.into_lease()`, keep the slot quarantined, and retry.
3. **Clean:** destroy the old network resources or reset the retained slot,
   as described below.
4. **Verify:** no old host TCP entries or slot conntrack/NAT entries remain.
   Enumeration errors and unsupported cleanup are failures, not empty results.
5. **Reuse:** attach the next immutable policy before enabling the new guest.

After successful close the library no longer owns the lease. If host cleanup
fails, the supervisor's slot record must still prevent reassignment. Never
release the slot merely because the Rust lease has been consumed.

The 25 ms default close quiet interval drains queued old accepts under the
revoking identity. It can restart on arrivals. It is separate from kernel
cleanup and cannot distinguish arbitrary delayed packets after reuse.

## Pattern A: destroy and recreate

After fence and successful close, remove run-owned namespace, interfaces,
routes, firewall rules, shaping, and conntrack/NAT state. Verify cleanup, then
create and prove the next deny-first path before attaching its lease.

Destroy state **where it is owned**. If the shared proxy lives in another
namespace, its accepted TCP sockets can leave kernel-owned orphans there after
close. Deleting the guest namespace alone does not erase them. Apply the
socket cleanup below whenever the surviving proxy namespace can retain old
connections. The postconditions are required regardless of the teardown method.

## Pattern B: reset a pooled slot

Keep the namespace, slot IP, and reusable network setup. While the guest stays
fenced, perform these host operations:

1. In the namespace owning the proxy sockets, destroy TCP connections whose
   peer is the slot's exclusive, proxy-observed source IP. Include kernel
   orphans without a process owner; do not filter only by proxy PID or ESTABLISHED.
2. Flush run-owned conntrack/NAT entries in every namespace/zone that tracks
   them. Perform the final flush after socket destruction so teardown traffic
   cannot recreate state after the flush.
3. Independently enumerate both kinds of state and require zero matching
   entries before making the slot available.

For a dedicated slot namespace and an IPv4 source address, the operations are:

```sh
# Trusted host code, after fence and successful Lease::close.
# Names/IP must come from the supervisor's exclusive slot record.
ip netns exec "$proxy_namespace" ss -Ktan dst "$slot_source_ip"
ip netns exec "$slot_namespace" conntrack -F

# Capture/check command status before inspecting output; failure is not empty.
ip netns exec "$proxy_namespace" ss -Htan dst "$slot_source_ip"
ip netns exec "$slot_namespace" conntrack -L -f ipv4
ip netns exec "$slot_namespace" conntrack -L -f ipv6
# Require empty results. Otherwise quarantine the slot.
```

These are the operations, not a complete production reset script. The
executable test in `scripts/test-linux-pooled-boundary.py` demonstrates the
failure checks for its isolated IPv4 topology. Include all address families,
translation spellings, and conntrack owners used by your actual deployment.

A whole-table conntrack flush is appropriate only when that namespace belongs
exclusively to the slot. Shared namespaces need slot-specific deletion or
conntrack zones, including original and reply/NAT tuple attribution. Never
flush an unrelated tenant's state. The source-IP socket filter similarly
requires an exclusive identity until reset ends.

TCP destruction needs `CONFIG_INET_DIAG_DESTROY` and host `CAP_NET_ADMIN` in
the applicable namespace. Test support during host readiness. `ss --kill`
can exit successfully on kernels that cannot destroy sockets: hosts must verify
the socket list is empty after destruction, as the fixture does, using a separate
all-state listing. A remaining FIN_WAIT, TIME_WAIT, or other matching entry keeps the
slot unavailable. Use a bounded cleanup deadline and quarantine or rebuild
when the kernel cannot satisfy the postcondition.

This crate intentionally leaves privileged resets with the supervisor. Normal
TCP EOF/half-close behavior remains graceful. A future abortive-close option
could reduce leftovers, but cannot reach sockets already orphaned before
cancellation and cannot replace host verification.

## What the host harness proves

Docker Desktop is not a supported runner for this lane: the reviewed LinuxKit
7.0.12 kernel lacks `CONFIG_INET_DIAG_DESTROY`, even with `CONFIG_INET_DIAG=y`.
`--privileged` cannot supply a missing kernel feature. Use a Linux host or VM
whose kernel enables socket destruction, with `CAP_NET_ADMIN` available.

```sh
docker build -f Dockerfile.host-boundary -t sandbox-egress-host .
docker run --rm --network=none --privileged sandbox-egress-host
```

Before either host scenario, the image opens a loopback TCP pair, observes one
exact tuple, requests its destruction with `ss -K`, and independently verifies
its disappearance while the handles remain open. A silent no-op exits **78**
with `kernel lacks CONFIG_INET_DIAG_DESTROY; pooled lane cannot run here`.
This is an unsupported environment, never a passing certificate or a failed
reset negative control. Tool, permission, and enumeration errors remain errors.
The pooled script also runs this check before its standalone scenarios; use
`python3 scripts/test-linux-pooled-boundary.py --preflight-only` for just the
capability check. It neither needs the Rust fixture nor creates a namespace.

The image uses the external public-API consumer, built and hashed from the
candidate source. It runs the existing namespace replacement lane and a pooled
lane with a disposable guest namespace behind a retained SNAT slot. The latter
models guest death without destroying the slot containing conntrack state.

The pooled lane proves:

- unacknowledged download data leaves a host TCP orphan after certified close;
- omitting socket destruction or conntrack flush fails the reuse check;
- reset leaves zero slot conntrack entries and zero host TCP entries to the IP;
- the slot namespace inode, uplink, IP, proxy process, and listener survive;
- the next lease uses the old source ports, rejects the old destination, and
  receives its full simultaneous connection budget; one extra tunnel is denied;
- an unrelated lease keeps one continuous tunnel with exact usage accounting.

The test uses local peers and a 90-second outer timeout. It does not launch
Firecracker, prove all TCP states are destroyable on every kernel, certify
IPv6/UDP/DNS/inherited-descriptor bypass prevention, authenticate arbitrary
delayed packets, or cover every NAT topology. Add these checks to your actual
host adapter before claiming its complete security boundary.

## Host capacity and bypasses

The proxy's concurrent and attempt-rate limits complement host/VMM packet and
bandwidth limits. They do not bound all kernel conntrack or ephemeral-port
usage. Measure those resources on the deployment host. The optional
`scripts/measure-linux-network-state.sh` wrapper reports kernel capacity and
recovery; its host-wide measurements need an otherwise quiet worker.

Private service grants must remain narrower than the host's tenant/control
boundary. Put fixed forbidden networks in `ProxyConfig::with_denied_network`
and retain independent host firewall isolation. A direct or inherited socket
never crosses the proxy's checks. Mark-based exemptions require removing both
`CAP_NET_ADMIN` and `CAP_NET_RAW` from every untrusted workload and sidecar.

References: [Linux ss](https://man7.org/linux/man-pages/man8/ss.8.html),
[conntrack](https://netfilter.org/projects/conntrack-tools/conntrack-manpage.html),
[Firecracker host setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md),
[Firecracker snapshots](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md).
