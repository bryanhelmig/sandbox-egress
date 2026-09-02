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

## Concurrent partial-ClientHello resource baseline

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1. Each connection
holds 60,020 bytes of legal TLS records describing an incomplete 65,535-byte
handshake. Exact aggregate upload accounting establishes that all parser
states are live before sampling:

```text
command: SANDBOX_EGRESS_TLS_CONNECTIONS=N cargo test --release \
  --test resource_soak concurrent_partial_client_hellos_release_process_resources \
  -- --ignored --nocapture --exact
connections:       1       32       64      128      256
peak RSS KiB:   9536    14304    19056    28512    47472
peak FDs:         18      142      270      526     1038
threads:           6        6        6        6        6
```

The one-to-256 comparison is about 149 KiB of additional peak RSS per live
connection for a 60 KiB partial hello. This includes the retained wire image,
Rustls incremental parser state, task/socket state, and allocator effects; it
is not a claim that one allocation has that size. The linear result reinforces
why global and per-lease connection limits and the configurable ClientHello
byte ceiling are resource controls.

The 64-connection case passed ten consecutive release-process runs. Peak RSS
was 19,008–19,088 KiB, with exactly 270 descriptors and six threads. Every
certified close returned to 13 descriptors and five threads with the proxy
alive, and shutdown returned to nine and two. RSS remained near its high-water
mark because the process allocator retained released pages, so RSS is reported
rather than used as the cleanup oracle. Exact ownership/counters, terminal
sockets, descriptors, and threads are enforced.

## Repeated bidirectional-backpressure resource baseline

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1. One guest writer and
one upstream writer use nonblocking sockets while neither application reads.
Each must observe `WouldBlock`, and the lease must account positive traffic in
both directions, before certified close. The same source identity is then
reattached for the next cycle.

```text
command: cargo test --release --test resource_soak \
  repeated_bidirectional_backpressure_releases_process_resources \
  -- --ignored --nocapture --exact
cycles: 16 per batch, 4 batches, 64 total
fresh-process runs: 10
elapsed through shutdown: 4.03 .. 7.35 s
first active cycle: 9056 .. 9168 KiB RSS, 18 FDs, 7 threads
after 64 closes:   9392 .. 9488 KiB RSS, 13 FDs, 5 threads
after shutdown:    9184 .. 9296 KiB RSS,  9 FDs, 2 threads
```

Every cycle reports one accepted connection, zero active, completed, or denied
connections, positive upload and download counters, and terminal errors from
both writers. RSS is reported as an allocator high-water observation, while
the exact lease state plus descriptor and thread return are enforced. This is
a repeated cleanup baseline, not sustained-throughput evidence.

## Concurrent partial upstream-response baseline

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1. Each of 128 guest
connections sends an approved numeric CONNECT request through the configured
upstream proxy. The upstream accepts every request and returns 900 bytes of a
legal but unterminated response header, holding every parser and both TCP
sockets live until lease revocation.

```text
command: SANDBOX_EGRESS_UPSTREAM_CONNECTIONS=128 cargo test --release \
  --test resource_soak concurrent_partial_upstream_responses_release_process_resources \
  -- --ignored --nocapture --exact
fresh-process runs: 10
peak:               11,216 .. 11,296 KiB RSS, 526 FDs, 6 threads
after close:        11,456 .. 11,536 KiB RSS,  13 FDs, 5 threads
after shutdown:     11,280 .. 11,392 KiB RSS,   9 FDs, 2 threads
elapsed:            0.26 .. 0.28 s
```

Every run reports 128 accepted connections and zero active, completed, denied,
uploaded, or downloaded counts after certified close. Both sides observe
terminal sockets. As in the other lanes, RSS is recorded as allocator
high-water evidence while exact ownership and descriptor/thread recovery are
the cleanup oracles.

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

A later three-run sweep at revision `d9cb0f1` made that experiment
reproducible with `./scripts/measure-load-sweep.sh 10000 16 3` and extended it
to 256 concurrent clients:

```text
concurrency:                 1        8       32       64      128      256
median connections/sec: 6,348   17,676   19,284   19,948   17,505   16,570
median p99 latency (us):   175      419    1,303    3,042    8,409   79,435
```

The local two-worker proxy is throughput-saturated by roughly 32–64 callers.
Adding callers beyond that point does not increase aggregate setup rate and
amplifies tail latency; the 256-caller runs ranged from 13,223 to 17,584/sec
with p99 latency from 41.7 to 160.1 ms. This is a capacity-planning observation,
not a configured concurrency recommendation. Production code and runtime
tuning remain unchanged.

The final three-run sweep at revision `0e634ef` reproduced the same curve on
the 187-case implementation:

