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

### Rejected runtime tuning

With the harness stable, five 20,000-connection runs compared the owned Tokio
runtime at one, two, and four workers on native macOS. Median rates were about
20,307/sec, 18,863/sec, and 18,400/sec respectively. One worker appears to
reduce scheduling overhead for short setup-and-reset cycles.

The change was not retained. A new barrier-synchronized data-plane harness then
moved 1 GiB per direction through eight established tunnels. Five-run medians:

- one worker: 2,352 MiB/sec upload and 2,508 MiB/sec download;
- two workers: 3,335 MiB/sec upload and 3,464 MiB/sec download;
- four workers: 3,071 MiB/sec upload and 3,104 MiB/sec download.

One worker lost about 29% upload and 28% download versus two; four workers lost
about 8% and 10%. Three final control runs after restoring two workers returned
to 3,323–3,372 MiB/sec upload after one colder outlier and 3,402–3,481 MiB/sec
download. The fixture also verifies exact final byte accounting and zero active
connections after close. The two-worker implementation is retained; no
production change survived this cycle.

The harness passed unchanged under Rust 1.88 in the Linux container. It adds
15 structural and 43 cognitive SCC points in `tests/throughput.rs`; production
source and complexity are unchanged.

## 2026-08-31 — source-stable container dependency cache

### Rejected attempts

The original Dockerfile copied the entire repository before any Cargo command,
so every source edit discarded downloaded and compiled dependencies in both
debug and release profiles. A first split-manifest layer primed only `cargo
test`; the real source layer still had to rebuild Cargo's separate check
metadata, so the cache was incomplete.

Priming all factory modes exposed a subtler failure. Cargo considered a dummy
library artifact newer than copied source timestamps and reused it for the
release resource test, producing unresolved imports even though debug tests had
compiled the real library. `cargo clean -p` did not reliably remove that
release fingerprint. Both variants were rejected.

### Result

The accepted dependency layer runs locked check, Clippy, test compilation,
rustdoc, and release resource-test compilation against documented placeholder
targets. After the real source copy, an explicit `touch` makes every project
source newer than those placeholders while retaining dependency artifacts.
The full exact-Rust-1.88 factory and Linux resource smoke then passed.

A later documentation-only edit exposed that copying the real README with the
manifest still invalidated this layer. The cache now creates a placeholder
README beside the copied manifests and removes it with the placeholder targets;
the real README arrives with the source layer and is checked there.
The next documentation-only rebuild reported the entire manifest/dependency
step as cached and reran only source verification.

On the local two-vCPU arm64 container VM, the cached source verification spent
0.6 seconds in check, 0.7 seconds in Clippy, 12 seconds compiling debug project
targets, and 18 seconds compiling the release resource target; the complete
source-layer image build finished in about one minute. A comparable prior
source edit under the unsplit Dockerfile took about 99 seconds. Cache population
is intentionally more expensive and is amortized over source iterations.

The source package now includes all factory scripts, `.cargo` configuration,
Docker metadata, the dependency policy, and the pinned toolchain. `cargo
package --list` reported 50 files, and the built image reran hostile conformance
successfully.

## 2026-08-31 — precise visible TLS authority

### Parser decision

Selected Rustls 0.23's incremental server `Acceptor` rather than maintaining a
ClientHello grammar or accepting a best-effort parser. The dependency disables
default features and enables only `std` and TLS 1.2 compatibility; the normal
dependency tree contains no Ring or AWS-LC crypto provider because Sandbox
Egress does not terminate TLS.

Rustls exposes the syntactically accepted visible server name but not the raw
ECH extension through its public ClientHello view. A small bounded extension
walk therefore runs only after Rustls has accepted the full message and checks
for the registered `0xfe0d` extension type. It does not reinterpret SNI or
decide whether malformed TLS is acceptable.

The retained policy is opt-in. `require_tls_sni()` requires a hostname CONNECT
authority, an equal visible SNI, and no ECH. An explicit `AllowOuterSni` mode
accepts ECH only while documenting that the encrypted inner authority is
unknowable. Neither mode claims to enforce an application authority inside the
encrypted stream.

### Lifecycle and forwarding evidence

- A ClientHello coalesced with the CONNECT header is accepted incrementally
  and reaches the controlled upstream byte-for-byte.
- A visible-SNI mismatch and strict ECH each close the client and upstream with
  zero tunnel bytes forwarded. The destination TCP socket has already been
  connected, which is documented rather than obscured.
- A partial ClientHello held after the 200 response is cancelled by successful
  lease close. Final usage has zero active connections and the upstream sees
  zero bytes.
- A separate partial hello is cancelled by a 50 millisecond absolute handshake
  deadline and records one denial.
- Parser unit cases cover fragmented records, missing SNI, ECH detection, and
  the outer byte bound. The full suite and Clippy with denied warnings pass.

The first test placement made `proxy.rs` 1,758 lines and mixed protocol fixtures
with lifecycle internals. The end-to-end cases were moved to a dedicated
crate-internal `tls_tests` module; the production proxy file returned to 1,447
lines. Its structural score rose from the prior 102 to 115 for the retained
inspection branch, while the parser is isolated at 46 and the conformance
module at 8. No production split was introduced solely to improve a score.

Criterion found no default-path regression: allowed CONNECT was 114.28
microseconds at the point estimate with a -6.55% to +6.16% change interval and
`p=0.91`; early hostname denial was 71.26 microseconds with `p=0.56`. Both were
reported as no change. A strict-path concurrent measurement remains future
work rather than an invented claim.

The exact Rust 1.88 Linux factory caught one older-Clippy documentation lint
that current Rust did not. After correction it passed 22 unit tests, 17
integration tests, the README doctest, rustdoc, and a 51-file package. The
500-lease resource smoke held eight descriptors and five threads while live,
then returned to four descriptors and two threads after shutdown. Running the
built image reran the hostile conformance lane successfully.

The first successful test run could not commit its Docker layer because the
container store was full. Only stopped build containers and untagged or
project-tagged Sandbox Egress images were removed; unrelated images and volumes
were left intact. The complete rebuild then succeeded.

The pinned `cargo-deny` 0.20.2 audit reported no advisories or source
violations, then correctly failed the existing license allowlist on Rustls's
new ISC (`rustls-webpki`, `untrusted`) and BSD-3-Clause (`subtle`) transitive
dependencies. Those specific OSI-approved permissive licenses were added to
the allowlist. The repeated `syn` versions remain a configured warning from
the Hickory and development dependency graphs.

## 2026-08-31 — IPv6-embedded SSRF address audit

### Finding

The existing floor correctly canonicalized IPv4-mapped IPv6 with
`to_ipv4_mapped`, but deprecated IPv4-compatible addresses such as
`::169.254.169.254` remained on the IPv6 path. Rust's broader `to_ipv4`
conversion explicitly covers both forms. Standard and local-use NAT64, Teredo,
and 6to4 added separate ways for an IPv6-looking address to represent or reach
an IPv4 endpoint.

### Result

Accepted a fail-closed extension grounded in the current IANA special-purpose
registries:

- mapped and compatible forms now receive the same IPv4 private, link-local,
  metadata, documentation, multicast, and reserved checks;
- the well-known NAT64 `64:ff9b::/96` is decoded so public embedded IPv4 remains
  available while a metadata or private embedded address is denied;
- local-use NAT64, Teredo, 6to4, benchmarking, ORCHID/DET, documentation,
  discard/dummy, and non-global SRv6 SID prefixes are denied by default;
- an explicit CIDR grant continues to override this floor.

A controlled resolver returns a mixed set containing public `93.184.216.34`
and compatible `::169.254.169.254`. The complete request is denied with
`resolved-address-denied`; 25 consecutive focused runs passed. Unit cases also
prove mapped, compatible, and well-known-NAT64 metadata forms are forbidden
while compatible and NAT64 encodings of a public test address remain allowed.

The first implementation extended the existing flat boolean chain and pushed
`policy.rs` to 147 structural and 461 cognitive points, with whole-tree totals
of 420 and 1,256. It was not retained. Prefixes are now immutable `(network,
length)` tables checked by one bit-prefix function. Boundary tests cover the
first and last address of every entry. With those tests included, the broader
implementation measures 41/135 for `policy.rs` and 314/930 for the whole tree,
below the pre-audit totals of 389/1,156.

## 2026-08-31 — absolute deadline through ClientHello forwarding

### Finding

The TLS authority phase ran the bounded read, Rustls parse, SNI comparison, and
ECH decision inside the absolute handshake deadline, then wrote the approved
ClientHello upstream outside it. A peer that accepted TCP but stopped reading
could therefore hold the connection between validation and tunnelling until
lease revocation.

