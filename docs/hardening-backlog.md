# Hardening backlog

This is an attack and measurement inventory, not a promise that every item
belongs in the core crate. Prefer deterministic local tests. Public-internet
services may supplement research but must not become test dependencies.
Completed items leave this file once their contract and reproducible evidence
are recorded in [testing](testing.md) and the
[engineering log](engineering-log.md); this keeps the list useful to the next
contributor instead of preserving solved work as apparent backlog.

The current bounded contribution list and measured pressure lanes are in
[factory pressure](factory-pressure.md). Start there before adding another harness.

## Lifecycle and identity

- Retransmitted or delayed old-run SYNs that arrive after listener-level close
  certification, including before or after a replacement policy is attached.
  This needs the host-cage/conntrack harness because a TCP listener cannot
  authenticate the run generation of a packet arriving after identity reuse.
- Extend the generic host certificate with explicit NAT/conntrack-zone teardown
  and delayed-packet identity-reuse evidence. Concrete VMM launch and snapshot
  tests belong to the integrating sandbox, not this crate's core suite.

## Request parsing and authority

- Slow-header schedules at every parser byte boundary and just below the
  deadline; continuous activity is already pinned to the original absolute
  deadline rather than extending it as an idle timeout.
- Parser differentials beyond the committed authority, header-smuggling, and
  post-header byte matrices.

## DNS, IP policy, and SSRF

- Broader malformed DNS packet matrices; a seven-link noncyclic alias chain to
  a forbidden terminal address, a two-name cycle, and a transaction-ID-only
  response are already pinned on the real wire path.
- A byte-aware upstream DNS decoder/cache bound. Current defaults disable
  caching, cap opt-in storage at 64 responses, and limit 32 concurrent lookups,
  but Hickory 0.26.1 and reviewed main `8c7b8780` still reserve decode vectors
  from wire section counts before Sandbox Egress can enforce its
  returned-address ceiling. Prefer an upstream capacity bound or supported
  transport response ceiling over a crate-local UDP/TCP framing fork.

## TLS and application authority

- Compatibility across deployed TLS versions and clients beyond the committed
  Rustls, OpenSSL, Apple SecureTransport, fragmentation, malformed-length, and
  GREASE matrices.
- ECH evolution and interoperability beyond the explicit current policy modes.
- Domain fronting and the exact difference between CONNECT, SNI, and
  application authority.

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

- Extend the Linux namespace/nftables certificate across additional generic
  host-network shapes where they strengthen the reusable contract; keep
  VMM-specific launch and restore machinery in the integrating sandbox.
- A black-box host-cage conformance harness covering direct TCP/UDP, both IP
  families, unrelated loopback and host IPC, proxy-environment overrides,
  inherited sockets, resolver/upstream reachability, and premature identity
  reuse. These paths cannot be certified by an in-listener library test.
- DNS routing that cannot bypass the proxy boundary.
- IPv4-only, IPv6-only, dual-stack, unusual MTU, and packet loss/delay.
- Resolver configuration changes and absent or malformed `resolv.conf`.
- Process signals and supervisor crashes beyond the current named-namespace
  orphan cleanup proof, including reconciliation from a durable run journal.
- Resource-capped standalone executable using the same library.
- Authenticated and TLS corporate upstream proxies, explicit trust-root
  ownership, and resolution-aware host-controlled bypass rules. The current
  transport slice is unauthenticated HTTP CONNECT with validated numeric
  targets and no ambient proxy-environment behavior.
- macOS and Windows compilation without overstating enforcement strength.

## Reproducible evidence

- Loom or state-machine tests for small ownership transitions where useful.
- Coverage reports tied to the hostile matrix, not used as a quality proxy.
