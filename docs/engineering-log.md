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

## 2026-08-31 — closed-identity registry retention

### Finding

The runtime registry held an `Arc<LeaseState>` for every distinct identity that
had ever closed successfully. Reusing the same address replaced the entry, but
rotating through source addresses retained each closed policy, tracker,
cancellation token, semaphore, and counters until the entire proxy stopped.

A deterministic regression test retained one observer `Arc`, closed the lease,
and waited for runtime work to settle. Before the fix its strong count remained
two rather than one, proving the registry reference was still live.

### Result

Accepted. Successful close and best-effort drop now enqueue a `Release` command
after cleanup. The runtime removes the registry entry only when it still points
to the exact same `Arc`; a delayed release from an old generation therefore
cannot remove a replacement lease for the same identity.

Evidence:

- The registry-reference test failed before the change (`left: 2`, `right: 1`)
  and passes afterward.
- Separate tests cover successful close, dropped-lease reaping, and a delayed
  old-generation release against a replacement entry.
- The two registry-release tests passed 25 consecutive focused runs.
- `./scripts/check.sh` passed with eight unit tests, seven integration tests,
  and the README doctest.
- Criterion measured attach plus close at 1.3415–1.3524 ms and reported no
  statistically detected performance change (`p = 0.29`).

Complexity impact: one internal command variant, one sender clone per reaper,
and a small pointer-checked removal helper. No public API or dependency changed.

## 2026-08-31 — CONNECT authority semantics

### Finding

RFC 9112 defines CONNECT authority-form as only `uri-host ":" port`. The
general-purpose `http::uri::Authority` parser is intentionally broader: it
accepts URI userinfo and returns IPv6 hosts with their square brackets. We were
using that broader output without narrowing it to CONNECT semantics.

Two regression tests demonstrated the effects before the change:

- `CONNECT user@example.com:443` was accepted and reduced to
  `example.com:443`, rather than rejected as invalid authority-form.
- `CONNECT [2001:db8::1]:443` produced host `[2001:db8::1]`; it therefore
  failed `IpAddr` parsing and could not use explicit IPv6 network policy.

Reference: [RFC 9112 section 3.2.3](https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2.3).

### Result

Accepted. The CONNECT adapter now rejects `@` before general authority parsing
and removes exactly one validated pair of IPv6 brackets before IP/policy
handling. The mature parser remains responsible for grammar and port parsing;
the adapter enforces the narrower protocol meaning.

Evidence:

- Both focused tests failed against the previous implementation and pass after
  the change.
- A real IPv6 loopback integration test proves an explicitly allowed `::1/128`
  target is checked and dialed directly.
- All four parser tests passed 20 consecutive focused runs.
- `./scripts/check.sh` passed with ten unit tests, eight integration tests, and
  the README doctest.

Open question: HTTP/1.1 requires Host-field validation, but policy is derived
only from CONNECT request-target today. Host absence, duplication, and mismatch
need a compatibility and request-smuggling review before choosing strictness.