### Result

Accepted a smaller phase boundary: inspection and the first upstream write now
share one `timeout_at` future and the original absolute deadline. A controlled
test builds a valid padded ClientHello of roughly 64 KiB, fragments it into
16 KiB TLS records, constrains both upstream socket buffers to 1 KiB, and never
reads until the proxy closes. The client sends the entire hello, the upstream
receives only a prefix, the 250 millisecond deadline records one denial, and
final usage has zero active connections. The case passed 25 consecutive runs.

## 2026-08-31 — deterministic protocol parser matrices

### Finding

The CONNECT parser accepted an empty host and port zero because the generic
HTTP authority type considers both syntactically representable. Neither is a
usable outbound destination, and accepting them moves rejection into later
policy or dial phases with less precise diagnostics.

### Result

Accepted a small internal CONNECT module with fixed, reviewable cases:

- empty hosts and port zero now fail in the parser;
- missing, negative, overflowing, and duplicated ports fail;
- userinfo, paths, queries, fragments, and malformed bracketed IPv6 fail;
- ordinary hostname and bracketed IPv6 authorities remain accepted.

The ClientHello suite now checks every prefix of a representative hello fails
closed, every valid TLS-record split is accepted, and corrupt record and
handshake lengths fail. These are deterministic ordinary tests: they use the
stable toolchain, perform no generated-input exploration, and require no
separate test harness.

`./scripts/check.sh` passed 30 unit tests, 17 integration tests, Clippy with
denied warnings, docs, examples, benches, and packaging. The serialized hostile
conformance lane also passed. The measured tree is 4,170 Rust code lines with a
328 structural and 970 cognitive complexity estimate; the new isolated
CONNECT module accounts for 98 code lines and 12/25 of those estimates.

The clean Linux image then passed the same factory on exact Rust 1.88. Its
500-lease resource smoke held the live proxy at eight descriptors and five
threads, returned to four descriptors and two threads after shutdown, and
finished at 3,928 KiB RSS. Running the resulting image passed the serialized
hostile conformance lane.

## 2026-08-31 — absolute deadline through uninspected forwarding

### Finding

The absolute handshake deadline covered headers, DNS, dialing, and the
inspected ClientHello path, but the TLS-disabled path wrote tunnel bytes that
arrived with the CONNECT header without a deadline. An upstream that stopped
reading could therefore hold that pre-tunnel phase until lease revocation.

### Result

Accepted one shared deadline around the buffered upstream write. Bytes already
read from the guest are accounted before forwarding, so a timed-out partial
write cannot disappear from usage. The rejection closes the tunnel and records
a denial rather than returning an unclassified I/O error.

A deterministic test fills a one-byte in-memory upstream, attempts the
buffered write, and proves the original 20 millisecond deadline cancels it
while preserving all 21 accepted upload bytes. It passed 25 consecutive runs,
then the complete factory passed 31 unit and 17 integration tests plus the
serialized hostile lane.

The first implementation pushed `serve_connect` over the 100-line Clippy
ceiling. No lint exception was retained. Extracting named approved-address dial
and uninspected-forward phases held whole-tree structural complexity at 328 and
reduced cognitive complexity from 970 to 966. Criterion reported no change for
allowed CONNECT (106.59 microseconds) or hostname denial (75.91 microseconds).
An unrelated attach/close sample initially read 1.86% slower immediately after
a cold container build; the isolated rerun moved back 1.09% and Criterion
classified it within the noise threshold.

The exact Rust 1.88 Linux image passed the same factory and hostile lane. Its
500-lease smoke again held eight descriptors and five threads while live,
returned to four descriptors and two threads, and finished at 3,916 KiB RSS.

## 2026-08-31 — precise bounded-header denials

### Finding

The CONNECT header reader was bounded and fail-closed, but its caller mapped
every outcome to `408 header-timeout`. A byte-ceiling violation, guest write
EOF, and an actual slow-header deadline were therefore indistinguishable in
the client response and future operational telemetry.

### Result

Accepted a private acquisition phase that preserves four bounded outcomes:

- `431 header-too-large` for the configured byte ceiling;
- `400 header-eof` when the guest ends an incomplete request;
- `408 header-timeout` for the absolute deadline;
- `400 header-read-failed` for other socket failures.

Three real-socket tests drive the first three outcomes and certify one denial
and zero active connections after close. The four-case header filter, including
the pre-existing close-during-slow-header test, passed 25 consecutive runs. The
complete native factory passed 31 unit and 20 integration tests plus the
serialized hostile lane.

Whole-tree structural complexity moved from 328 to 334 and cognitive
complexity from 966 to 983, including the new integration helper and cases.
Connection benchmarks found no change: hostname denial centered at 73.94
microseconds; the warmed rerun of allowed CONNECT centered at 111.18
microseconds with a -6.89% to +1.26% interval and `p=0.43`.

The exact Rust 1.88 Linux image passed the same factory and hostile lane. Its
500-lease smoke held eight descriptors and five threads while live, returned
to four descriptors and two threads, and finished at 4,012 KiB RSS.

## 2026-08-31 — symmetric admission-capacity accounting

### Finding

Both global and per-lease connection permits were reserved before task spawn,
but their rejection accounting differed. Global saturation incremented the
owning lease's denial counter; failure to acquire the per-lease permit returned
early and disappeared from usage.

### Result

A paired real-socket test held one slow header open, forced the next connection
through each ceiling, required a terminal rejected socket rather than accepting
a read timeout, and finalized the lease. Before the fix, global saturation
reported one denial and per-lease saturation reported zero. The retained
four-line `let ... else` path now records the missing refusal.

Both capacity cases passed 25 consecutive runs. The complete factory passed 31
unit and 22 integration tests plus the hostile lane. Production `proxy.rs`
complexity remained exactly 114 structural and 357 cognitive points; the
whole-tree increase from 334/983 to 339/995 belongs to the new concurrency test
helper and cases.

## 2026-08-31 — byte-ceiling violations count as denials

### Finding

Upload and download ceilings correctly counted an over-limit read and stopped
it before forwarding, but `copy_bidirectional` returned the violation as an
ordinary I/O error. Final usage therefore reported zero denials even though an
immutable run policy had ended the tunnel.

### Result

The existing real-socket zero-upload and zero-download cases were strengthened
to require one denial. Both failed at zero before the fix. A private typed
error now crosses the copy boundary, allowing the connection owner to count a
policy denial without reclassifying connection resets, broken pipes, or other
transport errors. The marker allocates only on the violation path.

The paired ceiling tests passed 25 consecutive runs; the complete native
factory passed 31 unit and 22 integration tests plus the hostile lane.
Production `proxy.rs` moved from 114/357 to 117/362 structural/cognitive points
for the typed classification and terminal match.

Three comparable 128 MiB-per-tunnel, eight-tunnel runs measured 3,133–3,294
MiB/sec upload and 3,433–3,492 MiB/sec download. Those ranges remain within the
retained two-worker baseline (3,335 MiB/sec median upload and 3,464 MiB/sec
median download), so no throughput change was claimed.

The exact Rust 1.88 Linux image passed the factory and hostile lane. Its
500-lease smoke held eight descriptors and five threads while live, returned
to four descriptors and two threads, and finished at 3,928 KiB RSS.

## 2026-08-31 — remove unused direct tracing surface

The simplification review found `tracing` declared directly even though no
production or test code called it. Hickory still uses tracing internally, but
removing Sandbox Egress's declaration also disabled the unused default
attributes feature and removed `tracing-attributes` from the resolved graph.
The lockfile lost that proc-macro package and its direct `syn` edge.

`ProxyError::Io` was also removed: no public operation constructed it, and all
actual start/shutdown failures already map to initialization, stopped-runtime,
or shutdown-timeout variants. Keeping an unreachable public variant would
promise a distinction the crate does not make.

The locked native factory, docs, package, and hostile lane all passed. Source
dropped four lines, direct runtime dependencies dropped from nine to eight,
and whole-tree structural/cognitive complexity stayed at 342/1,000. This was a
contract and compile-surface cleanup, not a runtime performance claim.

A cold exact-Rust-1.88 Linux rebuild passed the same factory and hostile lane.
Its 500-lease smoke held eight descriptors and five threads while live,
returned to four descriptors and two threads, and finished at 3,952 KiB RSS.

## 2026-08-31 — reject unrepresentable runtime deadlines

### Finding

Public timeout setters accept `Duration`, including values that cannot be added
to the platform's current monotonic clock. The connection path used unchecked
`Instant + Duration` arithmetic for its absolute handshake, header, and DNS
deadlines, so an extreme trusted configuration could panic a runtime task
instead of returning a construction error or bounded denial.

