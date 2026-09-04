# Contributing

Read `AGENTS.md` first. Discuss changes that alter a security invariant or
public lifecycle contract before implementing them.

Use a focused branch, add the failing test, make the smallest change, and run:

```text
./scripts/check.sh
```

Public APIs need rustdoc examples and documented errors. New dependencies need
a short rationale in the change description and must pass the license/advisory
checks when those tools are installed.

`./scripts/measure-coverage.sh` is an optional review tool for changes around
security or lifecycle boundaries. Inspect the uncovered code; do not optimize
for the aggregate percentage.

Bug reports involving a possible bypass should follow `SECURITY.md`, not a
public issue.

Hosted CI is deliberately one Linux job running `./scripts/check.sh`. Before a
release, a maintainer also runs the Rust 1.88 container factory, dependency
audit, and any resource, cross-platform, or performance checks appropriate to
the change. See the release gates in `docs/roadmap.md`.


For resource, lifecycle, or host-boundary changes, follow the claim-to-check
matrix in [factory pressure](docs/factory-pressure.md). Record exact workloads
and budgets with the results; a missing measurement cannot certify a release.
