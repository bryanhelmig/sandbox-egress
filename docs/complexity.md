# Complexity evidence

Complexity is a review signal, not a target score. This repository uses
[SCC 4.0.0](https://github.com/boyter/scc/tree/v4.0.0) for repeatable source
size, structural-complexity, and cognitive-complexity reports:

```text
./scripts/measure-complexity.sh
```

The default paths are `src`, `tests`, and `benches`; pass explicit paths to
narrow a review. The script requires SCC 4.0.0 and prints both reports without
applying an arbitrary project-wide threshold. This is an explicit local/release
check; the lean hosted CI job does not install or run SCC.

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

## Proxy production/test separation checkpoint

Recorded 2026-09-02 after moving the proxy's unchanged inline test body into
`src/proxy/tests/mod.rs`:

```text
files: 30
lines: 14,843
code lines: 13,473
structural complexity estimate: 865
cognitive complexity estimate: 2,449
proxy.rs: 187 structural, 612 cognitive, 1,721 lines
proxy/tests/mod.rs: 90 structural, 219 cognitive, 2,840 lines
```

The two proxy files retain the prior 277 combined structural points. The
reported cognitive total falls by 90 only because SCC no longer charges every
test for nesting inside one inline module; no decision branch was removed, so
that movement is a measurement artifact rather than a claimed simplification.
The material result is review scope: the production lifecycle and data path no
longer share a 4,568-line file with failure fixtures.

The immediate follow-up separates the 1,234-line DNS/address/dial proof cluster
as `src/proxy/tests/routing.rs`. The complete report becomes 31 files and
14,845 lines while retaining exactly 865 structural and 2,449 cognitive points.
The test root is 1,608 lines at 71/159; routing is 1,234 lines at 19/60. This is
again responsibility isolation, not deleted logic or a lower aggregate score.

The subsequent DNS question-association proof and wildcard-endpoint API
documentation brought that tree to 14,890 lines and 13,506 code lines while
aggregate complexity remained exactly 865 structural and 2,449 cognitive
points. Extending the existing failed-start resource lane across both pre-bind
and post-bind errors makes the current total 14,903 lines, 13,518 code lines,
and 868/2,457. The added decision shape is test-only. Removing an already-
quiesced branch whose caller loop already handles that state makes the final
tree 14,900 lines, 13,515 code lines, and 866/2,451. The production proxy is
1,553 code lines and 185/606. A later TLS/upload-ceiling proof adds 63 test-only
lines and 60 code lines without a decision point, so the current tree is 14,963
lines, 13,575 code lines, and still 866/2,451; the production proxy remains
unchanged.
