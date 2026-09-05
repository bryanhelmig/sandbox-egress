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

The [recorded release evidence and open gates](release-certification.md#recorded-release-evidence)
identify the evaluated commit and remaining management/performance work. A
passing ordinary factory alone is not the complete release verdict.

The source can become public after its permanent repository URL is known, that
URL is added to package metadata and README links, private vulnerability
reporting is configured, and the first hosted CI run passes. The full Git
history already has a clean secret scan; its author name and email must be
treated as intentionally public.

Before publishing a preview crate: choose the prerelease version, turn the
Unreleased changelog into release notes, run `cargo publish --locked --dry-run`,
inspect the package, and verify the generated crate documentation. Publishing
remains a deliberate local maintainer action; there is no automatic release
workflow or long-lived registry token in GitHub Actions.

Before presenting `0.1.0` as ready for serious sandbox integration: complete an
independent API review, an independent threat-model review, and at least one
external sandbox integration. The deterministic malformed-input corpus, MSRV
factory, dependency/license audit, package inspection, benchmark baseline, and
resource certification are already implemented but must be rerun for the
release commit.
