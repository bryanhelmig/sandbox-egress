# Roadmap

## M0 — repository and lifecycle spine

- [x] Public `Proxy / Policy / Lease` shape.
- [x] Owned runtime and synchronous management channel.
- [x] Immutable per-run policy and source-IP attachment.
- [x] CONNECT authority filtering, DNS/IP guard, direct dialing, accounting.
- [x] Fallible, ownership-retaining close.
- [x] Unit, integration, concurrency, docs, CI, and microbenchmark entry points.

## M1 — prove revocation

- [x] Controlled resolver and dialer phase barriers.
- [x] Revocation conformance for headers, DNS, dial, and tunnel.
- [x] Active socket/task gauges and final-zero assertions.
- [x] File-descriptor/thread/RSS soak harness on Linux and macOS.
- [x] Structured denial events with bounded fields and rate limiting.

## M2 — precise TLS authority

- [x] Mature, bounded ClientHello parser.
- [x] Opt-in CONNECT authority plus visible-SNI equality policy.
- [x] Strict ECH rejection and explicit outer-SNI compatibility mode.
- [x] Revocation and absolute-deadline conformance during partial ClientHello.
- [x] Deterministic malformed CONNECT and ClientHello cases.
- [x] Fixed GREASE cipher-suite and extension conformance.
- [x] Fixed OpenSSL and Apple SecureTransport ClientHello compatibility
  fixtures.
- [ ] Broader versioned real-client corpus beyond the fixed cross-stack
  samples.
- [ ] Application-authority research and tests without overstating what is
  enforceable without TLS termination.

## M3 — upstream composition

- [x] Host-configured, unauthenticated HTTP CONNECT chaining with locally
  validated numeric targets and lease-owned cancellation.
- [ ] Prove the library boundary in at least one external sandbox integration.
- [ ] Exercise the same implementation through a resource-capped executable.

## Deferred breadth, not `0.1` commitments

Plain HTTP forwarding, authenticated or TLS upstream proxies, transparent
interception, identities beyond source IP, arbitrary resolver backends,
configurable destination-range tables, and production metrics may be useful in
some deployments. They do not become core roadmap commitments without passing
the feature bar in [the simplicity review](simplicity-review.md). Prefer a
consumer-owned adapter when it can compose with the current library boundary.

## Release gates

Before a public `0.1.0`: API review, threat-model review, deterministic
malformed-input corpora, MSRV CI, dependency/license audit, package dry-run
inspection, benchmark baseline, and at least one external sandbox integration.
