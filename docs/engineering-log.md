# Engineering log

This is the durable record of hardening and performance work. Record what was
measured, what was learned, and what was rejected. Git commits contain accepted
changes; this log also preserves useful negative results.

## Working method

Each cycle should:

1. Name one invariant, attack, bottleneck, or simplification target.
2. Record the current evidence and a falsifiable expectation.
3. Add or identify a reproducer before changing the implementation.
4. Make the smallest plausible change.
5. Re-run correctness, conformance, resource, and performance evidence in
   proportion to the risk.
6. Keep the change only when the evidence supports it.
7. Record rejected approaches and unexpected results.
8. Review the resulting code for a simpler expression before committing.

Do not trade a security invariant for throughput. Do not retain an optimization
whose improvement is inside measurement noise. Do not call an absence of test
failures proof when the relevant interleaving is uncontrolled.

## 2026-08-31 — baseline and close-delivery audit

### Starting point

- Git: `e18bf0b` (`Rename crate to Sandbox Egress`), clean worktree.
- Toolchain: Rust and Cargo 1.97.1 on Apple M1, Darwin arm64.
- Production Rust: 1,400 lines; integration tests: 251 lines; benchmark: 34
  lines.
- Direct runtime dependencies: eight.
- Tests: four unit, six integration/concurrency, and one README doctest.
- Criterion `attach_close_empty_lease`: 1.3567–1.3684 ms on this host, recorded
  in [`performance.md`](performance.md).
- Dependency policy: advisories, bans, licenses, and sources pass with
  `cargo-deny` 0.20.2.

### Local tool availability

Docker/Podman, Hyperfine, cargo-nextest, cargo-llvm-cov, cargo-audit, Tokei,
SCC, Lizard, and Valgrind were not installed. The repository must not make
ordinary correctness depend on optional local tools. Container and extended
measurement entry points should report missing prerequisites clearly, while CI
installs pinned versions for enforcement.

### Finding: close success-delivery race

The runtime currently performs these operations in order:

1. wait for tracked work;
2. wait for the identity quiet period;
3. mark the lease `Closed`, allowing replacement;
4. send the final usage reply;
5. let the synchronous caller receive the reply.

The caller independently applies the same absolute deadline to receiving the
reply. At the boundary, step 3 can happen while step 5 times out. The API then
returns `CloseError` containing the owning `Lease`, but `Proxy::attach` can see
`Closed` and replace its identity. That contradicts the contract that every
failed close retains ownership and prevents identity reuse.

Expectation: move the `Closed` transition to the synchronous success path,
after the reply is actually received. The runtime should report that cleanup
is ready but must not release ownership on behalf of a caller that may have
timed out. Best-effort `Drop` remains a separate path that may release the
identity after cleanup without certifying anything to a caller.

Evidence still needed: a deterministic test seam or state-level test that
forces cleanup readiness to race reply delivery. Repeated wall-clock tests are
not sufficient evidence for a narrow interleaving.

### Result

Accepted. The runtime close waiter now stops at cleanup readiness and returns
final counters without changing the phase to `Closed`. The synchronous caller
marks `Closed` only after it receives the successful reply. The `Drop` reaper
and proxy shutdown retain their independent cleanup paths.

Evidence:

- A deterministic unit test invokes the real runtime close waiter and asserts
  that cleanup readiness leaves the phase `Revoking`. It fails with the old
  ordering, which marked the state `Closed` inside that waiter.
- An integration test proves observed successful close permits a replacement
  lease for the identity.
- The focused interleaving test passed 25 consecutive runs.
- The serialized hostile lifecycle/concurrency suite passed 10 consecutive
  runs.
- `./scripts/check.sh` passed, including Clippy with denied warnings, all tests,
  doctests, rustdoc, and package construction.
- Criterion after the change measured attach plus close at
  1.3381–1.3458 ms. Criterion reported the apparent 0.7% improvement as within
  the configured noise threshold, so this is evidence of no detected
  regression—not a performance claim.

Complexity impact: one retained `Arc` clone and one explicit commit-point phase
transition; no new production type, command, task, or dependency.

