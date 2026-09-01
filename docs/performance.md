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

## Process-wide dial-budget comparison

Recorded 2026-09-01 on the same Apple M1, comparing the working tree with
`9ec6256` using the end-to-end allowed-loopback CONNECT benchmark. Two
reversed-order, five-second runs without the direct-TCP control measured:

```text
command: cargo bench --bench connections -- connect_allowed_loopback \
         --noplot --sample-size 100 --measurement-time 5
previous median:  122.84, 122.89 us
dial-budget median: 127.52, 130.39 us
```

That unnormalized pair suggested a 4.6–7.6 microsecond setup cost, but shorter
paired runs crossed in both directions. Repeating with the direct loopback
control in each process made the proxy-minus-control medians 80.71 and 72.22
microseconds before the change. The exact retained implementation then measured
71.54 and 72.91 microseconds in two final control-normalized runs. The intervals
overlap substantially, so no precise speedup or regression is claimed. The
retained implementation uses a borrowed permit, paid once around outbound
connection establishment and released before tunnel traffic; it adds no
per-byte or tunnel-lifetime work.

## Absolute-deadline comparison

Recorded 2026-09-01 on the same Apple M1 against `7f4195d`. A first
implementation replaced every Tokio timeout with a deadline-first selector.
Across three control-normalized pairs it was 1.5–13.7 microseconds slower on
the local CONNECT path, so it was discarded.

The retained implementation performs one explicit elapsed-deadline check and
then uses Tokio's maintained timeout, including a newly bounded CONNECT success
write. Five alternating three-second comparisons used the direct loopback TCP
control in each process:

```text
command: cargo bench --bench connections -- loopback \
         --noplot --sample-size 50 --measurement-time 3
previous proxy-minus-control medians: 77.10, 77.39, 78.85, 84.39 us
retained proxy-minus-control medians: 79.57, 79.12, 82.71, 80.87 us
excluded host outlier: retained 107.09 us versus previous 77.39 us
```

The ordinary intervals overlap and the paired differences cross zero in
reversed order. No setup-latency change is claimed. The larger selector was not
retained; the deterministic deadline semantics and bounded response write are.

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

## Opt-in tunnel idle-timeout comparison

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1, comparing the
working tree against `2f1e883`. Five alternating 1 GiB-per-direction runs used
eight established loopback tunnels:

```text
command: ./scripts/measure-throughput.sh 128 8 both [idle timeout ms]
previous default median: upload 3,369; download 3,446 MiB/sec
current default median:  upload 3,335; download 3,464 MiB/sec
enabled 1000 ms median:  upload 3,249; download 3,406 MiB/sec
```

The disabled default moved -1.0% for upload and +0.5% for download versus the
previous commit, inside the observed run-to-run variation. No default-path
throughput change is claimed. Enabling the activity clock measured 2.6% lower
upload and 1.7% lower download than the current default medians. That opt-in
cost updates one shared activity value after each successful nonempty read; it
is retained in exchange for bounded silent tunnel lifetime. These are local
regression measurements, not portable bandwidth promises.

## Required next measurements

The next resource harnesses add live connections, slow peers, admitted/denied
counters, cleanup state, and bulk tunnel throughput. See
[`testing.md`](testing.md) and [`roadmap.md`](roadmap.md).

## Rejected shared-config handoff

Recorded 2026-09-01 on the same Apple M1. A prospective change wrapped the
process config in `Arc` so each admitted connection cloned one pointer instead
of the configuration value. Five sustained 10,000-connection runs moved the
median from 19,663 to 20,321 connections/second, but both sets were noisy.

Criterion then compared the ordinary loopback CONNECT path in both orders. The
first pair reported the value clone 4.0%–17.1% slower than the shared pointer
(`p < 0.05`). After saving the clone build first and comparing the shared build
back against it, the interval spanned 6.6% faster to 20.2% slower (`p = 0.60`):
no change detected. The implementation was reverted. The default config's
dynamic fields are empty, so an atomic reference-count operation was not
retained on every connection without a reproducible benefit.

## Direct TCP control for CONNECT attribution

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1:

