# Changelog

All notable changes will be documented here. The format follows Keep a
Changelog and versions follow Semantic Versioning.

## Unreleased

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
