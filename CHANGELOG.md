# Changelog

All notable changes will be documented here. The format follows Keep a
Changelog and versions follow Semantic Versioning. The finer-grained design,
measurement, and rejected-experiment history lives in the
[engineering log](docs/engineering-log.md).

## Unreleased

### Changed

- Reject zero or oversized process connection, DNS, and dial capacities at
  startup instead of silently clamping them. Valid values are unchanged.
- Apply the same startup validation to parser, DNS answer/cache, and diagnostic
  ceilings. Invalid requests fail instead of silently changing the limit.
- Reject malformed, out-of-range, or nondecimal Host ports before DNS or dialing,
  while preserving absent ports and valid matching decimal ports.
- Require exact CONNECT success in allowed benchmarks, with a real-denial
  negative control; instrument the opt-in management workload without weakening
  its competing-traffic requirement.
- Center the README on `Proxy / Policy / Lease`, certified close, and the host
  boundary; preserve advanced examples as tested configuration documentation.

### Added

- Explicit release certification with isolated source snapshots, fresh dependency
  evidence, independent performance budgets, and failure on missing/noisy evidence.
- A separate public-API host consumer covering failed-close ownership and retry;
  freshness and hash checks for the Linux fixture; ownership warnings on handles.

- Same-proxy Linux identity-reuse evidence with a changed destination policy
  and an unrelated continuous tunnel; opt-in management progress under churn;
  default-quiet lifecycle measurements; and a bounded resource certificate
  with explicit memory budgets and failure on missing measurements.

- Establish the `Proxy` / immutable `Policy` / owning `Lease` API, with one
  shared synchronous management handle backed by an owned async runtime and a
  thin executable built from the same library.
- Make successful `Lease::close` certify revocation, cancellation in every
  connection phase, identity release, and final usage counters. Failed close
  and proxy-wide shutdown return the still-owning handle for safe retry.
- Attribute peers only by the listener-observed source IP, assign a
  non-wrapping lease sequence, and protect source-address reuse with a
  queue-draining quiet period.
- Add deny-by-default hostname and port policy, exact and wildcard hostname
  grants and denials, CIDR grants and deny-overrides-grant rules, and explicit
  per-lease connection, rate, byte, DNS, handshake, and idle limits.
- Resolve names once, validate every answer, and dial only an approved numeric
  address. Add bounded DNS concurrency, answer cardinality, deadlines,
  optional caching, trusted explicit resolvers, and UDP-to-TCP recovery.
- Add process-wide connection, connection-attempt, DNS, and outbound-dial
  budgets reserved before their corresponding work begins.
- Add bounded TLS `ClientHello` inspection with visible-SNI equality, explicit
  ECH handling, malformed-input rejection, and exact forwarding of inspected
  bytes.
- Add optional operator-controlled HTTP CONNECT chaining that receives only
  the locally resolved and approved numeric destination.
- Add nonblocking, rate-limited structured denial events and saturating
  per-lease connection and byte accounting.
- Add deterministic lifecycle, DNS-wire, TLS-fixture, hostile-I/O, concurrency,
  identity-reuse, and resource-stability conformance suites without a public
  network dependency.
- Add opt-in local setup-latency, capacity, tunnel-throughput, and
  process-resource measurement harnesses, plus pinned structural and cognitive
  complexity reporting.
- Add a pinned Rust 1.88 Linux container factory, unprivileged packaged-crate
  conformance runner, offline-after-warmup checks, and an opt-in reviewed IANA
  registry-drift check.

### Security

- Reject private, loopback, link-local, multicast, unspecified, documentation,
  benchmarking, reserved, metadata, and other non-global destinations by
  default, including IPv4-mapped, compatible, transition, and registered
  RFC 6052 NAT64 representations.
- Require native IPv6 destinations to be in global unicast space and reject
  IANA special-purpose ranges by default; explicit network grants remain a
  host-controlled override unless an immutable denial also matches.
- Reject unsupported or ambiguous CONNECT framing and authority forms,
  including bodies, folded or duplicate authority headers, controls, userinfo,
  noncanonical numeric spellings, bracketed non-IPv6 text, oversized headers,
  and disagreement between request target and `Host`.
- Apply absolute deadlines from socket acceptance through headers, DNS, dial,
  CONNECT success, and optional `ClientHello` forwarding. Divide remaining dial
  time across approved addresses so one attempt cannot starve a fallback.
- Enforce upload and download ceilings on exact forwarded prefixes with bounded
  buffers and backpressure, including bytes coalesced with CONNECT or TLS
  framing.
- Prevent stale accepted sockets, late DNS answers, queued dial work, timed-out
  close replies, dropped handles, and delayed cleanup from crossing lease
  generations or inheriting replacement policy.
- Reject invalid listener, source-identity, resolver, upstream-proxy, and
  recursive-self-proxy configurations, including scoped IPv6 values without
  the required zone information.
- Document the exact authority promise: CONNECT authority plus visible outer
  SNI when enabled, without claiming that SNI inspection enforces hidden
  application authority or defeats domain fronting.

### Changed

- Make successful lease shutdown the only public constructor of `FinalUsage`;
  it no longer implements `Default`.
- Keep resolver caching disabled by default, cap optional cache storage at 64
  responses, default DNS concurrency to 32, and default global connection
  admission to 256 after resource and capacity measurements.
- Deduplicate immutable policy rules and approved DNS answers, store policy in
  its existing shared lease state, incrementally scan CONNECT headers, drain
  buffered TLS records before reading again, and avoid redundant handshake
  copies.
- Isolate the resolver and conformance modules from the lifecycle core while
  preserving the public API and measured behavior.
- Adopt the Sandbox Egress package identity and document its deliberately
  excluded scope, integration model, security boundary, reproducible factory,
  performance evidence, and contribution process.
