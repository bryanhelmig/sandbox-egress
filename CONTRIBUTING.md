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

Bug reports involving a possible bypass should follow `SECURITY.md`, not a
public issue.