## 2026-08-31 — closed-identity registry retention

### Finding

The runtime registry held an `Arc<LeaseState>` for every distinct identity that
had ever closed successfully. Reusing the same address replaced the entry, but
rotating through source addresses retained each closed policy, tracker,
cancellation token, semaphore, and counters until the entire proxy stopped.

A deterministic regression test retained one observer `Arc`, closed the lease,
and waited for runtime work to settle. Before the fix its strong count remained
two rather than one, proving the registry reference was still live.

### Result

Accepted. Successful close and best-effort drop now enqueue a `Release` command
after cleanup. The runtime removes the registry entry only when it still points
to the exact same `Arc`; a delayed release from an old generation therefore
cannot remove a replacement lease for the same identity.

Evidence:

- The registry-reference test failed before the change (`left: 2`, `right: 1`)
  and passes afterward.
- Separate tests cover successful close, dropped-lease reaping, and a delayed
  old-generation release against a replacement entry.
- The two registry-release tests passed 25 consecutive focused runs.
- `./scripts/check.sh` passed with eight unit tests, seven integration tests,
  and the README doctest.
- Criterion measured attach plus close at 1.3415–1.3524 ms and reported no
  statistically detected performance change (`p = 0.29`).

Complexity impact: one internal command variant, one sender clone per reaper,
and a small pointer-checked removal helper. No public API or dependency changed.

## 2026-08-31 — CONNECT authority semantics

### Finding

RFC 9112 defines CONNECT authority-form as only `uri-host ":" port`. The
general-purpose `http::uri::Authority` parser is intentionally broader: it
accepts URI userinfo and returns IPv6 hosts with their square brackets. We were
using that broader output without narrowing it to CONNECT semantics.

Two regression tests demonstrated the effects before the change:

- `CONNECT user@example.com:443` was accepted and reduced to
  `example.com:443`, rather than rejected as invalid authority-form.
- `CONNECT [2001:db8::1]:443` produced host `[2001:db8::1]`; it therefore
  failed `IpAddr` parsing and could not use explicit IPv6 network policy.

Reference: [RFC 9112 section 3.2.3](https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2.3).

### Result

Accepted. The CONNECT adapter now rejects `@` before general authority parsing
and removes exactly one validated pair of IPv6 brackets before IP/policy
handling. The mature parser remains responsible for grammar and port parsing;
the adapter enforces the narrower protocol meaning.

Evidence:

- Both focused tests failed against the previous implementation and pass after
  the change.
- A real IPv6 loopback integration test proves an explicitly allowed `::1/128`
  target is checked and dialed directly.
- All four parser tests passed 20 consecutive focused runs.
- `./scripts/check.sh` passed with ten unit tests, eight integration tests, and
  the README doctest.

Open question: HTTP/1.1 requires Host-field validation, but policy is derived
only from CONNECT request-target today. Host absence, duplication, and mismatch
need a compatibility and request-smuggling review before choosing strictness.

## 2026-08-31 — reproducible identity-churn resource harness

Added an opt-in release-mode harness that rotates distinct source identities
while one proxy remains alive. It samples RSS, descriptors, and threads after
each batch, uses `/proc` on Linux and `ps`/`lsof` on macOS, and keeps unsupported
platforms compilable without inventing measurements.

Initial Apple M1 result for 8,000 leases in four batches:

- final-batch time: 11.124 seconds;
- RSS: 8,576 KiB after proxy start, 8,864 KiB after the fourth batch;
- descriptors: 13 at start and after every batch;
- threads: 5 at start and after every batch;
- after shutdown: 9 descriptors and 2 threads.

Three repeated 4,000-lease runs showed the same descriptor/thread counts and a
flat RSS high-water range after the first batch. The harness deliberately does
not assert a hard RSS threshold yet: allocator high-water behavior is not the
same as a live-object leak, and a portable absolute limit would be brittle.
Descriptor and thread growth are asserted because those counts should return
to a narrow baseline independently of allocator behavior.