### Result

`Policy::build` and `Proxy::start` now reject unrepresentable durations. The
live connection path retains checked arithmetic as defense against elapsed
startup time and platform clock boundaries. A runtime check that cannot form a
handshake deadline fails closed with a bounded denial instead of panicking.

The first implementation pushed `serve_connect` over the project's 100-line
function ceiling. It was not retained: the stable tunnel phase was extracted
into a small helper, leaving the orchestration path within the existing lint
policy without an exception.

The complete native factory and 55-case deterministic conformance set passed.
Whole-tree structural complexity moved from 342 to 348 and cognitive
complexity from 1,000 to 1,017, including validation tests and the tunnel-phase
helper extraction.
The connection benchmark detected no performance change: allowed loopback was
109.28–127.56 microseconds with a -2.24% to +6.51% comparison interval
(`p=0.66`), and hostname denial was 66.08–71.64 microseconds with a -9.55% to
+2.76% interval (`p=0.30`).

The exact Rust 1.88 Linux image passed the factory and hostile lane. Its
500-lease smoke held eight descriptors and five threads while live, returned
to four descriptors and two threads, and finished at 4,004 KiB RSS.

## 2026-08-31 — bound DNS answer cardinality before dialing

### Finding

DNS lookup concurrency and time were bounded, and every returned address was
checked, but answer cardinality was not. A controlled 65-address answer reached
the connector and ended as the generic `dial-failed` response. A hostile DNS
server could therefore amplify one request into many sequential connection
attempts within the handshake budget.

### Result

The proxy now accepts at most 64 addresses per lookup by default. Callers can
configure the ceiling within a hard `1..=1024` range. The system resolver
collects no more than ceiling plus one entries; the extra entry detects an
oversized set. Such a set is rejected intact as `dns-answer-too-large`, rather
than truncated in an order-dependent way.

The deterministic regression supplies 65 allowed loopback answers and a
recording connector. It requires a 502 response with the stable reason, exactly
one lease denial, and zero dial attempts. The case passed 25 consecutive runs.
The complete native factory and 57-case conformance lane passed.

Whole-tree structural complexity moved from 348 to 350 and cognitive
complexity from 1,017 to 1,020. No connection benchmark was claimed because
the existing allowed benchmark uses an IP literal and does not exercise DNS.
The exact Rust 1.88 Linux image passed the same factory and hostile lane. Its
500-lease smoke held eight descriptors and five threads while live, returned
to four descriptors and two threads, and finished at 3,996 KiB RSS.

## 2026-08-31 — preserve counter monotonicity at integer limits

### Finding

Usage snapshots promised monotonic counters, but cumulative atomics used
wrapping `fetch_add`. At `u64::MAX`, accepted, completed, and denied counts
wrapped to zero. Upload and download accounting additionally performed a
normal addition on the previous atomic value, which panicked in a debug build
when a read crossed the integer boundary.

### Result

Cumulative totals now use one private saturating atomic update. Boundary tests
first reproduced both wraparound and the debug panic, then proved accepted,
completed, denied, upload, and download counts stop at `u64::MAX`. The active
gauge retains acquire/release increment and decrement: admission semaphores
bound it, and unlike cumulative usage it must fall as work ends.

The change adds no structural or cognitive complexity points: the whole tree
remains 350/1,020. Three 1 GiB, eight-tunnel measurements produced 3,052–3,391
MiB/sec upload and 3,465–3,498 MiB/sec download, overlapping the retained
baseline. Criterion detected no connection-setup change: allowed loopback was
108.91–112.79 microseconds (`p=0.64`) and hostname denial was 70.23–74.85
microseconds (`p=0.45`).

The complete native factory and 59-case deterministic conformance lane passed.
The exact Rust 1.88 Linux image passed the same gates. Its 500-lease smoke held
eight descriptors and five threads while live, returned to four descriptors
and two threads, and finished at 3,984 KiB RSS.

## 2026-08-31 — bounded structured denial diagnostics

### Finding

Lease usage counted denials but gave a supervisor no machine-readable reason.
Copying a daemon-style logger into the library would add backend policy, and a
callback invoked on a Tokio worker could block or panic the shared data plane.
Logging every attacker-triggered denial would also make observability itself a
resource-exhaustion path.

Stripe Smokescreen's pinned source reinforced the value of a canonical decision
reason, but Sandbox Egress needs a library boundary rather than a chosen logging
stack. The retained API therefore accepts a caller-owned bounded
`SyncSender<DiagnosticEvent>`. Proxy tasks use only `try_send`; the crate starts
no logging thread and performs no blocking callback.

### Result

An opt-in process-wide one-second window limits delivery to a caller-selected
rate clamped within `1..=10_000`. A full channel and excess rate both suppress
without blocking. Suppression accumulates with saturation and is attached to
the next event the channel accepts. Events contain a proxy-assigned lease
sequence, source identity, a crate-owned static reason code, and the suppression
count—never the requested hostname or another guest-controlled string.

All lease-owned policy and capacity denials now cross one `record_denial`
boundary that increments usage before attempting diagnostics. Deterministic
clock tests cover rate suppression; a one-slot channel proves backpressure;
and a public real-socket case verifies the bounded event shape across certified
close and source-IP reuse. Eight concurrent reporters also prove one exact
process-wide limit: 800 same-window reports deliver 100 and carry 700
suppressed into the next event. The identity-reuse case passed 25 consecutive
runs.

The review also found the pre-existing internal lease sequence used wrapping
`fetch_add`. Once exposed for correlation, that would make its uniqueness claim
technically false at exhaustion. Attachment now reserves the next sequence with
a checked atomic update and returns `AttachError::LeaseIdExhausted` rather than
wrapping. A boundary test drives that terminal state directly.

With the attribution follow-up, the complete native and exact Rust 1.88
factories passed 65 deterministic cases. The whole-tree structural/cognitive
report is 361/1,059; most of the increase from 356/1,036 is the concurrent,
reuse, and exhaustion evidence. The Linux 500-lease smoke retained five live
threads and eight descriptors, returned to two threads and four descriptors,
and finished at 4,060 KiB RSS.

## 2026-08-31 — make the local dependency audit discoverable

The dependency policy was already enforced in a dedicated CI job with pinned
`cargo-deny` 0.20.2, while the MSRV container intentionally omitted that large
tool build. Locally, however, Cargo could execute an installed subcommand from
its own binary directory even when `command -v cargo-deny` could not see it on
`PATH`. The ordinary factory therefore printed a skip despite a usable audit
tool.

`check.sh` now asks `cargo deny --version` directly before deciding whether to
skip. The full factory then executed the audit: advisories, bans, licenses, and
sources all passed. The only output remains the configured warning for
transitive `syn` 2.x and 3.x versions; no dependency pin was forced merely to
merge proc-macro build graphs owned by Hickory and development dependencies.

## 2026-08-31 — close the IPv6 protocol-assignments umbrella

### Finding

The current IANA IPv6 special-purpose registry marks `2001::/23` as the IETF
protocol-assignments umbrella and non-destination, non-forwardable, and
non-global unless a more-specific allocation applies. The policy table listed
Teredo, benchmarking, ORCHID, ORCHIDv2, and DET children individually. An
unassigned child such as `2001:5::1` therefore passed the default floor. The
new regression reproduced that allow-by-omission failure.

### Result

Five child entries were replaced by the one conservative `2001::/23` parent,
matching the existing treatment of IPv4's `192.0.0.0/24` special umbrella. A
boundary test continues to check the first and last address of every listed
prefix. A separate policy case proves an explicit `/128` grant still overrides
the floor for integrations that knowingly need a more-specific assignment.

This closes the gap while reducing the production prefix table by four rows.
Whole-tree structural/cognitive complexity remains 361/1,059.

The full native factory, including dependency policy, and the exact Rust 1.88
Linux factory passed 66 deterministic cases. No performance claim was made for
removing four entries from a short linear prefix scan. The Linux 500-lease
smoke held eight descriptors and five threads while live, returned to four
descriptors and two threads, and finished at 3,980 KiB RSS.

## 2026-08-31 — require native IPv6 global-unicast shape

### Finding

IANA's IPv6 address-space registry assigns global unicast from `2000::/3`.
After the embedded-IPv4 cases, the policy still allowed any native IPv6 address
that was not in the special-purpose table. Reserved shapes such as `4000::1`,
`8000::1`, and `fe00::1` therefore passed. A deterministic policy case first
reproduced the miss at `4000::1`.

### Result

