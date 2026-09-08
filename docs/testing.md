# Testing

The [security invariants](security-invariants.md) define the behavior to prove.
This page maps those promises to executable checks. Dated results and rejected
experiments belong in the [engineering log](engineering-log.md), while the
[release guide](release-certification.md) owns the readiness verdict.

## Ordinary checks

```sh
./scripts/check.sh
./scripts/test-conformance.sh
```

The ordinary factory runs formatting, all-target checking, Clippy, tests,
doctests, documentation, and offline package verification. If cargo-deny is
installed, it checks the cached advisory/license/source policy; otherwise it
reports that the check was skipped. Fresh advisory evidence is mandatory in
the explicit release driver, not implied by ordinary CI.

Conformance reruns the library tests and serializes the public lifecycle,
concurrency, tunneling, CLI and benchmark-oracle tests. It is another execution
mode over those tests, not a second disjoint collection to add to the count.
Hosted CI remains one cached Linux ordinary-check job.

All test traffic uses local controlled peers. Cargo and tool setup may need
network access. Use fixed inputs and deterministic phase barriers; do not add
random input generation or depend on public Internet services.

## Invariant-to-test index

| Boundary | Executable evidence |
| --- | --- |
| Deny-by-default hosts, ports, CIDRs, mapped/NAT64 equivalence and builder validation | [policy](../src/policy.rs), [configuration](../src/config.rs), [identity](../src/identity.rs) |
| CONNECT authority, Host agreement, forbidden framing headers, byte limits and exact header boundary | [parser/framer](../src/connect.rs), [real-socket lifecycle](../tests/lifecycle.rs) |
| Admission before spawning, final counters, lost close replies, failed-owner recovery, terminal-state reaping, identity release and queued accepts | [white-box lifecycle](../src/proxy/tests/mod.rs), [public lifecycle](../tests/lifecycle.rs), [concurrency](../tests/concurrency.rs) |
| Absolute deadlines, cancelled permit waits, pending dials and success/initial-payload backpressure | [deadlines](../src/proxy/tests/deadlines.rs), [dial budget](../src/proxy/tests/dial_budget.rs), [routing](../src/proxy/tests/routing.rs) |
| One absolute lookup, whole-answer policy checks before numeric dial, fallback, cache revalidation and resolver configuration | [routing](../src/proxy/tests/routing.rs) |
| Real UDP/TCP cancellation, malformed DNS, CNAME chains/cycles, wrong questions and late responses | [DNS wire](../src/proxy/tests/dns_wire.rs) |
| ClientHello fragmentation, malformed records, SNI/ECH, upload ceilings and no forwarding before approval | [TLS parser](../src/tls.rs), [TLS through the proxy](../src/tls_tests.rs), [captured client fixtures](../tests/fixtures/README.md) |
| Half-close, resets, bidirectional backpressure, exact byte ceilings and idle expiry | [tunneling](../tests/tunneling.rs), [white-box metering](../src/proxy/tests/mod.rs) |
| Saturating counters, attempt-rate buckets and bounded diagnostics | [usage](../src/usage.rs), [rate](../src/rate.rs), [diagnostics](../src/diagnostic.rs), [concurrency](../tests/concurrency.rs) |
| Thin executable and successful-CONNECT benchmark oracle | [CLI](../tests/cli.rs), [benchmark contract](../tests/benchmark_contract.rs) |

For every relevant change, include denial, cancellation, timeout, identity-reuse
and resource-bound behavior. A positive-path test or a final active count of
zero alone does not prove all of them. Prefer observing connector calls, future
destruction, terminal sockets and exact counters at the actual boundary.

The framing regression composes LF or mixed-line-ending headers with a later
CRLF delimiter in payload and exercises every two-part read split. Valid CRLF
preserves payload byte-for-byte; a parser/framer disagreement is denied before
dial. The shutdown regression abandons a shutdown certificate, then drops an
already-closed lease and requires registry/reaper ownership to disappear while
the stopping proxy remains alive. An already-certified state must not start a
new accept-drain cycle.

## Resource and management pressure

```sh
cargo build --locked --release --test resource_soak
python3 scripts/certify-resources.py
cargo test --locked --release --test management_load -- --ignored --nocapture
```

The resource driver runs eight independent process lanes: identity churn,
failed startup, concurrent management, idle expiry, partial ClientHello,
partial CONNECT headers, bidirectional backpressure, and terminal-connection
churn. Each requires RSS/FD/thread samples and recovery evidence. Failed startup
covers both an occupied listener and post-bind rejection of the listener as its
own DNS server. Chaining was removed, so there is no upstream-proxy response
occupancy lane. The remaining limits and failure criteria are unchanged.

The [factory-pressure guide](factory-pressure.md) defines workloads, budgets,
source provenance and their limits. Management overlap is measured over the
combined attach/close cycle, not independently inside each operation. Neither
that finite workload nor the shared listener promises fairness under arbitrary
saturation.

For raw resource observations without the certificate evaluator:

```text
./scripts/measure-resources.sh [lease-runs-per-batch] [lease-batches]
  [idle-connections] [tls-connections] [terminal-runs-per-batch]
  [terminal-batches] [header-connections] [failed-start-runs-per-batch]
  [failed-start-batches]
```

## Performance, coverage and host checks

The [performance guide](performance.md) defines setup, load and throughput
measurements and their controls. Do not run them beside builds or other
measurement workloads.

`./scripts/measure-coverage.sh` requires cargo-llvm-cov 0.9.0 and the pinned
toolchain's llvm-tools-preview component. Inspect missing parser, lifecycle and
failure spans; do not use a percentage as a correctness certificate. The
framing bug survived full upstream-parser line coverage before that feature
was removed. Combinations and assertions matter more than line execution.

`./scripts/measure-complexity.sh` uses SCC 4.0.0. Explain changed responsibilities
and distinguish implementation from tests rather than optimizing the total.

`docker build -t sandbox-egress:dev .` checks the Rust 1.88 floor; running the
image executes serialized conformance. The opt-in privileged
`scripts/test-linux-host-boundary.sh` uses the separate public-API consumer to
prove fenced close and two source-IP generations on one listener while an
unrelated tunnel remains active. See [host integration](host-integration.md).
This is not proof of every VMM, NAT/conntrack, delayed-packet or bypass topology.

`scripts/check-iana-drift.sh` is an explicit online maintainer check. It compares
authoritative registry downloads with reviewed pins and never edits policy.
