# Complexity evidence

Complexity is a review signal, not a target score. This repository uses
[SCC 4.0.0](https://github.com/boyter/scc/tree/v4.0.0) for repeatable source
size, structural-complexity, and cognitive-complexity reports:

```text
./scripts/measure-complexity.sh
```

The default paths are `src`, `tests`, and `benches`; pass explicit paths to
narrow a review. CI pins SCC 4.0.0 and prints both reports without applying an
arbitrary project-wide threshold.

Two useful review scopes are:

```text
./scripts/measure-complexity.sh                 # crate plus external evidence
./scripts/measure-complexity.sh src             # crate tree and colocated tests
./scripts/measure-complexity.sh src/rate.rs tests/concurrency.rs
                                                # one change and its evidence
```

The `src` view is deliberately called the crate-tree view, not production-only
code: Rust unit tests and test seams live beside the code they exercise, and
`src/tls_tests.rs` plus `src/proxy/tests/` are also beneath that directory.
Moving evidence solely to improve a headline number would make the metric less
honest rather than the implementation simpler.

SCC describes its structural complexity as a fast branch/loop approximation,
not an AST-derived cyclomatic proof. Its cognitive mode adds nesting weight.
Compare changes using the same tool version and language. Do not compare these
scores across languages or refactor a readable security predicate merely to
make a number smaller.

## Initial baseline

Recorded 2026-08-31 with SCC 4.0.0 over all committed Rust source, tests, and
benchmarks:

```text
files: 14
lines: 3,178
code lines: 2,778
structural complexity estimate: 292
cognitive complexity estimate: 869
```

The largest structural estimates were `src/policy.rs` at 117 and
`src/proxy.rs` at 102. The policy score is dominated by the deliberately flat
special-use address predicates. The proxy file includes its `cfg(test)` phase
barriers, so file-level totals do not mean all counted code ships in a normal
build.

Use the report to ask concrete questions: did a security change add nesting,
duplicate a decision tree, or concentrate unrelated responsibilities? Clippy's
denied warnings—including function-length and needless-complexity checks—remain
the enforceable local guards. Any new threshold should first be justified by
defect or review evidence and applied to the narrowest useful scope.

## Address-floor simplification

After the first TLS and IPv6 transition hardening passes, adding special ranges
as another boolean predicate chain briefly raised the whole-tree structural and
cognitive estimates to 420 and 1,256; `policy.rs` alone reached 147 and 461.
Replacing both address chains with reviewed prefix data and one bit-prefix
matcher retained broader behavior and produced:

```text
files: 18
lines: 4,473
code lines: 3,965
structural complexity estimate: 314
cognitive complexity estimate: 930
policy.rs: 41 structural, 135 cognitive
proxy.rs: 116 structural, 357 cognitive
```

This was accepted because reviewers can now audit network/prefix pairs directly,
not because a lower aggregate score is inherently safer.

## Current pre-release checkpoint

Recorded 2026-09-01 with SCC 4.0.0 over the exact 188-case tree:

```text
files: 28
lines: 13,481
code lines: 12,244
structural complexity estimate: 780
cognitive complexity estimate: 2,317
proxy.rs: 267 structural, 899 cognitive
policy.rs: 58 structural, 185 cognitive
```

The aggregate includes integration tests, resource lanes, benchmarks, fixed
TLS fixtures, and the large `cfg(test)` conformance body inside `proxy.rs`; it
is not a shipped-binary complexity score. The rise from the early checkpoint
tracks a much larger evidence matrix as well as implementation. Future work
should compare the touched module and its tests separately, explain new branch
shape, and prefer deletion when equivalent invariants remain covered.

## Host-lifecycle and rate-control checkpoint

Recorded 2026-09-02 with SCC 4.0.0 after the connection-attempt and
close/reattach conformance work:

```text
files: 29
lines: 14,015
code lines: 12,720
structural complexity estimate: 817
cognitive complexity estimate: 2,407
proxy.rs: 274 structural, 921 cognitive
policy.rs: 59 structural, 188 cognitive
rate.rs: 4 structural, 12 cognitive
```

At the same checkpoint, the narrower crate-tree command reports:

```text
files: 18
lines: 8,497
code lines: 7,575
structural complexity estimate: 544
cognitive complexity estimate: 1,687
```

Against the prior checkpoint, the aggregate rises by 37 structural and 90
cognitive points. Most of the new evidence shape is in connection-rate,
identity-reuse, and policy-phase integration cases. The production proxy rises
by 7/22, the policy by 1/3, and the isolated integer token bucket contributes
4/12. The Linux fixture lives under `examples/` and is intentionally outside
the default `src tests benches` report. A workspace-based fixture was removed
after package verification showed that it would not be present in the root
crate archive.