Native IPv6 must now match `2000::/3`; the separately checked IPv4-mapped,
IPv4-compatible, and well-known NAT64 paths retain their existing behavior.
The special-purpose table then needs only four unsafe children inside the
global block. Seven rows for local, reserved, multicast-adjacent, and other
out-of-block shapes were removed because the global-unicast check subsumes
them.

Tests deny representative reserved blocks, continue to allow a native public
IPv6 address and checked public embedded IPv4, and retain explicit network
override. Whole-tree structural/cognitive complexity falls from 361/1,059 to
359/1,052.

The native factory, dependency audit, and exact Rust 1.88 Linux factory passed
66 deterministic cases. The Linux 500-lease smoke held eight descriptors and
five threads while live, returned to four descriptors and two threads, and
finished at 3,996 KiB RSS.

## 2026-08-31 — make `FinalUsage` actually final

### Finding

The close waiter previously slept through the queued-socket drain interval and
then read `final_snapshot`, but the identity remained in the listener registry
until the synchronous caller received that reply and sent `Release`. A socket
accepted in that delivery gap could observe `Revoking` and increment the denial
counter after the supposedly final snapshot. The deterministic regression
received cleanup readiness, attempted another admission, and reproduced the
mutation from zero denials to one.

Simply marking the lease `Closed` before replying would have been wrong:
`Attach` treats `Closed` as replaceable, so a lost success reply could allow a
new run even though the old caller retained a failed lease. The lifecycle now
has a separate `Quiesced` phase. It freezes counters under the lifecycle lock
but is not replaceable. Only observed close success advances to `Closed` and
releases the registry entry.

Unadmitted global-capacity, lease-capacity, and revoking sockets are now closed
and accounted under that same lock. After quiescence, later sockets are still
closed but do not alter counters. Diagnostics use bounded `try_send` with no
callback, so completing their old-run event under the lock cannot wait on the
consumer.

The phase-barrier case now proves a late socket closes, final usage remains
exactly equal, and identity ownership is retained; it passed 25 consecutive
runs. The paired real-socket capacity cases remain green. Whole-tree
structural/cognitive complexity moves from 359/1,052 to 364/1,069 for the new
state and ordering proof.

Criterion detected no connection-setup change: allowed loopback was
103.79–110.66 microseconds (`p=0.11`) and hostname denial was 67.38–75.05
microseconds (`p=0.54`).

The native factory, dependency audit, and exact Rust 1.88 Linux factory passed
66 deterministic cases. The Linux 500-lease smoke held eight descriptors and
five threads while live, returned to four descriptors and two threads, and
finished at 4,004 KiB RSS.

## 2026-08-31 — certify already-quiesced close retries immediately

### Finding

The new non-replaceable `Quiesced` state made final counters immutable and
retained identity after a lost success reply, but a retry still waited through
the complete identity-reuse quiet period again. A deterministic test used an
already-quiesced lease, a one-second quiet period, and a 50-millisecond retry
deadline. Before the change, it returned `DeadlineExceeded` even though no
cleanup work remained.

### Result

The close waiter first checks for a frozen quiesced snapshot under the same
lifecycle lock. If present, it returns that exact value immediately; the
caller's observed success remains the only transition to replaceable `Closed`.
The test also injects and closes a late socket before retry, proving the second
snapshot equals the first rather than merely returning quickly.

The focused phase barrier passed 25 consecutive runs. Criterion detected no
empty-lease lifecycle regression at 1.349–1.368 milliseconds (`p=0.33`). The
extra state check and regression evidence move whole-tree
structural/cognitive complexity from 364/1,069 to 367/1,079.

The native factory and dependency audit passed 66 deterministic cases. A
clean-cache Rust 1.88 image passed the same factory and its serialized hostile
lane. Its 500-lease Linux smoke held eight descriptors and five threads while
live, returned to four descriptors and two threads, and finished at 3,940 KiB
RSS. Repeated prior image layers filled Docker's internal disk during the first
save attempt; removing only stopped and dangling Sandbox Egress build output
and the reproducible stale image made the clean rebuild succeed.

## 2026-08-31 — distinguish the CONNECT header-count ceiling

### Finding

CONNECT parsing already used `httparse` with a fixed 64-element header array,
so the work and stack space were bounded. Header 65 failed closed, but the
generic error arm labeled `httparse::Error::TooManyHeaders` as
`malformed-header`. That hid the intentional resource ceiling from both the
wire response and structured diagnostics.

### Result

The parser now names its 64-header constant and maps only that mature-parser
error to the bounded `too-many-headers` reason. A unit boundary accepts 64 and
rejects 65. A real listener case sends guest-controlled field names and values,
then requires one denial, the stable response reason, and the same diagnostic
code with none of those values copied into the event. It passed 20 consecutive
runs.

The native factory and dependency audit passed 68 deterministic cases. The
exact Rust 1.88 image passed the same factory and its serialized hostile lane.
Its 500-lease Linux smoke held eight descriptors and five threads while live,
returned to four descriptors and two threads, and finished at 3,996 KiB RSS.
The constant, one production match arm, and boundary evidence move whole-tree
structural/cognitive complexity from 367/1,079 to 369/1,084.

## 2026-08-31 — reject unusable and panic-shaped host limits

### Finding

Policy construction rejected zero DNS and handshake deadlines, but proxy
configuration accepted a zero header deadline and started a listener whose
requests immediately exhausted that budget. More seriously, the infallible
global connection and DNS limit setters accepted `usize::MAX`, and the
fallible per-lease setter did too. Those values eventually reached
`tokio::sync::Semaphore::new`, which documents a finite `MAX_PERMITS` and
panics above it.

### Result

Proxy startup now rejects a zero header deadline. Infallible process setters
clamp both ends to `1..=Semaphore::MAX_PERMITS`; the already-fallible per-lease
setter returns `PolicyError::ConnectionLimitTooLarge` above that boundary.
Tests prove the exact maximum remains valid, the oversized policy fails with
the typed error, and an extreme clamped process configuration starts and shuts
down a real proxy.

The native factory and dependency audit passed 71 deterministic cases. The
exact Rust 1.88 image passed the same factory and its serialized hostile lane.
Its 500-lease Linux smoke held eight descriptors and five threads while live,
returned to four descriptors and two threads, and finished at 4,000 KiB RSS.
The validation branches and boundary tests move whole-tree
structural/cognitive complexity from 369/1,084 to 371/1,090.

## 2026-08-31 — collapse duplicate approved DNS dial targets

### Finding

DNS cardinality was bounded and every result was checked before dialing, but a
legal 64-address answer containing the same approved IP 64 times produced 64
sequential connection attempts. The absolute handshake deadline bounded total
time, yet a hostile or broken resolver could still amplify connector work for
no routing benefit. A recording-connector test reproduced all 64 attempts.

### Result

Resolution still validates the complete answer and rejects the whole set when
any address is forbidden. Approved `SocketAddr` values are then deduplicated
with a bounded set while retaining first-seen resolver order. The regression
now observes one attempt and passed 20 consecutive runs. The mixed public plus
IPv4-compatible metadata case was upgraded to the same recording connector and
proves a forbidden later record yields zero attempts, not a partial early dial.

The native factory and dependency audit passed 72 deterministic cases. The
exact Rust 1.88 image passed the same factory and its serialized hostile lane.
Its 500-lease Linux smoke held eight descriptors and five threads while live,
returned to four descriptors and two threads, and finished at 3,988 KiB RSS.
The bounded ordered set, validation loop, and recording test move whole-tree
structural/cognitive complexity from 371/1,090 to 374/1,098.

## 2026-08-31 — verify the assembled crate in the local factory

### Finding

CI's release lane used verified `cargo package`, but the ordinary local factory
passed `--no-verify`. It proved the archive could be assembled while skipping
the clean compile from that archive—the check that catches an incomplete
`include` list or source code that accidentally depends on an unshipped file.
Cargo also warned that the manifest declared no documentation, homepage, or
repository metadata.

### Result

The local factory now runs verified `cargo package --allow-dirty`, aligning the
usual contributor loop with CI. The current assembled crate compiled
successfully in 9.14 seconds cold and 1.28 seconds warm from its isolated
package directory. The manifest declares the stable future docs.rs URL without
guessing a GitHub owner or repository URL that has not been chosen yet.

The full native factory passed all 72 deterministic cases and the dependency
policy gate. A clean-cache container build repeated the factory on the exact
Rust 1.88 MSRV, explicitly packaged 53 files and compiled from
`target/package/sandbox-egress-0.1.0`; the serialized hostile-input lane then
passed the same 72 cases. Its 500-lease Linux smoke held eight descriptors and
five threads while live, returned to four descriptors and two threads, and
finished at 4,012 KiB RSS. This factory-only change leaves whole-tree
structural/cognitive complexity at 374/1,098.

