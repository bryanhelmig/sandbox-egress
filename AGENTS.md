# Working agreement

Before doing substantive work, read `docs/founding-context.md` and
`docs/design-brief.md` in full. They preserve the durable founding input and
must survive context compaction. Then read `docs/security-invariants.md` and
`docs/architecture.md` before changing lifecycle, identity, DNS, or tunnelling
code.

For ongoing hardening, also read `docs/engineering-log.md` and select work from
`docs/hardening-backlog.md`. Record negative results; do not keep unmeasured
optimizations.

## Product boundary

This repository is an embeddable Rust library for sandbox supervisors, with a
thin executable around the same implementation. It is not a sandbox, service
mesh, general-purpose proxy framework, credential broker, or async rewrite
requirement for its caller.

The stable mental model is `Proxy / Policy / Lease`:

- `Proxy`: shared listener, owned runtime, resolver, and global budgets.
- `Policy`: immutable rules for exactly one sandbox run.
- `Lease`: exclusive ownership of the run identity, all admitted work, usage,
  cancellation, and certified shutdown.

## Non-negotiable invariants

1. No task is spawned before global and lease admission are reserved.
2. A connection snapshots exactly one immutable policy and never changes runs.
3. Revocation closes admission before it signals or aborts existing work.
4. Every phase is lease-owned: headers, DNS, ClientHello, dial, and tunnel.
5. A DNS result may only dial the exact checked address; never resolve again.
6. Successful close means no tracked task or socket remains and usage is final.
7. Failed close retains the `Lease`; identity reuse remains impossible.
8. `Drop` may initiate cancellation but can never claim successful cleanup.
9. Identity comes from the host boundary, never from a guest assertion.
10. Security behavior fails closed and is tested at the adversarial boundary.

If a design cannot make one of these statements true, stop and update the
design documents before adding code. Do not weaken an invariant silently.

## How to work

- Prefer the smallest vertical slice that strengthens a named invariant.
- Write or identify the failing test first. Include the phase and race in the
  test name.
- Use mature protocol parsers. Do not hand-roll HTTP or TLS grammar.
- Keep policy values immutable and public structs' fields private.
- Use typed errors with actionable, non-secret diagnostics.
- Keep unsafe code forbidden. Any future exception requires a dedicated design
  record, tests, and narrowly scoped lint allowance.
- Do not introduce a runtime per lease. Blocking management APIs marshal onto
  the proxy-owned runtime.
- Do not add a dependency without documenting why the standard library and
  existing dependencies are insufficient.
- Do not log payloads, credentials, full query strings, or unbounded
  attacker-controlled strings.
- Preserve cross-platform compilation. Put OS enforcement integrations behind
  small adapters and target-specific tests.

## Definition of done

Run `./scripts/check.sh`. Security-sensitive changes also run
`./scripts/test-conformance.sh`; performance-sensitive changes run
`./scripts/bench.sh` and record the before/after command and result in the PR.

A passing attractive-path test is not enough. Add the corresponding denial,
cancellation, timeout, identity-reuse, and resource-bound case where relevant.
Structural changes should run `./scripts/measure-complexity.sh` and explain a
material increase or decrease rather than optimizing blindly for the score.
