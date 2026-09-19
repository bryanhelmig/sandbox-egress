# Working agreement

Read README.md, docs/architecture.md, and docs/security-invariants.md before
substantive work. For host adapters, also read docs/deployment-contract.md and
docs/host-integration.md. CONTRIBUTING.md maps claims to factory commands.

## Product boundary

This is an embeddable Rust library for sandbox supervisors, with a thin
executable using the same implementation. Keep the `Proxy / Policy / Lease`
model: one shared runtime, immutable rules per run, one owning lease per
host-observed identity. Privileged guest fencing, kernel reset, and slot
ownership belong to the integrating host. Simplicity is a feature.

## Invariants

1. Reserve global and lease admission before spawning connection work.
2. Snapshot exactly one immutable policy for each connection.
3. Close admission before cancellation; track headers, DNS, ClientHello, dial,
   and tunnel work through destruction.
4. Dial only exact checked addresses, with no second DNS resolution.
5. Successful close means no library-owned task or socket handle remains and
   usage is final. Host TCP/conntrack cleanup is a separate required boundary.
6. Failed close retains the lease and forbids identity reuse. Drop never
   certifies cleanup. The host quarantines a slot after any reset failure.
7. Identity comes from the host boundary, never guest headers.
8. Proxy-wide destination denials cannot be overridden by a policy.
9. Fail closed and test the adversarial boundary, including cancellation,
   timeout, identity reuse, and resource recovery where relevant.

Change the design and public claim explicitly before changing an invariant.
Do not silently weaken a contract to make a test green.

## Test first

- Write or identify a deterministic failing test before implementing a fix.
  Preserve failed evidence and negative controls. No randomized generators.
- Prefer the smallest vertical slice that strengthens a named invariant.
- Use mature protocol parsers, typed errors, private public-struct fields, and
  immutable policy values. Keep unsafe code forbidden.
- A new dependency needs a reason existing dependencies or the standard
  library are insufficient. Keep OS enforcement in small host adapters.
- Never log secrets, payloads, or unbounded guest-provided text.
- Record decisions and reasons in current docs. Git history preserves old
  experiments; do not recreate an append-only engineering diary in the crate.

## Done means verified

Run ./scripts/check.sh. Security changes also run ./scripts/test-conformance.sh.
Resource/ownership changes run python3 scripts/certify-resources.py; missing
measurements fail. Lifecycle changes run the opt-in management_load target.
Host changes run Dockerfile.host-boundary, including pooled negative controls.
Structural changes run ./scripts/measure-complexity.sh and explain the change.
Performance changes run ./scripts/bench.sh and retain comparable measurements.

Keep correctness, resource, host, and real management-pressure evidence
blocking. Report performance calibration separately; never widen budgets or
relax workload overlap merely to pass. Keep hosted CI small; heavy release
checks are explicit maintainer work. See CONTRIBUTING.md for commands and
exact certificate scope.