## 2026-08-31 — measure the opt-in visible-SNI path

### Question

The existing connection benchmark proved that linking the optional TLS parser
did not slow the default path, but it never exercised the parser. Can an
end-to-end comparison isolate the cost of actually enforcing visible SNI
without confusing DNS, dialing, or incomplete tunnel setup for parser work?

### Result

A paired Criterion fixture now sends the same valid `localhost` ClientHello
through hostname CONNECT with inspection disabled and with visible-SNI
enforcement enabled. Both wait for an upstream acknowledgement, and the
upstream asserts exact byte-for-byte receipt. An initially underspecified
ClientHello was rejected by rustls; adding its supported-version and
signature-algorithm extensions made the fixture valid and kept that fail-closed
observation out of the retained measurement.

Across four native M1 runs, the uninspected case produced 132.66–176.76
microsecond confidence intervals and the inspected case produced
142.55–164.60 microsecond intervals. Their broad overlap means socket, DNS, and
scheduler noise dominate this macrobenchmark; no precise inspection surcharge
or optimization claim is justified. The benchmark is retained as a realistic
regression signal, while an optimization is not. The native factory passed all
72 deterministic cases, including debug execution of both new benchmark paths,
and verified the assembled 54-file crate. The benchmark fixture moves
whole-tree structural/cognitive complexity from 374/1,098 to 381/1,116 without
changing library code. The exact Rust 1.88 factory and serialized lane passed
the same cases; its 500-lease Linux smoke returned to four descriptors and two
threads at 4,056 KiB RSS. Cargo verified 54 packaged files in the native Git
checkout and 53 in the clean container, where the VCS metadata file is absent.

## 2026-08-31 — make header acquisition linear

### Finding

The CONNECT byte ceiling bounded memory, but `read_header` searched the entire
accumulated vector after every 4 KiB socket read. A guest sending the permitted
1 MiB maximum without a terminator therefore induced quadratic comparison work
before receiving its ordinary 431 denial.

Three retained pre-change Criterion runs took 43.857–44.377 milliseconds to
send that header through a real socket and observe the denial.

### Result

Header acquisition now searches only bytes added by the latest read plus the
three preceding bytes required to detect a split `\r\n\r\n`. Four post-change
runs took 646.80–679.72 microseconds, a repeatable 64–69x improvement. The full
connection benchmark found no measurable regression in normal allowed,
hostname, visible-SNI, or denied requests.

A deterministic unit case places the terminator at all five positions around
the 4 KiB boundary and reconstructs following tunnel bytes from the buffer and
unread input. Production structural/cognitive complexity moves only from
134/412 to 135/415 in `src/proxy.rs`; whole-tree complexity moves from
381/1,116 to 383/1,122 because the retained benchmark and test carry most of
the additional proof.

The native and clean-cache exact Rust 1.88 factories passed all 73 deterministic
cases and verified the assembled crate; the serialized hostile-input lane
passed the same set. The 500-lease Linux smoke returned from eight descriptors
and five threads while live to four descriptors and two threads, finishing at
4,072 KiB RSS.

## 2026-08-31 — anchor handshake time at socket acceptance

### Finding

Each connection used one absolute deadline across header acquisition, DNS,
dialing, optional ClientHello inspection, and initial forwarding. Its origin,
however, was captured inside the spawned connection task. Scheduler delay
between listener admission and the task's first poll therefore granted the
guest extra time that was absent from the documented handshake budget.

The adjacent parser audit found no comparable change worth retaining. The
CONNECT acquisition layer requires `CRLF CRLF`, so the HTTP parser's tolerant
LF-only mode cannot reach policy evaluation. Leading empty lines and bytes
after the first header terminator do not change the parsed authority; the
latter are tunnel payload to the already-approved address. TLS acquisition
already feeds Rustls only newly received bytes, then performs one bounded ECH
extension scan after syntactic acceptance. Custom syntax rejection or another
parser optimization would add machinery without tightening the policy promise.

### Result

The listener now captures a monotonic timestamp immediately after `accept` and
passes it through admission to the spawned connection task. Header and
handshake deadlines derive from that timestamp. A direct deterministic case
starts the task with a deliberately expired accept timestamp and no guest
bytes; it receives `408 header-timeout`, records one denial, and never reaches
DNS or dialing. The neighboring deadline suite passed all eight cases.

The native connection benchmark detected no performance change: allowed
loopback completed in 102.36–114.57 microseconds, hostname CONNECT in
146.82–181.04 microseconds, visible-SNI CONNECT in 147.01–153.50 microseconds,
hostname denial in 67.388–72.514 microseconds, and the 1 MiB header rejection
in 652.48–669.90 microseconds. Production and whole-tree complexity remain
unchanged at 135/415 and 383/1,122 structural/cognitive respectively.

The native and exact Rust 1.88 factories passed all 74 deterministic cases and
verified the assembled crate; the serialized Linux lane passed the same set.
Its 500-lease smoke returned from eight descriptors and five threads while live
to four descriptors and two threads, finishing at 3,992 KiB RSS.

## 2026-08-31 — require an actually quiet identity interval

### Finding

Close waited for tracked work and then slept through one fixed identity-reuse
interval. A socket accepted for the revoking identity during that sleep was
closed and counted, but did not restart the timer. A sufficiently deep old-run
backlog could therefore keep draining right up to close success, leaving less
than the configured interval between the last observed old socket and source
address reuse. Dropped-lease cleanup used the same fixed sleep.

### Result

Each lease now records a mutex-ordered, saturating revocation generation. Both
ordinary admission and the global-capacity path advance it when they reject a
socket in `Revoking`. Cleanup samples the generation around each interval and
restarts the full wait after any change. At `u64::MAX` it never certifies
quiescence, so exhaustion fails closed instead of wrapping. Explicit close and
best-effort reap share the same small helper.

A deterministic phase case injects an ordinary admission 100 milliseconds
into a 200 millisecond quiet period. It proves the original completion point is
missed, waits another full interval, includes the denial in final usage, and
passed five concurrent repetitions at 0.41 seconds each. The existing close,
retry, counter-freeze, DNS, dial, TLS, and blocked-tunnel cases remain green.

The public lifecycle fixture now adds a real old-source socket during
revocation and places the caller deadline between the original and restarted
completion points. It requires `DeadlineExceeded`, recovers the lease with the
new denial visible, receives exactly `IdentityInUse` when it attempts a
replacement, and closes successfully after arrivals stop. Five concurrent
runs passed before retention.

This does not claim to identify arbitrary late packets: TCP carries no run
generation. The host must still fence the old namespace/NAT/conntrack path
before close, and must not reassign the source address until close succeeds.
The resettable interval strengthens observable backlog drainage inside that
contract. Criterion found no empty-lease regression at 1.3543–1.3684
milliseconds (`p=0.88`).

The lifecycle code and proof move whole-tree structural/cognitive complexity
from 383/1,122 to 393/1,149. The native and exact Rust 1.88 factories passed all
75 deterministic cases and verified the assembled crate; the serialized Linux
lane passed the same set. Its 500-lease smoke returned from eight descriptors
and five threads while live to four descriptors and two threads, finishing at
4,076 KiB RSS.

## 2026-08-31 — do not reinterpret bracketed hosts

### Finding

The maintained HTTP URI parser validates bracket placement and character
shape, but deliberately does not prove that bracket contents are IPv6. The
CONNECT layer unconditionally stripped a surrounding pair. A failing
regression demonstrated that `CONNECT [example.com]:443` therefore became the
ordinary DNS hostname `example.com`; bracketed IPv4 and IPvFuture text had the
same class ambiguity. Later hostname and address policy still applied, so this
was not a broad SSRF bypass, but it violated the crate's precise authority
promise and could make an operator's syntax mistake mean something else.

The surrounding hostname audit found no second defect: policy, CONNECT, and
visible SNI share ASCII lowercase/trailing-dot canonicalization; wildcard
matching requires a dot boundary and excludes the apex; resolver names are
absolute; and every answer remains post-checked. A fresh comparison with the
official IANA IPv4 and IPv6 special-purpose registries found the conservative
address floor current, including the 2025 dummy IPv6 prefix.

### Result

After generic authority parsing, bracketed host contents must parse as
`Ipv6Addr` before brackets are removed. DNS names, IPv4, IPvFuture, and scoped
zone syntax fail with `invalid-ipv6-literal`; ordinary hostname and bracketed
IPv6 behavior is unchanged. Unit cases pin all four rejected classes and a
real listener case pins the bounded 400 denial reason.

