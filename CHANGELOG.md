# Changelog

All notable changes will be documented here. The format follows Keep a
Changelog and versions follow Semantic Versioning.

## Unreleased

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
