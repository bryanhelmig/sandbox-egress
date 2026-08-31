# Changelog

All notable changes will be documented here. The format follows Keep a
Changelog and versions follow Semantic Versioning.

## Unreleased

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