The change moves whole-tree structural/cognitive complexity from 393/1,149 to
397/1,160. The native and exact Rust 1.88 factories passed all 77 deterministic
cases and verified the assembled crate; the serialized Linux lane passed the
same set. Its 500-lease smoke returned from eight descriptors and five threads
while live to four descriptors and two threads, finishing at 4,132 KiB RSS.

## 2026-08-31 — execute the thin wrapper in the factory

### Finding

The all-target native factory compiled the executable but did not execute it.
The serialized container conformance lane named its integration test binaries
individually, so adding a process-level test without extending that list would
also leave the packaged wrapper unproved on the pinned Linux toolchain.

### Result

A dedicated process test now pins the two stable wrapper edges: no host policy
exits with status 2 and an exact usage line, while an allowed host with stdin
already at EOF starts the shared proxy, prints its loopback endpoint, revokes
the lease, and reports final usage before exiting successfully. The serialized
container command explicitly includes this test binary.

The first native factory attempt stopped at the formatting gate, as intended;
formatting was applied before any later gate ran. The completed native and
exact Rust 1.88 factories passed all 79 deterministic cases and verified the
assembled crate. The rebuilt image executed all 79 cases through its default
command. Complexity remains 397/1,160, and the Linux 500-lease smoke returned
from eight descriptors and five threads while live to four descriptors and two
threads, finishing at 4,060 KiB RSS.

## 2026-08-31 — collapse mapped source-identity aliases

### Finding

The lease registry compared `PeerIdentity` values exactly. IPv4 `127.0.0.1`
and its IPv4-mapped IPv6 transport spelling `::ffff:127.0.0.1` could therefore
attach two immutable policies even though a dual-stack listener can present
those values as two representations of one effective source address. The new
ownership regression first failed because the second attachment succeeded.

### Result

One private canonicalization rule now converts only IPv4-mapped IPv6 to IPv4.
It runs symmetrically when the trusted host attaches a lease and when the
listener accepts a peer. Native IPv6 and deprecated IPv4-compatible forms stay
distinct. The registry regression now returns the exact `IdentityInUse` error;
a real dual-stack listener routes an IPv4 client into the canonical IPv4 lease;
and the existing IPv6 destination case now also uses an IPv6 listener, source
identity, client, and upstream.

The added accept-path branch produced no detected connection-setup change:
allowed loopback measured 109.13–112.41 microseconds (`p=0.19`), hostname
CONNECT 142.17–157.82 microseconds (`p=0.05`), visible-SNI CONNECT
142.19–147.22 microseconds (`p=0.11`), hostname denial 64.728–72.314
microseconds (`p=0.71`), and 1 MiB header rejection 650.22–710.34 microseconds
(`p=0.30`). Whole-tree structural/cognitive complexity moves from 397/1,160 to
398/1,163; `identity.rs` accounts for 2/4.

The native and exact Rust 1.88 factories passed all 82 deterministic cases and
verified the assembled crate; the serialized Linux lane passed the same set.
Its 500-lease smoke returned from eight descriptors and five threads while live
to four descriptors and two threads, finishing at 4,056 KiB RSS.

The first exact-toolchain build ran every debug case successfully, then filled
Docker's internal disk while linking the release resource target. The stopped
build container and only the explicitly inventoried, dangling Sandbox Egress
build images were removed; unrelated tagged images and volumes were retained.
The clean rebuild then passed the complete factory and image command.

## 2026-08-31 — make wildcard depth an explicit promise

### Question

`HostPattern::Subdomains` matched every depth below its suffix, but the README
called the syntax a left-most wildcard. That wording could be read as the
single-label rule commonly associated with TLS certificates. Narrowing it
without research would silently revoke working policy and diverge from the
project's primary inspiration.

### Result

The pinned Smokescreen source and its ordinary tests explicitly treat
`*.example.com` as any non-apex subdomain, including
`more.contrived.example.com`. Sandbox Egress already used the same dot-boundary
suffix rule, so no production change was justified. Public API and README text
now say one or more complete left-hand labels, and the existing unit case pins
both one-label and nested matches plus apex and false-suffix rejection.

The clarification leaves whole-tree complexity at 398/1,163. The native and
exact Rust 1.88 factories passed all 82 deterministic cases and verified the
assembled crate; the serialized Linux lane passed the same set. Its 500-lease
smoke returned from eight descriptors and five threads while live to four
descriptors and two threads, finishing at 4,072 KiB RSS.

## 2026-08-31 — carry GREASE through the inspected TLS path

### Question

The maintained Rustls parser handled unknown values by design, but the local
fixtures contained only the minimum known cipher suite and extensions plus the
separately tested ECH value. The suite did not prove that a reserved GREASE
value remains compatible or that the focused post-parse ECH scan distinguishes
it from the registered ECH extension.

### Result

A fixed valid ClientHello now carries `0x0a0a` as both an offered cipher suite
and a zero-length extension before SNI. Rustls accepts the message, visible SNI
is recovered, and ECH remains false. The attractive-path proxy case now uses
that same message and proves it reaches the controlled upstream byte-for-byte
with normal accounting. No production parser or policy behavior changed.

The reusable test fixture and focused case move whole-tree
structural/cognitive complexity from 398/1,163 to 401/1,172, all in test-only
TLS code. The broader real-client corpus remains open rather than treating one
GREASE shape as universal compatibility evidence.

The native and exact Rust 1.88 factories passed all 83 deterministic cases and
verified the assembled crate; the serialized Linux lane passed the same set.
Its 500-lease smoke returned from eight descriptors and five threads while live
to four descriptors and two threads, finishing at 4,072 KiB RSS.

## 2026-08-31 — check network-specific NAT64 before dialing

### Finding

The address floor decoded mapped and compatible IPv6 plus the well-known
`64:ff9b::/96` NAT64 prefix. RFC 6052 also permits a network operator to route
a network-specific `/32`, `/40`, `/48`, `/56`, `/64`, or `/96`. A DNS64 answer
under such a global prefix therefore looked like ordinary public IPv6 even
when its effective IPv4 destination was private or link-local metadata.

This cannot be inferred safely from arbitrary IPv6 syntax. Translation-prefix
knowledge comes from the trusted host network and is shared across runs, so it
belongs in `ProxyConfig`, not guest input or an individual `Policy`.

### Result

`ProxyConfig::with_nat64_prefix` now registers each network-specific route.
Startup rejects prefix lengths outside RFC 6052's six layouts. DNS results
inside a registered prefix are decoded before the normal forbidden IPv4 check;
the well-known prefix remains automatic. Duplicate configuration is collapsed,
and a deliberate policy CIDR grant retains its existing floor-override
semantics.

Unit cases recover `192.0.2.33` from all six address examples published in RFC
6052. The end-to-end case supplies a globally shaped `/96` DNS answer embedding
`169.254.169.254`, requires `resolved-address-denied`, and proves the recording
connector receives zero dial attempts. A public embedded IPv4 remains allowed,
and startup rejects a syntactically valid but nonstandard `/80`.

Whole-tree structural/cognitive complexity moves from 401/1,172 to 409/1,202;
the additional decision shape is concentrated in the six-layout decoder and
its proofs. The first full benchmark replay flagged hostname denial even though
that path exits before address filtering. An isolated rerun measured
72.104–77.275 microseconds with `p=0.85`, so no change was detected. Allowed
loopback, hostname, visible-SNI, oversized-header, and empty-lease paths also
reported no detected change in the full replay.

The native and exact Rust 1.88 factories passed all 87 deterministic cases and
verified the assembled crate; the serialized Linux image ran the same set. Its
500-lease smoke held eight descriptors and five threads while live, returned to
four descriptors and two threads, and finished at 4,044 KiB RSS.

## 2026-08-31 — make every port grant explicit

### Finding

`Policy::builder()` described a deny-by-default policy, but its port set began
with 443. Because `allow_port` is additive, a caller constructing an apparently
HTTP-only policy with `allow_port(80)` silently retained HTTPS access. The
first regression test reproduced the mismatch by requiring an untouched
builder to deny 443; it failed before the implementation changed.

### Result

The library policy now starts with no allowed ports. Every port is an explicit
host-side grant. The thin executable deliberately opts into 443 so its familiar
single-policy example remains useful without weakening the reusable library
default. A real-socket integration case grants loopback and port 80, attempts
port 443, requires the structured `port-denied` response, and checks the final
denial counter. Existing cases and benchmarks that intentionally exercise a
later phase now grant 443 explicitly.

