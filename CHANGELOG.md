# Changelog

All notable changes will be documented here. The format follows Keep a
Changelog and versions follow Semantic Versioning.

## Unreleased

- Release closed identity registry entries without allowing delayed cleanup to
  remove a replacement lease.
- Keep a timed-out close's identity unavailable even when cleanup readiness
  races reply delivery.
- Adopt the Sandbox Egress name and package identity.
- Establish the repository, design invariants, contributor factory, initial
  `Proxy / Policy / Lease` API, CONNECT path, tests, and benchmarks.
