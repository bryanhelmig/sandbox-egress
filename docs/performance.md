# Performance evidence

Performance results are evidence for regression review, not portable promises.
Record the command, revision, machine, toolchain, and interval. Prefer
comparisons on the same host over absolute cross-host thresholds.

## Initial M0 baseline

Recorded 2026-08-31 on Darwin arm64, Apple M1, with Rust 1.97.1:

```text
command: ./scripts/bench.sh
benchmark: attach_close_empty_lease
estimate: 1.3567 ms .. 1.3684 ms (95% confidence interval)
samples: 100
```

This measures synchronous attachment followed by certified closure of an empty
lease. It includes management-channel round trips and the default 25 ms
identity-reuse quiet period is disabled by the benchmark. It does not measure
CONNECT parsing, DNS, dialing, tunnel throughput, or resource ceilings.

## Required next measurements

The M1 resource-soak harness will report peak RSS, thread count, descriptor
count, admitted/denied connections, and cleanup state under repeatable abuse.
Later macrobenchmarks add connections per second, tunnel throughput, and
p50/p95/p99 setup latency. See [`testing.md`](testing.md) and
[`roadmap.md`](roadmap.md).