The test target cross-checked successfully for `x86_64-unknown-linux-gnu`,
including the `/proc` collector. CI runs a smaller `500 x 2` release-mode smoke;
the longer local command remains configurable for soak work.

## 2026-08-31 — local connection-setup benchmark

### Rejected first attempt

Criterion's default three-second warmup opened enough short-lived loopback
connections to exhaust the macOS ephemeral source-port range (`49152–65535`).
The benchmark failed with `EADDRNOTAVAIL` before producing a repeatable result.
Reducing warmup and sample duration made one run pass but an immediate repeat
still exhausted the range, so duration tuning alone was rejected.

### Result

Accepted as an initial latency benchmark. Benchmark clients set zero-duration
linger after reading the proxy response, producing RST on close and avoiding
`TIME_WAIT` accumulation. `socket2` is a direct dev dependency only; it was
already present transitively in the runtime graph.

Four immediate runs completed without port exhaustion:

- allowed local CONNECT point estimates: 110.32, 112.72, 126.19, and
  116.36 microseconds;
- hostname denial point estimates: 72.38, 75.10, 72.43, and 73.34
  microseconds.

The relatively wide allowed-path intervals make this a regression baseline,
not evidence for small optimizations. A sustained-load harness must separately
model concurrency, socket lifecycle, and OS tuple limits.

## 2026-08-31 — CONNECT payload upload-limit bypass

### Finding

The header reader may receive tunnel payload in the same socket read as the
CONNECT header. Those buffered bytes were written directly upstream after the
200 response, before the metered tunnel wrapper was installed. A policy with
`max_upload_bytes(0)` could therefore send `secret` by placing it immediately
after `\r\n\r\n` in the same write.

The regression test failed against the previous implementation: the proxy
returned 200 and the controlled upstream received all six forbidden bytes.
Inspection during the fix exposed a second error. The documented per-tunnel
ceiling was compared to the lease-wide usage counter, so bytes sent by one
tunnel reduced the allowance available to later or concurrent tunnels.

### Result

Accepted. Buffered payload is counted and checked before DNS or dialing. An
over-limit request receives 413 and cannot create an upstream connection. The
metered copy path now keeps a local per-tunnel byte count initialized with any
already-forwarded buffered payload; aggregate lease counters remain unchanged
for usage reporting.

Evidence:

- A controlled upstream proves that a zero-byte policy receives no coalesced
  payload and no upstream connection is opened.
- A separate post-handshake test proves the metered copy path also forwards no
  payload at a zero-byte ceiling.
- An exact-boundary test proves six allowed coalesced bytes are forwarded and
  counted exactly once.
- Two sequential tunnels on one lease each receive their independent one-byte
  allowance; final lease usage reports two bytes.
- The four focused upload-limit tests passed 20 consecutive runs. Fixing that
  loop also hardened the observer: accepted capture sockets are explicitly
  returned to blocking mode rather than treating a transient `WouldBlock` as
  an empty read.
- `./scripts/check.sh` passed with ten unit tests, twelve integration tests,
  the README doctest, rustdoc, Clippy with denied warnings, and packaging.
- Three connection-benchmark runs found no repeatable regression. One initial
  comparison labeled the denial path slower despite a 69.7 microsecond point
  estimate below the recorded baseline; the next two comparisons detected no
  change. Allowed-path point estimates were 108.9–119.6 microseconds and the
  final two denial estimates were 72.7–76.4 microseconds.

Complexity impact: one early integer comparison and one per-direction `u64`
inside each live tunnel wrapper. No public type, task, allocation, or dependency
was added.

## 2026-08-31 — bidirectional tunnel shutdown conformance

### Question

Does certified close actually finish when a tunnel peer refuses to cooperate,
and do download ceilings mirror the corrected per-tunnel upload semantics?
An assertion that accepts any socket error is insufficient because a read
timeout also means the connection may still be live.

### Result

The existing cancellation path passed five new real-socket conformance cases:

- a zero-byte download ceiling counts six upstream bytes but forwards none;
- two sequential tunnels each receive an independent exact one-byte download
  allowance and aggregate usage reports two bytes;