```text
concurrency:                 1        8       32       64      128      256
median connections/sec: 6,792   19,919   21,295   21,949   20,524   17,742
median p99 latency (us):   165      431    1,231    2,315    5,932   68,420
```

Absolute medians improved in this local run, but no code-path change explains
that movement and no speedup is claimed. The stable signal is the curve:
throughput again levels around 32–64 clients and 256 clients sharply increase
tail latency. Runtime tuning remains unchanged.

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

## Fixed-work tunnel concurrency sweep

Recorded 2026-09-01 on the same Apple M1 at revision `30372a4`. Each point
moves exactly 1 GiB in one direction, divided evenly over the concurrent
tunnels, and is repeated three times:

```text
command: ./scripts/measure-throughput-sweep.sh 1024 3
concurrency:             1       2       4       8      16      32
median upload MiB/sec: 2548    3725    3550    3540    3233    2241
median download MiB/s: 2739    3877    3734    3712    2828    1200
```

Two tunnels exploit the owned runtime's two workers and materially outperform
one. Four and eight remain on the same broad plateau; more tunnels add
scheduling contention, and 32 is substantially slower and noisier. This is a
local loopback capacity curve, not a production concurrency recommendation.
No production tuning is retained.

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

## Empty destination-denial path check

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1. A detached
pre-change worktree and the candidate alternated the end-to-end allowed-hostname
benchmark after one build warmup:

```text
command: cargo bench --bench connections -- connect_allowed_hostname
paired runs: 3
candidate intervals: 138.17 .. 167.09 us
baseline intervals:  136.91 .. 155.73 us
```

The first pair separated with the candidate slower; the second and third
overlapped, and Criterion reported no change within each candidate's later
same-binary comparisons. An earlier three-pair run also moved in both
directions. The loopback and scheduler noise does not support a stable
regression or improvement claim. The result justifies retaining the security
rule while keeping this path in the benchmark suite; it does not claim that
the larger immutable policy is free.

## Empty hostname-denial path check

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1. A detached
`3c2456c` worktree and the candidate alternated the allowed-hostname benchmark:

```text
command: cargo bench --bench connections -- connect_allowed_hostname
paired runs: 3
candidate intervals: 145.73 .. 183.13 us
baseline intervals:  145.37 .. 173.68 us
```

The second pair was tightly overlapping. Both the first baseline and third
candidate had several severe outliers, widening opposite sides of the overall
ranges. Criterion reported no statistically detected candidate change in all
three runs. No stable regression or improvement is claimed; the benchmark
remains the signal for future changes to hostname matching.

## Policy-builder simplification lifecycle check

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1. Three alternating
pairs compared the simplified builder with a detached `4587c07` worktree:

```text
command: cargo bench --bench lifecycle -- attach_close_empty_lease --noplot
paired runs: 3
candidate intervals: 1.3720 .. 1.3973 ms
baseline intervals:  1.3752 .. 1.4071 ms
```

Every pair overlapped. The result supports retaining the deletion without a
performance claim; it neither demonstrates an improvement nor a regression in
the measured attach/build/close lifecycle.

## Smaller initial CONNECT-header reservation

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1. Three alternating
pairs compared a 1 KiB initial header reservation with the 4 KiB `f2f19c6`
baseline. Each round covered an allowed connection, a hostname denial, and two
distinct 1 MiB hostile-header shapes:

```text
command: cargo bench --bench connections -- <case> --noplot
paired runs: 3

case                                  candidate us     baseline us
connect_allowed_loopback              109.77..138.52   103.88..146.14
connect_denied_hostname                73.43..85.45      68.25..89.74
connect_oversized_header_1mib         650.26..680.53   643.13..756.52
connect_near_terminator_header_1mib   652.12..673.39   639.62..673.23
```

Intervals overlapped in every workload and moved in both directions across
runs. No latency improvement or regression is claimed. The retained effect is
smaller bounded allocation demand: each in-progress header starts by requesting
1 KiB rather than 4 KiB of vector capacity, while larger headers still grow up
to the same configured byte limit.

## Shared immutable proxy configuration

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1. Each admitted
connection used to clone and retain the complete `ProxyConfig`, including its
DNS-server and NAT64-prefix vectors. The connection runtime now shares one
immutable configuration through `Arc` and clones only that handle.

The isolated benchmark deliberately populates both vectors so it measures the
ownership operation this change removes:

```text
command: cargo bench --bench lifecycle -- clone_ --noplot
clone_populated_proxy_config_control: 58.900 .. 61.846 ns
clone_shared_proxy_config:             9.858 .. 10.029 ns
```

This is not an end-to-end speedup claim. Three alternating 50,000-connection
load pairs against `9ae7c31` were mixed: candidate throughput ranged from
17,056 to 19,718 connections/second and baseline from 18,578 to 19,578.
Setup-latency ranges overlapped as well. DNS, socket, and scheduler work
dominate that harness. The retained result is narrower: populated vectors are
no longer allocated and copied once per admitted connection, and connection
tasks have simpler immutable ownership.