Changing only the initial `BTreeSet` unexpectedly moved the full-LTO 1 MiB
header-rejection benchmark from the parent commit's 638.83–646.77 microseconds
to 1.018–1.279 milliseconds, even though that path never consults the port set.
Using `BTreeSet::default()` did not help; temporarily restoring the hidden 443
recovered 655.77–660.12 microseconds and isolated this as whole-program code
layout sensitivity rather than runtime policy work. A measured, non-inlined
boundary around the hostile-input header scan preserves the correct empty
default and recovered 640.59–648.71 microseconds in the focused replay. The
final full replay measured 659.77–695.71 microseconds, within its established
historical range. Allowed loopback, allowed hostname, visible-SNI, hostname
denial, and empty-lease benchmarks reported no detected change.

Whole-tree structural/cognitive complexity remains 409/1,202. The native and
exact Rust 1.88 factories passed all 89 deterministic cases and verified the
assembled crate; the serialized Linux image ran the same set. Its 500-lease
smoke held eight descriptors and five threads while live, returned to four
descriptors and two threads, and finished at 4,072 KiB RSS. During the first
exact rebuild, Docker's legacy builder retained a failed 1.85 GiB intermediate
container after exhausting its internal disk. Only inspected, superseded
Sandbox Egress build state was removed; the clean rebuild then passed without a
source change.

## 2026-08-31 — make CONNECT authority single-source

### Finding

The earlier CONNECT authority review deliberately left Host-field strictness
open. RFC 9112 requires every HTTP/1.1 request to carry exactly one valid Host
field and reconstructs an authority-form target from the request-target, not
that field. The proxy already selected policy and DNS only from the target, but
accepted a missing, repeated, malformed, or contradictory Host. That left two
guest-controlled authority spellings in one otherwise bounded message.

The first parser regression required a Host-less HTTP/1.1 CONNECT to return
`missing-host-header`; it failed against the permissive implementation. The
protocol decision follows [RFC 9112 sections 3.2 and
3.3](https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2), while retaining a
deliberate HTTP/1.0 compatibility path.

### Result

HTTP/1.1 now requires exactly one syntactically valid Host whose hostname
agrees with the CONNECT request-target. If Host supplies a port, it must agree
too; omitting that port remains valid, matching the RFC's CONNECT examples.
DNS names compare case-insensitively and equivalent IPv6 text compares by
address value. HTTP/1.0 can still omit Host. Missing, case-insensitively
duplicated, malformed, hostname-mismatched, and port-mismatched fields receive
distinct bounded parse reasons before policy or DNS. The returned authority is
still exclusively the request-target, so no guest header can select identity,
policy, resolution, or dialing.

Unit cases pin both rejection and compatible spellings. A real socket requires
`400 host-header-mismatch` and one final accounted denial. All normal traffic
fixtures now send standards-valid Host fields, and the 64-field ceiling counts
Host as one of those fixed parser slots.

The full connection benchmark detected no regression: allowed loopback was
110.65–126.93 microseconds (`p=0.05`, no detected change), allowed hostname
137.59–150.32, visible-SNI 151.54–161.73, and hostname denial 72.46–82.76.
Because the first loopback result was borderline noisy, an isolated same-tree
replay measured 102.76–111.11 microseconds. The 1 MiB header scan was faster in
the full sample, but is treated as code-layout noise rather than an improvement
caused by Host validation.

The shared validation and its proofs move whole-tree structural/cognitive
complexity from 409/1,202 to 428/1,248, concentrated in `connect.rs`. The native
and exact Rust 1.88 factories passed all 92 deterministic cases and verified
the assembled crate; the serialized Linux image ran the same set. Its
500-lease smoke held eight descriptors and five threads while live, returned to
four descriptors and two threads, and finished at 4,060 KiB RSS.

## 2026-08-31 — give every approved address a dial chance

### Finding

Approved DNS results were fully checked, bounded, deduplicated, and dialed in
resolver order, but every attempt received the same absolute handshake
deadline. If the first connector future stayed pending, it consumed the whole
deadline and a reachable second address was never attempted. The fail-first
case held address one pending under a 400 ms deadline and mapped address two to
a local listener; the old loop returned no connection after observing only
address one.

