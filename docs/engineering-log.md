# Engineering log

This is the durable record of hardening and performance work. Record what was
measured, what was learned, and what was rejected. Git commits contain accepted
changes; this log also preserves useful negative results.

## Working method

Each cycle should:

1. Name one invariant, attack, bottleneck, or simplification target.
2. Record the current evidence and a falsifiable expectation.
3. Add or identify a reproducer before changing the implementation.
4. Make the smallest plausible change.
5. Re-run correctness, conformance, resource, and performance evidence in
   proportion to the risk.
6. Keep the change only when the evidence supports it.
7. Record rejected approaches and unexpected results.
8. Review the resulting code for a simpler expression before committing.

Do not trade a security invariant for throughput. Do not retain an optimization
whose improvement is inside measurement noise. Do not call an absence of test
failures proof when the relevant interleaving is uncontrolled.

## 2026-08-31 — baseline and close-delivery audit

### Starting point

- Git: `e18bf0b` (`Rename crate to Sandbox Egress`), clean worktree.
- Toolchain: Rust and Cargo 1.97.1 on Apple M1, Darwin arm64.
- Production Rust: 1,400 lines; integration tests: 251 lines; benchmark: 34
  lines.
- Direct runtime dependencies: eight.
- Tests: four unit, six integration/concurrency, and one README doctest.
- Criterion `attach_close_empty_lease`: 1.3567–1.3684 ms on this host, recorded
  in [`performance.md`](performance.md).
- Dependency policy: advisories, bans, licenses, and sources pass with
  `cargo-deny` 0.20.2.

### Local tool availability

Docker/Podman, Hyperfine, cargo-nextest, cargo-llvm-cov, cargo-audit, Tokei,
SCC, Lizard, and Valgrind were not installed. The repository must not make
ordinary correctness depend on optional local tools. Container and extended
measurement entry points should report missing prerequisites clearly, while CI
installs pinned versions for enforcement.

### Finding: close success-delivery race

The runtime currently performs these operations in order:

1. wait for tracked work;
2. wait for the identity quiet period;
3. mark the lease `Closed`, allowing replacement;
4. send the final usage reply;
5. let the synchronous caller receive the reply.

The caller independently applies the same absolute deadline to receiving the
reply. At the boundary, step 3 can happen while step 5 times out. The API then
returns `CloseError` containing the owning `Lease`, but `Proxy::attach` can see
`Closed` and replace its identity. That contradicts the contract that every
failed close retains ownership and prevents identity reuse.

Expectation: move the `Closed` transition to the synchronous success path,
after the reply is actually received. The runtime should report that cleanup
is ready but must not release ownership on behalf of a caller that may have
timed out. Best-effort `Drop` remains a separate path that may release the
identity after cleanup without certifying anything to a caller.

Evidence still needed: a deterministic test seam or state-level test that
forces cleanup readiness to race reply delivery. Repeated wall-clock tests are
not sufficient evidence for a narrow interleaving.

### Result

Accepted. The runtime close waiter now stops at cleanup readiness and returns
final counters without changing the phase to `Closed`. The synchronous caller
marks `Closed` only after it receives the successful reply. The `Drop` reaper
and proxy shutdown retain their independent cleanup paths.

Evidence:

- A deterministic unit test invokes the real runtime close waiter and asserts
  that cleanup readiness leaves the phase `Revoking`. It fails with the old
  ordering, which marked the state `Closed` inside that waiter.
- An integration test proves observed successful close permits a replacement
  lease for the identity.
- The focused interleaving test passed 25 consecutive runs.
- The serialized hostile lifecycle/concurrency suite passed 10 consecutive
  runs.
- `./scripts/check.sh` passed, including Clippy with denied warnings, all tests,
  doctests, rustdoc, and package construction.
- Criterion after the change measured attach plus close at
  1.3381–1.3458 ms. Criterion reported the apparent 0.7% improvement as within
  the configured noise threshold, so this is evidence of no detected
  regression—not a performance claim.

Complexity impact: one retained `Arc` clone and one explicit commit-point phase
transition; no new production type, command, task, or dependency.
