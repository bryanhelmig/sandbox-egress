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

## Initial identity-churn resource baseline

Recorded 2026-08-31 on the same Apple M1 with the proxy alive throughout:

```text
command: ./scripts/measure-resources.sh 2000 4
completed leases: 8000 distinct source identities
elapsed: 11.124 s through the final batch
RSS: 8576 KiB after proxy start; 8864 KiB after 8000 leases
open descriptors: 13 after proxy start; 13 after every batch
threads: 5 after proxy start; 5 after every batch
after shutdown: 8832 KiB RSS, 9 descriptors, 2 threads
```

Three additional runs of `1000 x 4` leases completed in 5.758–5.798 seconds
through the final batch. Each held 13 descriptors and 5 threads while the proxy
was alive. RSS after the first batch was 8800–8848 KiB and remained within
that narrow range through the fourth batch. This is an initial same-host
plateau observation, not yet a long-duration bound.

## Required next measurements

The next resource harnesses add live connections, slow peers, admitted/denied
counters, and cleanup state. Later macrobenchmarks add connections per second,
tunnel throughput, and p50/p95/p99 setup latency. See
[`testing.md`](testing.md) and [`roadmap.md`](roadmap.md).