[RFC 8305 section 5](https://www.rfc-editor.org/rfc/rfc8305.html#section-5)
addresses this family of failure with staggered parallel connection attempts.
That design was not retained here: it would allow one admitted guest connection
to own several live upstream sockets and would add another cancellation and
global-budget subsystem. The narrower requirement is to prevent one approved
answer from starving the rest while preserving a one-live-dial invariant.

### Result

Before each sequential attempt, the dialer divides the remaining absolute
handshake time by the number of addresses not yet tried. A pending address can
consume only that fair share; an immediate connector error advances without an
artificial delay; the final address receives all time still available. The
current connector future remains inside the lease-owned connection task, so
close still drops it synchronously and cannot start the next attempt. No task,
socket, configuration option, or dependency was added.

The deterministic pending-first/reachable-second case now connects through the
second address in resolver order and passed ten consecutive runs. Existing
single-address close and absolute-deadline cases still pass, proving a lone
address keeps the complete remaining deadline and revocation still cancels its
pending dial.

The full connection benchmark detected no regression: allowed loopback was
103.19–122.72 microseconds (`p=0.81`), allowed hostname 131.22–140.11
(`p=0.07`), and hostname denial 65.93–79.19 (`p=0.06`). Visible-SNI and the
1 MiB header scan measured faster, but are treated as unrelated sample noise.

The fair-share loop and controlled connector proof move whole-tree
structural/cognitive complexity from 428/1,248 to 433/1,265. The native and
exact Rust 1.88 factories passed all 93 deterministic cases and verified the
assembled crate; the serialized Linux image ran the same set. Its 500-lease
smoke held eight descriptors and five threads while live, returned to four
descriptors and two threads, and finished at 3,996 KiB RSS.

## 2026-08-31 — preserve lease certificates across proxy shutdown

### Finding

`Proxy::shutdown` already stopped the listener, drained every lease tracker,
captured final counters, and marked each lease closed before joining the owned
runtime. A `Lease` retained by the embedding service could not observe that
completed work, however: its later `close` tried to send to the stopped runtime
and returned `RuntimeStopped`. The API therefore hid an existing certificate
and left the caller holding an identity that was already safely closed.

The fail-first integration case sent one denied CONNECT, completed proxy-wide
shutdown, and then asked the surviving lease for its certificate. The old
implementation failed with `CloseErrorKind::RuntimeStopped` instead of the
known final counters.

### Result

A lease can now consume a local counter snapshot only after its shared phase is
`Closed`. That phase is committed only after the lease tracker is empty, so no
connection, DNS lookup, dial, tunnel direction, or counter writer can survive
the snapshot. The same check resolves the narrow races where the command send
fails or its reply channel disconnects as shutdown finishes. A receive timeout
deliberately does not infer success: the deadline boundary still returns the
owning lease so the caller can retry.

The real-socket regression proves the surviving lease observes exactly one
accepted connection, one denial, and zero active connections after successful
proxy shutdown. The empty-lease lifecycle benchmark measured 1.3382–1.3530
milliseconds, a change of -2.85% to -0.83%; Criterion classified it within the
configured noise threshold, so this is recorded as no regression rather than
an improvement.

The local certificate path and its integration proof move whole-tree
structural/cognitive complexity from 433/1,265 to 439/1,283. The native and
exact Rust 1.88 factories passed all 94 deterministic cases and verified the
assembled crate; the freshly serialized Linux image ran the same set. Its
500-lease smoke held eight descriptors and five threads while live, returned to
four descriptors and two threads, and finished at 4,052 KiB RSS. A first Linux
export exhausted Docker's internal disk after the checks had passed; the older
project image and two disposable build containers were removed, then a clean
rebuild and serialized run proved that the tagged image contained all 94 cases.

## 2026-08-31 — serialize a small conformance image

### Finding

The Docker factory correctly pinned Rust 1.88, ran the complete check lane and
resource smoke, and reran conformance from the resulting tag. But that tag was
the factory itself: it included Rust, Cargo's registry, source, documentation,
and the entire debug and release target trees. Docker reported
1,129,028,857 bytes of image content, and repeated 5 GB virtual build layers
were the direct cause of the local disk-pressure failures seen during exact
rebuilds.

The first multi-stage attempt copied the five stripped test executables into a
minimal runner. Its 62 library cases passed as an unprivileged user, but both
CLI cases failed because the integration test embeds Cargo's absolute path to
the standalone `sandbox-egress` executable. Dropping those cases would have
made the smaller artifact weaker, so that attempt was not accepted.

### Result

The Rust factory stage is unchanged: it still runs format, compile, lint, all
targets, doctests, documentation, package assembly, dependency policy when
available, and the 500-lease Linux resource smoke. A small checked script now
asks Cargo for each selected test artifact in JSON form, filters by target
kind, requires exactly one match, copies and strips it. The standalone CLI is
also placed at the exact path embedded in its integration test. The final
Debian stage contains only these files and runs as UID/GID 65534; it has no
compiler, Cargo registry, source tree, or build cache.

The serialized image passes the same 94 deterministic cases, including both
process-level CLI cases. Docker's content-size field fell from 1,129,028,857 to
40,301,120 bytes, a 96.4% reduction. The final exact build's 500-lease smoke
held eight descriptors and five threads while live, returned to four
descriptors and two threads, and finished at 4,076 KiB RSS. Rust source
complexity remains 439 structural / 1,283 cognitive; the change is isolated to
the factory and two POSIX shell helpers.

Two rebuilds exhausted Docker's internal disk while committing already-passed
factory layers. Only inspected Sandbox Egress containers and intermediate
images from this session were removed. The last clean build selected artifacts
without probing unrelated executables, completed without noisy side effects,
and reproduced the unprivileged 94-case run from the tagged image.

## 2026-09-01 — drain the accept queue before identity handoff

### Finding

The identity quiet period only observed sockets after the Tokio listener
accepted them. The runtime loop deliberately prioritized management commands,
so a continuously ready command channel could leave an already-established
old-source socket in the kernel accept queue until after close succeeded. Policy
selection happens at accept time. After the host attached a replacement lease,
that old socket was therefore interpreted as new-run traffic.

A deterministic test command kept the management branch ready for 300
milliseconds while an old-source client queued a CONNECT request. The old
implementation completed a 100 millisecond quiet close, installed a replacement
policy that allowed the local target, and returned the exact leak signal:
`HTTP/1.1 200 Connection Established`. Ten retained repetitions pass after the
fix. Making branch selection merely probabilistically fair was not accepted as
a lifecycle certificate.

### Result

The listener owner now performs an explicit nonblocking drain after each
candidate quiet interval. Ready sockets are dispatched under the still-current
registry; an old socket is refused in `Revoking`, counted against the old lease,
and advances the generation so another complete interval is required. The
drain processes at most 256 sockets per command. A full batch or accept error
is not evidence of emptiness: the same one-shot request is requeued, while an
expired close cancels its receiver so no orphan command circulates. Best-effort
reaping shares the barrier.

Attachment independently requires an empty ready-queue poll on that same
listener-owner task before it installs any mapping. This closes the handoff
gap without pretending TCP contains a run generation. The host must still
fence the old namespace/NAT path before close; an arbitrarily delayed packet
beyond the configured interval remains outside what a shared TCP listener can
authenticate.

The empty attach/close benchmark measured 1.3642–1.3907 milliseconds, a 2.16%
median increase that Criterion classified within its noise threshold. This is
the measured cost of two lifecycle barriers; the connection data path is
unchanged. Whole-tree complexity moved from 439/1,283 to 460/1,383
structural/cognitive.

The native and exact Rust 1.88 factories passed all 96 deterministic cases,
doctests, documentation, and package verification. The serialized runner
passed the same 96 cases as UID/GID 65534 and measured 40,350,627 bytes. The
500-lease Linux smoke held eight descriptors and five threads while live,
returned to four descriptors and two threads, and finished at 4,012 KiB RSS.

## 2026-09-01 — serialize concurrent identity attachment

### Prior-art check

The pinned Smokescreen, lens-sandbox-core, and nono sources were compared at
their admission and shutdown boundaries. Lens shares a semaphore across two
listener tasks and reserves a permit before spawning. Nono's count check and
increment are safe from overshoot in its present shape because one accept task
executes both without a suspension point. Nono's shutdown signal does not join
its accept task or its spawned handlers, while Lens's handlers are likewise
detached process-lifetime work. Smokescreen has a stronger process-wide
connection tracker, but no live-listener handoff of one source identity between
ephemeral policies.

The comparison supported the existing design rather than an implementation
change: Sandbox Egress reserves both process and lease permits, obtains the
lease tracker token before spawning, and serializes registry mutations on its
single listener-owner command loop. A second shared registry lock would add
coordination without strengthening the ownership proof.

### Contention proof

A new integration case releases 32 host threads from one barrier into
`Proxy::attach` for the same source address. It requires exactly one lease to
win, all 31 losing calls to return `IdentityInUse`, and a later attach to remain
refused until the winner completes certified close. The case passed ten
consecutive focused runs, the native factory, the exact Rust 1.88 Linux
factory, and the unprivileged serialized runner.

No production source changed. Whole-tree SCC 4.0.0 complexity moved from
460/1,383 to 464/1,393 structural/cognitive, entirely in the conformance test.
The native 500-lease smoke returned from 13 descriptors and five threads while
live to nine descriptors and two threads, finishing at 8,848 KiB RSS. The
Linux factory's 500-lease smoke returned from eight descriptors and five
threads to four descriptors and two threads, finishing at 4,020 KiB RSS.

All 97 deterministic cases, doctests, documentation, package verification,
and dependency policy checks passed natively. The runner reproduced the same
97 cases as UID/GID 65534; image content size was 40,348,913 bytes. The small
size change from the preceding image is treated as build-layout noise, not a
performance result.

## 2026-09-01 — distinguish half-close from reset

### Question

Certified revocation already proved that cancelling a tunnel destroys both
socket directions without waiting for either peer. That does not establish the
ordinary data-plane behavior before revocation: a graceful EOF in one direction
must not truncate valid traffic still moving in the other direction, while a
reset must not be reported as normal completion or a policy decision.

Tokio 1.53.1's locked `copy_bidirectional` source keeps one state machine per
direction. EOF advances only the corresponding writer through shutdown; the
reverse copy continues. Any I/O error instead returns immediately. Three real
loopback cases now pin how Sandbox Egress uses those semantics:

- after the guest sends seven bytes and finishes upload, the upstream returns a
  delayed 16-byte response;
- after the upstream sends eight bytes and finishes download, the guest sends
  an 11-byte late upload;
- an upstream proves receipt of five bytes and then closes with zero linger,
  producing a terminal reset rather than a graceful completion.

Both FIN orders finish as one completed tunnel with exact bidirectional byte
counters. The reset finishes with zero active, zero completed, zero denied,
five uploaded, and zero downloaded. Each focused case passed ten consecutive
runs. No production source or data-path configuration changed.

### Evidence

The native and exact Rust 1.88 Linux factories passed all 100 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Whole-tree SCC 4.0.0 complexity moved from 464/1,393
to 467/1,399 structural/cognitive, entirely in `tests/tunneling.rs`.

The native 500-lease smoke returned from 13 descriptors and five threads to
nine descriptors and two threads, finishing at 8,944 KiB RSS. The Linux smoke
returned from eight descriptors and five threads to four descriptors and two
threads, finishing at 4,020 KiB RSS. The serialized runner reproduced all 100
cases as UID/GID 65534 and measured 40,359,099 bytes; the roughly 10 KiB increase
is test payload, not a proxy performance result.

## 2026-09-01 — refresh the address floor and reject config indirection

### Registry refresh

The production prefix table was checked against the current IANA
[IPv4](https://www.iana.org/assignments/iana-ipv4-special-registry) and
[IPv6](https://www.iana.org/assignments/iana-ipv6-special-registry)
special-purpose registries, both last updated 2025-10-09. No newly unguarded
destination class was found. The code remains deliberately conservative: it
rejects the full `192.0.0.0/24` and `2001::/23` assignment umbrellas by default,
including globally reachable exceptions that an operator may explicitly grant.
Only the source review date changed.

### Rejected allocation optimization

Each admitted task currently owns a clone of immutable `ProxyConfig`. A trial
stored the config in `Arc` instead, replacing any configured NAT64-prefix vector
copy with an atomic reference-count increment. To make the copy nontrivial, the
existing real-loopback CONNECT benchmark temporarily registered 64 distinct
NAT64 prefixes.

The pre-change repeated interval was 106.04–119.67 microseconds with a 111.72
microsecond point estimate. The `Arc` trial measured 109.32–110.98 microseconds
with Criterion's change interval spanning -5.87% to +3.15% (`p = 0.70`), then
109.82–111.53 microseconds on repetition. Criterion detected no improvement.
Local socket setup dominates the vector copy, while `Arc` would add shared
ownership and an atomic operation to every connection. The production and
temporary benchmark changes were therefore removed exactly; this log is the
only retained result.
