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
