# Changelog

All notable changes will be documented here. The format follows Keep a
Changelog and versions follow Semantic Versioning. Git history preserves prior design reviews and experiment records.

## 0.1.0 — 2026-09-19

First official release, promoting the reviewed alpha.3 implementation without
runtime or public API changes. Includes proxy-wide denied networks, the pooled
namespace reset contract and executable negative controls, and kernel capability
preflight that preserves independent host-boundary coverage on Docker Desktop.
See alpha.2 below for migration from alpha.1.

This release retains the known macOS management-pressure overlap gap: some
samples miss completed competing traffic even when attach/close deadlines pass.
Publication does not turn the incomplete full certificate into a pass. Required
assertions, recorded failures, and host integration obligations are unchanged;
performance calibration remains advisory. Source-bound evidence is attached to
the release.

## 0.1.0-alpha.3 — 2026-09-19

This preview updates host conformance and integration guidance. The Rust
runtime and public API are unchanged from alpha.2. It remains a Git preview;
the previously recorded macOS management-overlap gap is not resolved by these
harness changes.

### Fixed

- Preflight actual TCP socket destruction before pooled scenarios, including the
  documented Docker command. A silent `ss -K` no-op now reports missing
  `CONFIG_INET_DIAG_DESTROY` and exits 78 instead of failing the wrong negative
  control. Keep permission/tool failures distinct and add portable regressions.
- Run the independent host-boundary lane before that preflight, preserving its
  coverage and success output on unsupported kernels. Keep exit 78 for the
  unsupported pooled lane and preserve failures from the first lane.
- Document Docker Desktop's unsupported LinuxKit kernel and require an
  independently empty socket inventory after every host reset.

## 0.1.0-alpha.2 — 2026-09-19

### Integration recipe

Fence the guest, close its lease, clean host TCP and conntrack/NAT state,
verify both empty, then attach the next run. Destroy/recreate and pooled reset
must meet the same postconditions. Close certifies library-owned tasks and
socket handles; it does not erase kernel orphans. A reset failure keeps the
supervisor's slot quarantined even after a successful close.

### Added

- `ProxyConfig::with_denied_network(IpNet)`: a destination floor no policy can
  override, for literals, DNS answers, cache hits, and translated IPv4 forms.
  Trusted recursive DNS endpoints remain separate. Denial reason:
  `proxy-network-denied`.
- Privileged Linux pooled-host coverage with unacknowledged download bytes,
  guest death before close, retained namespace/IP, socket and conntrack reset,
  exact source-port reuse, full new-lease capacity, policy replacement, and
  bystander continuity. Omitting either reset operation must prevent reuse.
- Explicit README explanation of the 25 ms minimum quiet interval and the
  separate host cleanup boundary. Arrivals can restart the interval.

### Release evidence

The release evaluator now separates required correctness/resource/host lanes
from advisory comparative performance. Schema 2 uses `release_eligible` and
retains each measurement's status; `--baseline` is optional. Missing required
measurements and missing management-pressure overlap still fail the gate.
Historical reports keep their original verdict. This remains a preview for
controlled integration, not certification of an arbitrary sandbox deployment.


### Changed

- **Breaking:** remove `ProxyConfig::with_upstream_proxy` and HTTP CONNECT
  chaining. Approved destinations are dialed directly; there is no replacement
  proxy-routing or authentication API.
- **Breaking:** rename `PolicyBuilder::max_upload_bytes` and
  `max_download_bytes` to `max_tunnel_upload_bytes` and
  `max_tunnel_download_bytes`. Semantics remain per tunnel; lease usage remains
  aggregate. Update call sites; no compatibility aliases are retained.
- Reduce the resource certificate from nine to eight lanes by removing only
  the deleted upstream-response workload. Keep post-bind failed-startup
  coverage through recursive-DNS-server rejection. The raw resource script's
  former eighth upstream-connection argument is removed; failed-start settings
  are now arguments eight and nine.
- Keep five focused guides plus README. Move durable maintainer instructions
  into CONTRIBUTING/AGENTS; remove historical reviews and the engineering log
  from the current tree and package. Git history retains their provenance.
- Keep dial-before-SNI inspection deliberately: failed upstream dials return
  HTTP 502 before CONNECT success. Optional reset-on-cancel and pre-dial SNI
  modes are deferred; the host reset contract works with graceful sockets.

### Fixed

- Raise the Rustls minimum to 0.23.45 for
  [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285), found by
  the release factory's fresh advisory scan. Update both consumer lockfiles.
- Reject disagreement between CONNECT framing and the HTTP parser before DNS
  or dialing, preventing LF headers from silently swallowing tunnel payload.
  The removed upstream response parser no longer carries the same defect.
- Recognize already-closed lease state during cleanup after failed or
  unobserved proxy shutdown, preventing retained reapers and repeated listener
  drains. Add deterministic ownership, race and framing regressions.

## 0.1.0-alpha.1 — 2026-09-05

First public preview for API evaluation and controlled host integration. It is
not a production-readiness certificate. Management-pressure overlap and
unchanged-source performance calibration remain unresolved; see the
[release evidence](https://github.com/bryanhelmig/sandbox-egress/blob/v0.1.0-alpha.1/docs/release-certification.md#preview-launch-evidence).
Independent API/threat-model review and a real sandbox integration are still
required before a stable release.

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
