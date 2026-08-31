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

## Initial local connection-setup baseline

Recorded 2026-08-31 on the same Apple M1:

```text
command: cargo bench --bench connections
allowed CONNECT to loopback: 110–126 us point estimates across four runs
hostname denied before DNS:   72–75 us point estimates across four runs
```

These are sequential local setup-latency measurements, not a sustained
connections-per-second claim. The allowed path includes TCP accept, CONNECT
parse and policy, direct loopback dial, and the 200 response. The denied path
includes accept, parse, policy denial, and the HTTP denial response.

Benchmark clients set zero-duration linger after receiving the response so
repeated runs close with RST rather than exhaust the macOS 16,384-port ephemeral
range with `TIME_WAIT` sockets. That socket option is part of the measured
harness overhead and must remain consistent in comparisons.

## Initial sustained CONNECT baseline

Recorded 2026-08-31 on the same Apple M1 with Rust 1.97.1:

```text
command: ./scripts/measure-load.sh 10000 64 16
runs: 5
connections/second: 16,925 .. 20,994 (median 19,079)
p50 setup latency: 1,794 .. 1,900 us
p95 setup latency: 2,114 .. 2,990 us
p99 setup latency: 2,356 .. 5,408 us
```

This opens a real client socket, parses CONNECT, checks policy, dials one of
sixteen controlled loopback destinations, and observes the 200 response. The
reported latency stops there. Aggregate throughput also includes a one-byte
tunnel teardown exchange and remote reset, so every iteration releases its
admission before the worker proceeds.

A one-run concurrency sweep over 10,000 connections measured 6,642/sec at one
worker, 19,078/sec at eight, 15,411/sec at 32, 20,822/sec at 64, and 17,747/sec
at 128. The non-monotonic results are a warning against selecting runtime
settings from one sweep. No production tuning was retained from this baseline.

The same five-run command in the pinned Rust 1.88 Linux container on the local
two-vCPU arm64 VM measured 27,421–31,592 connections/second (median 29,989),
p50 984–1,077 microseconds, p95 1,598–1,856 microseconds, and p99
2,229–3,339 microseconds. These numbers establish a second reproducible
environment; they are not directly comparable to native macOS results.

## Initial local tunnel-throughput baseline

Recorded 2026-08-31 on the same Apple M1 with Rust 1.97.1:

```text
command: ./scripts/measure-throughput.sh 128 8 both
runs: 5
upload:   2,486 .. 3,389 MiB/sec (median 3,335)
download: 3,395 .. 3,518 MiB/sec (median 3,464)
```

Each direction moves 1 GiB through eight established loopback tunnels using
16 KiB application chunks. Setup is outside the timed interval. The final
lease snapshot must report exactly 1 GiB in the measured direction and one
teardown or acknowledgement byte per tunnel in the opposite direction.

This harness resolved an apparent setup optimization: one runtime worker
improved short CONNECT cycles but reduced median upload and download throughput
to 2,352 and 2,508 MiB/sec. Four workers measured 3,071 and 3,104 MiB/sec. The
existing two-worker runtime was retained because it won the data-plane test and
its result reproduced after the comparison.

## Required next measurements

The next resource harnesses add live connections, slow peers, admitted/denied
counters, cleanup state, and bulk tunnel throughput. See
[`testing.md`](testing.md) and [`roadmap.md`](roadmap.md).
