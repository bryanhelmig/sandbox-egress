# Hardening backlog

This is an attack and measurement inventory, not a promise that every item
belongs in the core crate. Prefer deterministic local tests. Public-internet
services may supplement research but must not become test dependencies.

## Lifecycle and identity

- Admission racing `Lease::close` before and after tracker closure.
- Close completion racing synchronous reply timeout or channel disconnect.
- Revocation during headers, DNS, `ClientHello`, dial, upload, and download.
- Half-open and half-closed sockets during revocation.
- Remote peers that never read, never write, or ignore FIN.
- Source-address reuse with accepted, queued, retransmitted, and delayed SYNs.
- Old-run connections that arrive after a new policy is attached.
- Host fencing requirements for Firecracker TAP/NAT/conntrack teardown.

## Request parsing and authority

- Slowloris headers at every byte boundary and just below deadlines.
- CONNECT authority with missing ports, zero, overflow, userinfo, fragments,
  paths, whitespace, IPv4 variants, and bracketed IPv6.
- Absolute-form HTTP when only CONNECT is supported.
- Request smuggling shapes and bytes following the CONNECT header.

## DNS, IP policy, and SSRF

- A/AAAA answers containing a mix of allowed and forbidden addresses.
- CNAME chains, loops, large answer sets, truncation, and malformed packets.
- DNS rebinding between requests and after policy checks.
- Proof that the dialer receives only the checked `SocketAddr`.
- IPv4-mapped IPv6, IPv4-compatible forms, scoped IPv6, zone identifiers, and
  host-configured network-specific NAT64 prefixes.
- Complete special-use range tables, including cloud metadata variants.
- Resolver concurrency, queueing, and memory bounds.
- IP literals and explicit CIDR overrides without accidental broadening.

## TLS and application authority

- Fragmented and coalesced TLS records around `ClientHello`.
- Malformed lengths, oversized handshakes, unknown versions, and GREASE.
- ECH present, absent, malformed, or unsupported by policy.
- Domain fronting and the exact difference between CONNECT, SNI, and
  application authority.
- TLS parser time, memory, and input bounds.

## Tunnelling and accounting

- Byte ceilings at exact boundaries and across buffered post-header bytes.
- Counter behavior on cancellation, denial, reset, timeout, and normal EOF.
- Integer overflow and monotonic snapshot behavior.
- Large full-duplex transfers and asymmetric traffic.
- File-descriptor release after every terminal path.

## Capacity and denial of service

- Global and per-lease reservations before task creation.
- Optional reserved-share or fair admission semantics between leases; the
  current contract is fail-fast attribution and recovery on retry.
- Listener backlog saturation and general accept-loop fairness beyond the
  certified close/attach drain barriers.
- DNS and dial concurrency distinct from tunnel concurrency.
- A bounded trusted-host control-plane design that preserves nonblocking Drop
  cleanup; concurrent caller recovery is measured, but outstanding host calls
  are not currently capped inside the crate.
- Stable RSS, threads, tasks, sockets, and descriptors under soak.
- Allocation and copy overhead per connection and per transferred byte.
- Long-duration downstream diagnostic retention and aggregation behavior; the
  proxy-side reason cardinality, emission rate, and channel work are bounded.

## Deployment and integration

- Linux network namespaces, nftables, TAP devices, and Firecracker guests.
- DNS routing that cannot bypass the proxy boundary.
- Containerized conformance with explicit capabilities and no host networking.
- IPv4-only, IPv6-only, dual-stack, unusual MTU, and packet loss/delay.
- Resolver configuration changes and absent or malformed `resolv.conf`.
- Process signals, supervisor crashes, restart, and orphan cleanup.
- Resource-capped standalone executable using the same library.
- macOS and Windows compilation without overstating enforcement strength.

## Reproducible evidence

- Phase barriers for deterministic lifecycle races.
- Local controllable DNS and upstream fault servers.
- Deterministic malformed-input corpora for protocol parsers.
- Loom or state-machine tests for small ownership transitions where useful.
- Criterion microbenchmarks for attach/close, policy matching, accounting, and
  admission contention.
- Macrobenchmarks for connections/second, setup latency distributions, tunnel
  throughput, and concurrent leases.
- Linux/macOS collectors for peak RSS, threads, descriptors, and cleanup.
- Coverage reports tied to the hostile matrix, not used as a quality proxy.
- Complexity and dependency reports with stable tool versions.
- Docker build and conformance entry point suitable for clean machines.