## Azure WireServer floor check

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1. Three A/B pairs
compared the allowed-hostname path after adding one reviewed provider endpoint
to the flat forbidden-IPv4 table with detached `5dc2754`:

```text
command: cargo bench --bench connections -- connect_allowed_hostname \
         --noplot --sample-size 50 --measurement-time 3
paired runs: 3
candidate intervals: 148.57 .. 241.49, 154.23 .. 170.30,
                     156.27 .. 173.33 us
baseline intervals:  152.38 .. 166.38, 154.97 .. 168.98,
                     154.88 .. 174.07 us
```

Every pair overlaps. The first candidate contains a wide host outlier; the two
following pairs are nearly coincident. No allowed-path latency change is
claimed. The security rule and deterministic zero-dial proof are retained, and
the detached comparison worktree was removed.

## Borrowed DNS admission permit

Recorded 2026-09-01 on the same Apple M1 with Rust 1.97.1. Three alternating
A/B pairs compared the allowed-hostname path after replacing an owned DNS
semaphore permit with a permit borrowed for the lookup's lexical scope:

```text
command: cargo bench --bench connections -- connect_allowed_hostname \
         --noplot --sample-size 50 --measurement-time 3
baseline: 7b63b9a
candidate intervals: 139.56 .. 162.71, 144.74 .. 157.94,
                     147.66 .. 169.33 us
baseline intervals:  141.71 .. 159.46, 148.88 .. 165.85,
                     147.75 .. 163.44 us
```

Every pair overlaps and the medians move in both directions, so no latency
change is claimed. The retained result is a narrower ownership boundary, one
fewer atomic shared-owner operation per hostname lookup, and one fewer
production line. The detached comparison worktree was removed.

## Connection-attempt limiter comparison

Recorded 2026-09-02 on the same Apple M1 with Rust 1.97.1. The first
token-bucket implementation acquired the lease lifecycle mutex on every
connection, including when rate control was disabled. Three 30,000-connection
A/B pairs against detached `57a4e1f` put the candidate below its baseline in
every pair: 18,329 versus 19,709, 9,784 versus 10,445, and 12,136 versus 12,355
connections/second. That default-path implementation was rejected.

The retained implementation branches around all time and lock work when both
buckets are absent, and keeps the optional process-wide bucket on the single
listener owner rather than behind another mutex. Eight alternating and
reversed-order 30,000-connection comparisons measured:

```text
command: SANDBOX_EGRESS_LOAD_CONNECTIONS=30000 \
         SANDBOX_EGRESS_LOAD_CONCURRENCY=64 \
         SANDBOX_EGRESS_LOAD_DESTINATIONS=16 \
         cargo test --locked --release --test load -- --ignored --nocapture
retained default: 18,211 .. 20,292 connections/second
detached baseline: 16,125 .. 20,641 connections/second
retained p50: 1,761 .. 1,933 us
baseline p50: 1,763 .. 2,021 us
```

The ranges overlap and ordering effects reversed, so no default-path speedup
or regression is claimed. The feature remains disabled by default.

Eight same-tree pairs then compared the disabled path with both global and
per-lease buckets enabled at a nonbinding rate and 30,000-attempt burst:

```text
disabled: 15,690 .. 19,556 connections/second, median 18,891
enabled:  16,498 .. 19,408 connections/second, median 18,355
disabled median p50: 1,912 us
enabled median p50:  1,925 us
```

The optional dual-bucket path is about 2.8 percent lower at the throughput
median and 13 microseconds higher at the p50 median in this noisy local
workload. That is recorded as the current security-control cost, not a portable
performance promise. The harness configures both scopes explicitly and the
connection is still denied before task creation when either bucket is empty.

## Post-hardening tunnel-throughput checkpoint

Recorded 2026-09-02 on the same Apple M1 with Rust 1.97.1 after the denial
response lifetime work. Eight concurrent loopback tunnels each transferred 256
MiB, for 2 GiB of exact accounted payload per direction:

```text
command: ./scripts/measure-throughput.sh 256 8 both 0
upload:   3,229.9 MiB/s, 634 ms
download: 3,083.3 MiB/s, 664 ms

command: ./scripts/measure-throughput.sh 256 8 both 1000
upload:   3,257.5 MiB/s, 628 ms
download: 3,346.4 MiB/s, 611 ms
```

Both modes completed with exact 2,147,483,648-byte directional counters. The
idle-tracked runs happened to be faster in this single local checkpoint, so no
idle-tracking speedup or overhead claim is made. The useful result is a fresh,
reproducible established-tunnel baseline whose scale is long enough to be less
dominated by connection setup than the default 32 MiB smoke run.
