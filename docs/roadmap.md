# Roadmap

## M0 — repository and lifecycle spine

- Public `Proxy / Policy / Lease` shape.
- Owned runtime and synchronous management channel.
- Immutable per-run policy and source-IP attachment.
- CONNECT authority filtering, DNS/IP guard, direct dialing, accounting.
- Fallible, ownership-retaining close.
- Unit, integration, concurrency, docs, CI, and microbenchmark entry points.

## M1 — prove revocation

- Injectable resolver and dialer phase barriers.
- Revocation conformance for headers, DNS, dial, and tunnel.
- Active socket/task gauges and final-zero assertions.
- File-descriptor/thread/RSS soak harness on Linux and macOS.
- Structured denial events with bounded fields and rate limiting.

## M2 — precise TLS authority

- Mature ClientHello parser.
- CONNECT authority plus visible-SNI policy.
- Explicit missing-SNI and ECH policy modes.
- Domain-fronting tests and precise documentation of what is not enforceable
  without TLS termination.

## M3 — protocol and integration breadth

- Plain HTTP absolute-form forwarding.
- Optional transparent ingress adapter.
- Host-authenticated identities beyond source IP.
- Configurable resolver and destination-range tables.
- Thin production daemon configuration and metrics export.

## Release gates

Before a public `0.1.0`: API review, threat-model review, fuzz seeds, MSRV CI,
dependency/license audit, package dry-run inspection, benchmark baseline, and
at least one external sandbox integration.

