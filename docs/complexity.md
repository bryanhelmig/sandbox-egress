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

## Recorded measurements

Current changes record their before/after command, source state and results in
the [engineering log](engineering-log.md). Run the script for the source under
review instead of treating a historical total as the current tree size.

The [alpha.1 complexity ledger](https://github.com/bryanhelmig/sandbox-egress/blob/919dd0aa5ab4187ae0659de7aa02c1551cb0bdf3/docs/complexity.md)
preserves the founding baseline, address-floor simplification and later
production/test separation checkpoints. Moving tests out of an inline module
changed SCC nesting scores without removing decisions; that history explains
why a lower number alone does not prove a simpler implementation.
