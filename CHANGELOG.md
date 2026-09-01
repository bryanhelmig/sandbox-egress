# Changelog

All notable changes will be documented here. The format follows Keep a
Changelog and versions follow Semantic Versioning.

## Unreleased

- Add paired end-to-end benchmarks for hostname CONNECT with and without
  visible-SNI enforcement.
- Make the local factory compile the assembled crate package and declare its
  future docs.rs location in package metadata.
- Deduplicate approved DNS results in first-seen order so repeated records
  cannot amplify sequential dial attempts.
- Reject a zero header deadline, clamp process semaphore limits to Tokio's safe
  maximum, and return a typed error for an oversized per-lease limit.
- Give the fixed 64-header CONNECT parser ceiling its own bounded
  `too-many-headers` response and diagnostic reason.
- Let a close retry immediately certify an already-quiesced lease without
  repeating the identity-reuse quiet period or changing its final counters.
- Commit final counters under the lease lifecycle lock before close success,
  preventing late unadmitted sockets from mutating `FinalUsage`.
- Require native IPv6 destinations to be inside IANA's `2000::/3` global
  unicast block before applying the smaller special-purpose deny table.
- Reject the full IANA `2001::/23` protocol-assignments umbrella by default,
  closing unassigned IPv6 special-purpose gaps while preserving CIDR override.
- Add opt-in, rate-limited structured denial events through a caller-owned
  bounded channel, without a logging dependency or blocking callback. Events
  retain a non-wrapping lease sequence across source-identity reuse.
- Saturate cumulative usage counters at `u64::MAX` so final accounting cannot
  wrap or panic at the integer boundary.
- Bound accepted DNS answer cardinality and reject oversized sets before any
  address can reach the dialer.
- Extend the absolute handshake deadline through forwarding an approved
  ClientHello, including a constrained-socket cancellation proof.
- Apply the IPv4 forbidden-address floor to mapped, compatible, and
  well-known-NAT64 IPv6 forms, and deny unsafe transition prefixes by default.
- Add opt-in bounded TLS ClientHello inspection with visible-SNI equality,
  explicit ECH policy, and revocation/deadline conformance.
- Add an opt-in sustained local CONNECT harness with concurrency, throughput,
  and p50/p95/p99 setup latency.
- Add an opt-in concurrent tunnel throughput harness with exact directional
  accounting checks.
- Cache debug and release dependency builds separately from source changes in
  the Linux container factory, and include factory scripts in source packages.
- Add a pinned structural and cognitive complexity report with an initial
  evidence baseline and CI output.
- Add a pinned Rust 1.88 Linux container factory with conformance and resource
  smoke entry points.
- Add a controlled dial phase and prove both lease revocation and the absolute
  handshake deadline cancel in-progress connection attempts.
- Bound process-wide concurrent DNS work and prove queued lookup cancellation
  and late-answer safety with a controlled resolver seam.
- Add hostile tunnel conformance for download ceilings and certified shutdown
  with idle, nonreading, and flooding peers.
- Enforce upload ceilings on bytes coalesced with a CONNECT header before DNS
  or dialing, and keep each tunnel's byte ceiling independent while retaining
  lease-wide accounting.
- Add repeatable allowed and denied local connection-setup benchmarks.
- Add an opt-in cross-platform identity-churn resource measurement harness.
- Reject userinfo in CONNECT authority-form and support checked bracketed IPv6
  literals.
- Release closed identity registry entries without allowing delayed cleanup to
  remove a replacement lease.
- Keep a timed-out close's identity unavailable even when cleanup readiness
  races reply delivery.
- Adopt the Sandbox Egress name and package identity.
- Establish the repository, design invariants, contributor factory, initial
  `Proxy / Policy / Lease` API, CONNECT path, tests, and benchmarks.