- an idle tunnel reaches EOF or a terminal reset on both guest and upstream
  sides after close;
- close finishes in under 500 milliseconds while an uploader is active and
  its upstream deliberately never reads;
- close finishes in under 500 milliseconds while an upstream floods data and
  the guest deliberately never reads.

The active writer in each backpressure case receives a terminal socket error.
Timeout and `WouldBlock` are explicitly rejected as proof of closure. All five
tests passed 20 consecutive focused runs. This cycle required no production
change; the evidence is kept because it closes a named conformance gap and is
now part of `scripts/test-conformance.sh`.

## 2026-08-31 — bounded and revocable DNS phase

### Finding

The global connection semaphore indirectly capped the number of resolver
calls, but DNS had no independent process budget despite the design promise.
A workload could therefore occupy every connection slot with concurrent
resolver work, and the suite had no deterministic way to observe cancellation
or late-answer behavior.

### Result

Accepted. `ProxyConfig::with_max_concurrent_dns` now controls a process-wide
semaphore, defaulting to 128. Connections wait for a permit inside their DNS
and absolute handshake deadlines. The permit covers only the resolver call and
is released before address policy checks and dialing.

An internal resolver backend seam keeps the public API small while making the
real connection path controllable in unit tests:

- Five admitted hostname connections with a DNS ceiling of two enter exactly
  two pending resolver futures. Three remain queued without starting lookup
  work.
- Closing the lease drops both active futures, cancels all queued acquisitions,
  leaves the resolver's active count at zero, and allows no queued lookup to
  enter during permit release.
- A separate lookup is held at a one-shot answer boundary. After successful
  close, sending a loopback answer fails because the receiver has been dropped,
  and a real target listener observes no dial.
- Both focused tests passed 25 consecutive runs. `./scripts/check.sh` passes
  with twelve unit tests, seventeen integration tests, and the README doctest.
- The local connection benchmark detected no change: allowed CONNECT measured
  114.2 microseconds and hostname denial 72.0 microseconds at the point
  estimates.

Clippy initially rejected a 312-byte resolver enum variant and a connection
handler that crossed 100 lines. Rather than suppress those signals, the shared
resolver was boxed once at proxy startup and resolution/policy was extracted
as one phase function returning a structured internal denial. No per-connection
resolver allocation or public resolver trait was introduced.

## 2026-08-31 — deterministic dial cancellation

### Question

Can the suite prove revocation while a connection attempt is genuinely pending,
and prove that the advertised absolute handshake deadline includes dialing?
Tests against unroutable public or private addresses are not reproducible:
routing tables, firewalls, and kernels may fail immediately or wait for
different periods.

### Result

Accepted. A test-only connector backend runs through the same address loop and
deadline logic as the system connector while exposing entry and future drop:

- An IP-literal request reaches the connector as exactly the policy-approved
  `127.0.0.1:19443` `SocketAddr`. While the connector is pending, successful
  lease close drops its future and the active-dial count returns to zero.
- A second connector remains pending under a 50 millisecond absolute handshake
  deadline. The proxy drops the dial future, returns a structured 502
  `dial-failed` denial, records the denial, and closes normally afterward.
- Both cases passed 25 consecutive focused runs and Clippy with denied warnings.
- `./scripts/check.sh` passed with fourteen unit tests, seventeen integration
  tests, and the README doctest; the Linux target also passed check and Clippy.
- Criterion detected no connection-setup change: allowed CONNECT measured
  112.6 microseconds and hostname denial 75.1 microseconds at the point
  estimates.

No public connector abstraction, dependency, per-dial allocation, or runtime
task was added. The proxy holds one process-wide connector `Arc`; in non-test
builds its backend has only the zero-sized system variant and calls Tokio's
`TcpStream::connect` directly.

## 2026-08-31 — clean Linux MSRV container factory

### Rejected first attempt

The initial image used `rust:1.88.0-slim-bookworm`, but repository-local
`rust-toolchain.toml` has higher selection precedence and caused Rustup to
download and run 1.97.1 inside the container. The build was stopped: a base
image label is not evidence of the compiler that actually ran.