```text
command: cargo bench --bench connections \
  'connect_(direct_loopback_control|allowed_loopback|denied_hostname)' -- --noplot
runs: 3
direct loopback TCP: 34.74–38.30 us in two stable runs; 43.42–60.56 us noisy run
hostname denial:    71.02–79.80 us first run; 72.27–87.25 us later runs
allowed CONNECT:   108.99–127.77 us first run; later runs were noisy
```

The direct case uses the same controlled upstream listener and teardown socket
option, but connects without the proxy. In the stable runs, one local TCP
handshake accounts for roughly 35–38 microseconds. The denial path adds one
proxy accept, bounded parse and policy decision, and denial response. The
allowed path adds an upstream TCP handshake before the 200 response. When the
third run slowed, the direct control slowed too; this is evidence of host
network/scheduler noise, not an isolated proxy regression. These overlapping
operations do not support subtracting a precise parser cost.

## Hostile header near-match baseline

Recorded 2026-09-01 on the Apple M1 with Rust 1.97.1:

```text
command: cargo bench --bench connections header_1mib -- --noplot
paired runs: 3
ordinary unterminated header: 641.13 .. 652.31 us across intervals
3-of-4-byte near matches:     640.98 .. 743.29 us across intervals
```

The hostile input repeats `\r\n\rX`, forcing frequent three-byte matches
without ever completing the header terminator. Two near-match runs stayed in
the same tight 641–650 microsecond range as the ordinary input; the third was
noisy at 649–743 microseconds. Criterion reported no statistically significant
change in any paired run. This supports retaining the existing incremental
linear scan and the benchmark as a regression signal; it is not evidence for a
new optimization.

## TLS authority default-path check

Recorded 2026-08-31 on the same Apple M1 after adding opt-in ClientHello
inspection:

```text
command: cargo bench --bench connections
allowed CONNECT: 109.36 .. 123.22 us (114.28 us point estimate)
change: -6.55% .. +6.16%, p=0.91; no change detected
hostname denial: 69.07 .. 73.36 us (71.26 us point estimate)
change: -3.88% .. +6.06%, p=0.56; no change detected
```

Both benchmarks use the default `TlsAuthority::Disabled` policy. The result is
evidence that merely linking the parser does not change connection setup
measurably; it does not measure the opt-in parse path.

## Opt-in visible-SNI setup baseline

Recorded 2026-08-31 on the same Apple M1 with Rust 1.97.1:

```text
command: cargo bench --bench connections
runs: 4 paired runs
hostname CONNECT, inspection disabled: 132.66 .. 176.76 us intervals
hostname CONNECT, visible SNI required: 142.55 .. 164.60 us intervals
```

Both cases resolve `localhost`, connect to the same controlled upstream shape,
forward the same valid ClientHello, and wait for a one-byte upstream
acknowledgement. The inspected case additionally parses the bounded
ClientHello, extracts SNI, checks equality with CONNECT authority, and checks
for ECH. The upstream asserts byte-for-byte receipt in every iteration.

The intervals overlap substantially and individual point estimates varied
more than the difference between modes. No precise parser surcharge is claimed
from this end-to-end test; DNS, scheduling, and socket setup dominate its
noise. The retained benchmark gives future changes a realistic regression
signal without pretending the current machine can isolate parser nanoseconds.

## Incremental oversized-header scan

Recorded 2026-08-31 on the same Apple M1 with Rust 1.97.1:

```text
command: cargo bench --bench connections connect_oversized_header_1mib
before, 3 runs: 43.857 .. 44.377 ms intervals
after, 4 runs:  646.80 .. 679.72 us intervals
improvement:    approximately 64x .. 69x
```

The benchmark writes an exact 1 MiB unterminated header through a real client
socket and waits for the proxy's 431 response. Previously each 4 KiB read
rescanned every preceding byte. The retained implementation scans only the new
bytes and the three-byte overlap where `\r\n\r\n` may cross reads. A complete
connection-benchmark replay found no measurable change in the ordinary
allowed, hostname, visible-SNI, or denied paths.
