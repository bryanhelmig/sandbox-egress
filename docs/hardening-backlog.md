# Hardening backlog

This is an attack and measurement inventory, not a promise that every item
belongs in the core crate. Prefer deterministic local tests. Public-internet
services may supplement research but must not become test dependencies.
Completed items leave this file once their contract and reproducible evidence
are recorded in [testing](testing.md) and the
[engineering log](engineering-log.md); this keeps the list useful to the next
contributor instead of preserving solved work as apparent backlog.

## Lifecycle and identity

- Source-address reuse with accepted, queued, retransmitted, and delayed SYNs.
- Old-run connections that arrive after a new policy is attached.
- Host fencing requirements for Firecracker TAP/NAT/conntrack teardown.

## Request parsing and authority

- Slowloris headers at every byte boundary and just below deadlines.
- Parser differentials beyond the committed authority, header-smuggling, and
  post-header byte matrices.

## DNS, IP policy, and SSRF

- Longer noncyclic CNAME chains and broader malformed DNS packet matrices; a
  one-hop alias to a forbidden terminal address, a two-name cycle, and a
  transaction-ID-only response are already pinned on the real wire path.
- A byte-aware upstream DNS decoder/cache bound. Current defaults disable
  caching, cap opt-in storage at 64 responses, and limit 32 concurrent lookups,
  but Hickory still allocates its decode vectors from wire section counts before
  Sandbox Egress can enforce its returned-address ceiling.

## TLS and application authority

- Compatibility across deployed TLS versions and clients beyond the committed
  Rustls, fragmentation, malformed-length, and GREASE matrices.
- ECH evolution and interoperability beyond the explicit current policy modes.
- Domain fronting and the exact difference between CONNECT, SNI, and
  application authority.

## Tunnelling and accounting

- Accounting when a policy ceiling and an independent transport failure become
  observable at nearly the same time.
- Large full-duplex transfers and asymmetric traffic.

## Capacity and denial of service

- Optional reserved-share or fair admission semantics between leases; the
  current contract is fail-fast attribution and recovery on retry.
- Listener backlog saturation and general accept-loop fairness beyond the
  certified close/attach drain barriers.
- A bounded trusted-host control-plane design that preserves nonblocking Drop
  cleanup; concurrent caller recovery is measured, but outstanding host calls
  are not currently capped inside the crate.
- Stable RSS, threads, tasks, sockets, and descriptors under long-lived
  sustained traffic and repeated backpressured tunnel soak; simultaneous
  silent idle expiry is measured.
- Allocation and copy overhead per connection and per transferred byte.
- Long-duration downstream diagnostic retention and aggregation behavior; the
  proxy-side reason cardinality, emission rate, and channel work are bounded.

## Deployment and integration

- Linux network namespaces, nftables, TAP devices, and Firecracker guests.
- DNS routing that cannot bypass the proxy boundary.
- IPv4-only, IPv6-only, dual-stack, unusual MTU, and packet loss/delay.
- Resolver configuration changes and absent or malformed `resolv.conf`.
- Process signals, supervisor crashes, restart, and orphan cleanup.
- Resource-capped standalone executable using the same library.
- macOS and Windows compilation without overstating enforcement strength.

## Reproducible evidence

- Loom or state-machine tests for small ownership transitions where useful.
- Coverage reports tied to the hostile matrix, not used as a quality proxy.