### Result

Accepted after setting the container's `RUSTUP_TOOLCHAIN=1.88.0` override and
adding a fail-closed compiler-version assertion to the container script. A
cold local `linux/arm64` build then passed:

- formatting, check, Clippy with denied warnings, fourteen unit tests,
  seventeen integration tests, the README doctest, rustdoc, and package
  construction on Rust 1.88.0;
- a release-mode Linux `/proc` smoke with 500 distinct leases in two batches;
- live proxy descriptors held at eight and threads at five across both batches;
- RSS rose from 3,636 KiB after startup to 3,992 KiB after 500 leases, then the
  proxy shut down with four descriptors and two threads;
- `docker run --rm sandbox-egress:dev` reran the serialized conformance lane
  successfully.

The image uses the same repository scripts rather than maintaining a parallel
test implementation. CI now builds and runs it on Ubuntu. The first build also
documented why the explicit compiler assertion is load-bearing; removing it
would allow a future toolchain-file update to invalidate the MSRV claim while
the image still appeared green.

## 2026-08-31 — complexity measurement without score gaming

Selected SCC 4.0.0 after comparing a parser-based Mozilla metrics tool with a
small, current, cross-platform counter. SCC explicitly characterizes its
structural score as a same-language branch/loop approximation; version 4 also
provides a nesting-weighted cognitive mode. Those limitations make it useful
as a trend and hotspot prompt, not as a universal quality number.

Initial `src + tests + benches` baseline:

- fourteen Rust files, 3,178 physical lines, and 2,778 code lines;
- structural complexity estimate 292;
- cognitive complexity estimate 869;
- largest structural file estimates: `policy.rs` 117 and `proxy.rs` 102.

The policy score is mostly the flat forbidden-address table, while the proxy
file includes test-only phase seams. Neither should be split solely to improve
the aggregate. The report is pinned in CI but has no failure threshold; Clippy
continues to enforce function-level warnings. A future gate needs evidence that
its chosen scope predicts review difficulty or defects.

## 2026-08-31 — sustained CONNECT harness and teardown traps

### Rejected fixtures

The first local load fixture accepted each upstream dial and immediately
closed it. Adding zero-duration linger made that close a reset, which raced the
proxy between successful `connect()` and its 200 response. The resulting 502s
measured a broken mock server, not proxy capacity.

Keeping upstreams alive fixed that race, but repeated runs still filled all
16,384 macOS ephemeral ports. Two 5,000-connection runs succeeded; the third
failed with `EADDRNOTAVAIL`, `TIME_WAIT` reached 16,322 sockets, and `netstat -m`
showed 65 historical network-memory denials. Client linger alone did not make
the full bidirectional tunnel teardown deterministic. Both fixture variants
were discarded.

### Result

Accepted an opt-in release-mode harness with an explicit lifecycle. A client
measures through the 200 response, sends one teardown byte, and waits for the
controlled upstream to consume it and reset the established tunnel. Sixteen
destination listeners spread upstream tuples. Five consecutive 10,000-request
runs at concurrency 64 then completed with only eight `TIME_WAIT` sockets:

- 16,925–20,994 connections/second, median 19,079;
- p50 setup latency 1,794–1,900 microseconds;
- p95 2,114–2,990 microseconds and p99 2,356–5,408 microseconds;
- exactly 10,000 accepted connections and zero active connections after
  certified close on every run.

A single concurrency sweep was visibly non-monotonic, ranging from 6,642/sec
at one worker to 20,822/sec at 64 before falling to 17,747/sec at 128. That is
enough to establish a reproducible baseline, but not enough evidence to change
the proxy runtime. The production implementation and public API remain
unchanged.

The identical five-run lane also passed in the pinned Rust 1.88 Linux image on
the local two-vCPU arm64 container VM at 27,421–31,592 connections/second
(median 29,989). Its p50 range was 984–1,077 microseconds and p99 was
2,229–3,339 microseconds. The harness adds 15 structural and 54 cognitive SCC
points, all in `tests/load.rs`; production complexity is unchanged.
