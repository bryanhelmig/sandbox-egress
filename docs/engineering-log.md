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

## 2026-09-01 — retain the proxy after shutdown failure

### Failure model

`Proxy::shutdown` previously consumed the management handle even when its
deadline expired. The runtime also exited after processing that shutdown
request. A surviving lease still retained its identity, but the caller had no
live proxy with which to retry cleanup and certify the final process state.
That made the strongest shutdown operation less recoverable than `Lease::close`.

The public operation now returns a typed `ShutdownError` that owns the
still-stopping `Proxy`. Once shutdown begins, the listener owner permanently
rejects new attachments and disables ordinary socket admission. Accept-drain
barriers may still take already queued sockets only to reject them under their
revoked lease; they cannot start new work. A caller can recover the handle with
`ShutdownError::into_proxy` and retry with a later deadline.

There is a second race at the success boundary: cleanup may finish just as the
caller's deadline expires. The public request therefore uses a zero-capacity
reply channel. The runtime exits on success only after the caller receives the
certificate. If the receiver has already gone away, the runtime remains in the
stopping state for a retry. `Drop` remains deliberately weaker: it initiates a
non-retryable best-effort shutdown and lets the runtime exit even when cleanup
misses that internal deadline, because no handle exists to own another retry.

### Evidence

The first deterministic case delays cancellation cleanup, proves that a short
shutdown returns `DeadlineExceeded`, recovers the same proxy, proves a new
identity is rejected with `ProxyStopping`, and then completes a longer retry.
The second queues an otherwise successful shutdown whose reply receiver has
gone away, proves the runtime remains stopping, and certifies it through a
public retry. The focused proxy-shutdown set passed ten consecutive runs.

The native and exact Rust 1.88 Linux factories passed all 102 deterministic
cases, doctests, documentation, package verification, and policy checks where
the policy tool was installed. The serialized runner reproduced the same 102
cases as UID/GID 65534; its image content size was 40,371,751 bytes.

Whole-tree SCC 4.0.0 complexity moved from 467/1,399 to 480/1,447
structural/cognitive. Most of the increase is the two lifecycle cases and the
typed public error; the runtime change is one irreversible state bit, one
admission guard, and one success-delivery check. The native 500-lease smoke
returned from 13 descriptors and five threads while live to nine descriptors
and two threads, finishing at 8,960 KiB RSS in 880 ms. Linux returned from
eight descriptors and five threads to four descriptors and two threads,
finishing at 3,960 KiB RSS in 1,026 ms.

The first lifecycle benchmark interval was 1.3815–1.3967 ms; Criterion called
its +0.01% to +2.58% change within the configured noise threshold. An immediate
repeat measured 1.3373–1.3457 ms and reversed the apparent movement. This is
host variance, not evidence of either a regression or an improvement, so no
performance claim is attached to the change.

## 2026-09-01 — close the proxy/lease race matrix

The remaining proxy-shutdown race inventory had four meaningful combinations:
explicit `Proxy::shutdown` and best-effort `Proxy::drop`, each concurrent with
`Lease::close` and `Lease::drop`. Four barrier-synchronized cases now start a
real proxy connection, hold its dial pending in a controllable connector, and
release the two management operations together.

Both explicit-shutdown combinations require a successful proxy certificate;
the lease-close variant also requires its own zero-active certificate,
regardless of command order. The best-effort combinations retain the runtime
join handle only inside the unit-test boundary, then prove that the owned
runtime terminates. Both lease-drop combinations additionally hold a weak
lease-state reference and require its final strong owner to disappear. Every
case requires the pending dial token to be destroyed and the guest socket to
stop. The four-case set passed ten consecutive focused runs.

No production source changed. Whole-tree SCC 4.0.0 complexity moved from
480/1,447 to 490/1,479 structural/cognitive, entirely in the reusable pending
dial fixture, strict terminal-socket assertion, and four lifecycle cases. A
data-path benchmark was intentionally skipped because test-only code cannot
support a throughput claim.

The native factory passed all 106 deterministic cases, doctests,
documentation, package verification, and dependency policy checks. Its full
8,000-lease resource run returned from 13 descriptors and five threads while
live to nine descriptors and two threads, finishing at 8,976 KiB RSS in
11,608 ms. The exact Rust 1.88 Linux factory passed the same cases; its
500-lease lane returned from eight descriptors and five threads to four and
two, finishing at 3,968 KiB RSS in 1,080 ms. The serialized runner reproduced
106/106 as UID/GID 65534 and measured 40,388,758 bytes.

## 2026-09-01 — keep legacy numeric hosts behind the address floor

[RFC 3986](https://www.rfc-editor.org/rfc/rfc3986.html#section-3.2.2)
distinguishes IPv4 literals from registered names using a first-match rule.
Rust's documented [`Ipv4Addr` textual
representation](https://doc.rust-lang.org/std/net/struct.Ipv4Addr.html#textual-representation)
accepts four decimal octets and explicitly rejects legacy octal and hexadecimal
forms. In contrast, the [WHATWG URL host
parser](https://url.spec.whatwg.org/#concept-ipv4-parser) retains legacy
one-part, short dotted, octal, and hexadecimal IPv4 number handling. A proxy
must not let disagreement between those parsers become disagreement between
policy and dialing.

Sandbox Egress parses only standard Rust IP literals directly. Every other
accepted ASCII host is canonicalized and policy-matched as a name. The system
resolver receives an absolute name (the implementation appends the terminal
dot), and only returned `IpAddr` values cross the forbidden-address floor. The
dialer receives the resulting checked `SocketAddr`, never the original host
string.

A new end-to-end case grants each of `127.1`, `0177.0.0.1`, `0x7f000001`, and
`2130706433` as a hostname and allows port 443, but grants no private network.
A controlled resolver returns `127.0.0.1`. Every form must receive
`resolved-address-denied`, and a counting connector must remain untouched. The
case passed ten consecutive focused runs.

The first version of the case failed with `dial-failed`: it reused a loopback
happy-path policy helper that explicitly grants `127.0.0.0/8`. That behavior
was correct and exposed an invalid test premise. The case now constructs the
minimal deny-floor policy locally rather than weakening production behavior.

No production source changed and no data-path benchmark was run. Whole-tree
SCC 4.0.0 complexity moved from 490/1,479 to 491/1,482
structural/cognitive. The native and exact Rust 1.88 Linux factories passed all
107 deterministic cases, doctests, documentation, and package verification;
native dependency policy checks also passed. Linux's 500-lease lane returned
from eight descriptors and five threads to four and two, finishing at 3,960
KiB RSS in 1,017 ms. The rootless runner reproduced 107/107 as UID/GID 65534
and measured 40,391,595 bytes.

## 2026-09-01 — bound source-cycle Docker storage

A legacy Docker build exhausted its storage only after the Rust 1.88 factory
had passed tests, package verification, and the resource lane. Inspection found
one 1.9 GB failed Sandbox Egress container and several enumerated stale project
factory images; Docker reported 18.63 GB in images and 1.9 GB in the exited
container. Only those explicit project artifact IDs were removed. No broad
prune or unrelated image deletion was used.

The source-validation step needs `target/` while checking, but the next stage
copies only the already-collected executables under `/conformance`. The first
trial therefore deleted `target/` after successful collection in the same
layer. The source-dependent layer fell from 1.90 GB to 172 MB, and comparable
factory content reported by image inspection fell from 1,145,464,261 to
662,347,550 bytes. The rootless runner reused exactly the same output layers
and passed all 107 cases.

That trial still changed about 137 MB of Cargo registry state during package
verification. A second trial set `CARGO_NET_OFFLINE=true` only after the locked
dependency-warmup stage. The complete source check, documentation, package
verification, and resource lane succeeded without registry access. The
source-dependent layer fell again to 35.8 MB: 98.1% below the original. Factory
content measured 635,814,086 bytes, 44.5% below the original and 4.0% below the
first cleanup trial.

The retained runner remained byte-identical at image
`sha256:6019ada1245e1242842e1a6451aa1f456b788711c4078d0732b5179d4784ba08`
and 40,391,595 bytes, then reproduced 107/107 as UID/GID 65534. The final Linux
resource lane returned from eight descriptors and five threads to four and two,
finishing at 3,960 KiB RSS in 1,074 ms. No Rust source or complexity changed;
this is a factory storage and network-reproducibility improvement, not a proxy
throughput result.

## 2026-09-01 — prove lease Drop under unwind and runtime loss

`Lease::drop` is deliberately an infallible fallback, not a cleanup
certificate. It must nevertheless initiate cancellation without panicking when
the owner is already unwinding, and it must release local ownership when the
proxy command receiver no longer exists.

The first new deterministic case holds a dial pending, moves the lease into a
caught owner panic, and lets normal stack unwinding invoke Drop. It then
requires the dial token to disappear, the guest socket to reach a terminal
state, the old lease state to lose its final strong owner, and the same source
identity to become attachable under a replacement policy. The second stops and
joins the owned proxy runtime before dropping a lease, then proves that the
disconnected management channel neither causes a second panic nor retains the
lease state. The focused four-case lease-drop set passed ten consecutive runs.

No production source changed, so no data-path benchmark was run and no
performance claim is attached to this cycle. Whole-tree SCC 4.0.0 complexity
moved from 491/1,482 to 498/1,506 structural/cognitive, entirely in the two
lifecycle cases.

The native and exact Rust 1.88 Linux factories passed all 109 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 3,932 KiB RSS in
1,067 ms. The rootless runner reproduced 109/109 as UID/GID 65534; image
`sha256:24d9268478d7ae257942e9b52666076ea46388d78c1323ab4d73cfda318f2716`
measured 40,398,450 bytes.

## 2026-09-01 — keep repeated close failures ownership-stable

One public lifecycle case now records a real denied CONNECT, waits for its
connection accounting to settle, and then forces three consecutive
`DeadlineExceeded` results inside a 300 millisecond identity-reuse quiet
period. Each error must return the same lease sequence, preserve an exact
nonzero usage snapshot, and keep replacement attachment at `IdentityInUse`.
The final retry must certify the identical snapshot as `FinalUsage`.

The first exact Linux factory attempt exposed a race in the test premise. A
client that has read the denial through EOF can still observe the connection
guard immediately before its final active-counter decrement; that run captured
one active connection where the test expected zero. The proof now has an
explicit bounded wait for accounting quiescence before taking its baseline.
No proxy behavior was changed or weakened. The corrected focused case passed
ten consecutive runs.

No production source changed, so no data-path benchmark was run. Whole-tree
SCC 4.0.0 complexity moved from 498/1,506 to 502/1,514
structural/cognitive, entirely in the lifecycle proof.

The native and corrected exact Rust 1.88 Linux factories passed all 110
deterministic cases, doctests, documentation, and package verification; native
dependency policy checks also passed. Linux's 500-lease lane returned from
eight descriptors and five threads to four and two, finishing at 3,960 KiB RSS
in 1,067 ms. The rootless runner reproduced 110/110 as UID/GID 65534; image
`sha256:3828250dce921f4b076a5062deb9cfccdb4e9bed2c59e13a02bfae92ba3de590`
measured 40,402,723 bytes.

## 2026-09-01 — make the mark-bypass capability boundary explicit

The pinned `lens-sandbox-core` reference advanced from `a0a95786` to
`2bc4ecc5`. Its only network-relevant change was a documentation correction:
an `SO_MARK`-based nftables cage is forgeable by a process with either
`CAP_NET_ADMIN` or `CAP_NET_RAW`, not only the former. Linux
[`socket(7)`](https://man7.org/linux/man-pages/man7/socket.7.html) confirms that
`CAP_NET_RAW` has sufficed since Linux 5.17, while
[Docker's runtime documentation](https://docs.docker.com/engine/containers/run/#runtime-privilege-and-linux-capabilities)
lists `NET_RAW` among its default retained capabilities.

The public integration guidance and security invariants now require both
capabilities to be absent from every untrusted workload and sidecar sharing a
mark-governed network namespace. They also state that a non-root UID is not a
substitute for checking effective and bounding capability sets. Sandbox Egress
still does not install or certify the host cage; this clarification makes that
load-bearing deployment boundary harder to misconfigure without expanding the
crate.

No Rust source, dependency, test count, complexity, or performance result
changed. The source factory and documentation checks remain the appropriate
evidence for this research-only clarification.

## 2026-09-01 — align controlled DNS with absolute system lookups

Smokescreen's hostname path performs IDNA mapping before lookup, while the
existing Sandbox Egress contract deliberately accepts only canonical ASCII DNS
text. The narrower choice remains: explicit ACE/punycode spellings are valid,
but the policy layer does not silently transform raw Unicode or confusable
characters. New unit boundaries pin ASCII case, one trailing root dot,
63-byte labels, the 253-byte unrooted name ceiling, explicit ACE text, raw
Unicode, a non-ASCII confusable, underscores, edge hyphens, repeated dots, and
IP literals.

The system resolver already appended a root dot so local search suffixes could
not reinterpret a policy hostname. The controlled resolver interface received
the unrooted form, however, making local tests semantically weaker than the
production backend. A test written first expected `mixed.case.test.` and failed
with `mixed.case.test`. Absolute-name construction now happens before the
backend split, so system and controlled lookups receive identical lowercase,
rooted text. The focused end-to-end case passed ten consecutive runs.

The production system-resolver arm still performs the same one string
allocation and Hickory call; the functional change is confined to the private
test backend. No data-path benchmark was run and no performance claim is
attached. Whole-tree SCC 4.0.0 complexity moved from 502/1,514 to 504/1,519
structural/cognitive, almost entirely in the two boundary cases.

The native and exact Rust 1.88 Linux factories passed all 112 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 3,984 KiB RSS in
1,081 ms. The rootless runner reproduced 112/112 as UID/GID 65534; image
`sha256:6a5c358fc6b0527485a9a27185a502b3a4d7db130664f55aa90565b4750e3aa4`
measured 40,410,919 bytes.

## 2026-09-01 — retain the linear header scan under near matches

The CONNECT header reader previously became quadratic when a large header
arrived in many small reads. Retaining only the three-byte overlap needed to
find `\r\n\r\n` across read boundaries reduced the 1 MiB all-`a` case from
roughly 44 ms to 647–680 us. This cycle asks a different question: does an
attacker who supplies three of the four terminator bytes on every candidate
position recover a material CPU penalty?

The connection benchmark now sends exactly 1 MiB of repeated `\r\n\rX` over a
real socket, contains no complete terminator, and requires the same 431
response as the ordinary oversized-header case. Three paired runs of
`cargo bench --bench connections -- header_1mib` placed the ordinary input's
confidence intervals between 641.13 and 652.31 us. Two near-match intervals
were similarly tight at 640.98–649.91 us; one noisy interval widened to
648.65–743.29 us. Criterion reported no statistically significant change in
any paired run.

The first standalone ordinary-input measurement also illustrates why the
paired repetitions matter: its 650.96–741.15 us interval was initially labeled
a regression, while an immediate repeat returned 644.08–653.65 us and no
change. We are keeping the adversarial benchmark and the current scanner, not
claiming an optimization from measurement noise. The full benchmark lane also
found no change for allowed IP, allowed hostname, visible SNI, denied hostname,
or empty-lease lifecycle paths.

No production source changed. Whole-tree SCC 4.0.0 complexity moved from
504/1,519 to 505/1,522 structural/cognitive, entirely in benchmark code. The
native and exact Rust 1.88 Linux factories passed all 112 deterministic cases,
doctests, documentation, package verification, and benchmark smoke; native
dependency policy checks also passed. Linux's 500-lease lane returned from
eight descriptors and five threads to four and two, finishing at 4,032 KiB RSS
in 1,065 ms. The rootless runner reproduced 112/112 as UID/GID 65534. Because
the runtime runner excludes benchmark artifacts, its image remained
`sha256:6a5c358fc6b0527485a9a27185a502b3a4d7db130664f55aa90565b4750e3aa4`
at 40,410,919 bytes.

## 2026-09-01 — close both directions under simultaneous backpressure

The tunnel suite separately proved revocation while the guest continuously
writes to an upstream that never reads and while an upstream continuously
writes to a guest that never reads. Those cases did not prove the harder
combined state: both kernel send paths backpressured at once while the
bidirectional copy future owns both sockets.

A new real-socket case starts both hostile writers, requires both upload and
download counters to become nonzero, and then closes the lease. Certified close
must return within 500 ms, final active ownership must be zero, and both writer
threads must receive a terminal socket error. A write timeout is deliberately
not accepted as cleanup evidence. The focused case passed ten consecutive
runs.

The implementation already held the invariant, so no production source or
data-path benchmark changed. A small terminal-write assertion helper removes
duplicated platform error matching from the two earlier one-direction cases.
Whole-tree SCC 4.0.0 complexity moved from 505/1,522 to 510/1,535
structural/cognitive, entirely in conformance code.

The native and exact Rust 1.88 Linux factories passed all 113 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 3,960 KiB RSS in
1,073 ms. The rootless runner reproduced 113/113 as UID/GID 65534; image
`sha256:84b775c525dfe9a244fdc20238ea49a797bd0c1697358a3e3a5ac15fbfe59cd3`
measured 40,413,030 bytes.

## 2026-09-01 — reject ambiguous SNI hostname lists

[RFC 6066 section 3](https://www.rfc-editor.org/rfc/rfc6066.html#section-3)
prohibits more than one SNI name of the same type: a client cannot reliably
learn which name the server selected. Accepting the first hostname would
therefore make the proxy's authority decision depend on an interpretation that
another TLS implementation need not share.

The deterministic ClientHello builder can now emit multiple hostname entries.
One parser case pins Rustls's mature-parser rejection, and one real proxy case
proves the full consequence: an allowed first name plus a disallowed second
name closes the guest, records one denial, leaves final active ownership at
zero, and forwards exactly zero bytes to the already-connected upstream. The
focused end-to-end case passed ten consecutive runs.

Production behavior already held this invariant; the fixture and proofs are
test-only, so no data-path benchmark was run. Whole-tree SCC 4.0.0 complexity
moved from 510/1,535 to 513/1,545 structural/cognitive, entirely in test and
fixture code.

The native and exact Rust 1.88 Linux factories passed all 115 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 3,932 KiB RSS in
1,073 ms. The rootless runner reproduced 115/115 as UID/GID 65534; image
`sha256:dfcd57a000466a33516c7274e438f787fbcb6041f78ca8c32dff2d0549834c3a`
measured 40,414,382 bytes.

## 2026-09-01 — distinguish missing SNI from non-TLS tunnels

Visible-SNI mode previously had parser-level evidence for a valid ClientHello
without SNI, but no public-path proof for that input or for a tunnel that does
not begin with TLS. Both must fail closed, yet their operational causes should
remain distinguishable without logging attacker-controlled bytes.

One real two-connection case now sends a valid no-SNI ClientHello and a fixed
non-TLS byte string through the same lease. It requires the stable bounded
reasons `tls-sni-missing` and `client-hello-invalid`, respectively. Both
already-connected upstreams must observe zero bytes. Certified close then
requires exactly two accepted and denied connections, zero completed or active
connections, and exact attempted-upload accounting. The focused case passed
ten consecutive runs.

Production behavior already held the invariant, so no data-path benchmark was
run. Whole-tree SCC 4.0.0 complexity moved from 513/1,545 to 514/1,547
structural/cognitive; the loop-based conformance proof added only one
structural and two cognitive points.

The native and exact Rust 1.88 Linux factories passed all 116 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 4,012 KiB RSS in
1,041 ms. The rootless runner reproduced 116/116 as UID/GID 65534; image
`sha256:e2d9b5ef3769972bf8fba26903e87409eadf89bb7cc6a285d426f40f12e99fdb`
measured 40,421,194 bytes.

## 2026-09-01 — pin hostile CONNECT syntax and repair a racey proof

The request parser now has a fixed eight-shape matrix covering obsolete field
folding, NUL in names and values, another value control byte, whitespace before
a field name or colon, and non-ASCII CONNECT and Host authorities. All fail
closed. The test was written expecting UTF-8 request-target text to fail as
generic malformed syntax; the first run instead returned `invalid-authority`
because the mature HTTP parser accepted the target bytes and the stricter
authority parser rejected them. The exact stable reason was updated without a
production change.

A separate reader boundary proof accepts a complete `\r\n\r\n` whose final byte
lands exactly at the configured ceiling and rejects the same terminator shifted
one byte beyond it. This removes ambiguity between the inclusive valid bound
and the first invalid byte without adding another data-path branch.

The first stripped-runner execution then exposed an unrelated scheduling race
in the earlier unwind-time Lease Drop proof. Cancellation completed, the guest
stopped, and replacement attachment succeeded, but the test immediately
expected the old state's last `Arc` to be gone. A replacement may legally be
installed after the old phase becomes closed but before the queued
pointer-checked stale release is processed. The proof now waits independently
and boundedly for that last owner. It still requires both identity reuse and
eventual old-state destruction. The corrected focused case passed 25 native
runs, and the complete rootless suite passed three additional repetitions.

No production source changed and no data-path benchmark was run. Whole-tree
SCC 4.0.0 complexity moved from 514/1,547 to 519/1,562
structural/cognitive, entirely in deterministic conformance code.

The native and corrected exact Rust 1.88 Linux factories passed all 118
deterministic cases, doctests, documentation, and package verification; native
dependency policy checks also passed. Linux's 500-lease lane returned from
eight descriptors and five threads to four and two, finishing at 4,024 KiB RSS
in 1,058 ms. The rootless runner reproduced 118/118 as UID/GID 65534; image
`sha256:ab40985d6c486522a4eb80b1ef590e77b5bd599e8e1631a5ddf168d8402d97b5`
measured 40,425,450 bytes.

## 2026-09-01 — distinguish DNS deadline enforcement from resolver failure

DNS permit acquisition, resolver execution, and the absolute handshake were
already bounded, but an elapsed resolver deadline and an actual resolver I/O
error both produced `502 dns-failed`. The configured DNS deadline is a
first-class safety control, so collapsing those outcomes made diagnostics less
useful than the implementation's behavior.

The real-socket deadline proof was written first with a resolver future held
pending and a connector that counts every call. Its first run failed exactly
at the intended contract: the proxy returned `502 dns-failed` instead of the
expected `504 dns-timeout`. The resolver path now uses one explicit match:
permit starvation remains `503 dns-capacity`, a returned resolver error
remains `502 dns-failed`, and elapsed execution returns `504 dns-timeout`. A
second end-to-end proof pins the resolver-error neighbor. Both paths require
one denial and zero dial attempts, and each passed ten consecutive focused
runs.

The successful lookup and dial path is unchanged, so no throughput claim or
benchmark was made. Whole-tree SCC 4.0.0 structural complexity stayed at 519,
while cognitive complexity fell from 1,562 to 1,560 because the explicit match
replaced nested error mapping.

The native and exact Rust 1.88 Linux factories passed all 120 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 3,956 KiB RSS in
1,088 ms. The rootless runner reproduced 120/120 as UID/GID 65534; image
`sha256:51287dfe25d9098bccf2176a1000de9357258a65744fc618b18e374fc7b02985`
measured 40,428,701 bytes.

## 2026-09-01 — make shared DNS cache bounds an explicit host contract

The shared Hickory resolver cached responses, but Sandbox Egress inherited the
dependency's defaults without naming them in `ProxyConfig`: 8,192 responses
and an 86,400-second maximum TTL for both positive and negative entries. A
dependency update could therefore change process memory or freshness behavior
without changing this crate's API. In contrast, Smokescreen resolves each
outbound request and retains the selected address only on that request before
dialing it directly.

The first configuration proof failed to compile because no cache API existed.
`ProxyConfig::with_dns_cache` now lets the host narrow, but never widen, the
8,192-entry and 24-hour process ceilings. Zero entries disables storage. A
zero TTL is documented only as the narrowest validity window: Hickory treats
an entry as current at its exact expiry instant, so TTL zero is not advertised
as the cache-disable switch. Resolver construction explicitly applies the same
maximum TTL to positive and negative entries; a unit proof inspects Hickory's
effective options rather than merely checking the configuration fields.

Cache data remains proxy-owned rather than lease-owned, and cached answers do
not carry policy. A real-socket identity-reuse proof gives two sequential runs
the same hostname and loopback answer. The first policy explicitly grants
loopback and reaches the connector; the replacement policy omits that grant,
returns `resolved-address-denied`, and leaves the connector count unchanged.
That proof passed ten consecutive runs. Real-resolver expiry remains a separate
integration-test item.

Default behavior is unchanged, so no throughput claim or benchmark was made.
Whole-tree SCC 4.0.0 complexity moved from 519/1,560 to 520/1,562
structural/cognitive; the only new production branch is resolver-construction
error propagation.

The native and exact Rust 1.88 Linux factories passed all 123 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 4,044 KiB RSS in
1,064 ms. The rootless runner reproduced 123/123 as UID/GID 65534; image
`sha256:fc358fe7339c8ee16dc2031808b99cfbeb50dd42ce0ef535966744552a23b7e8`
measured 40,436,156 bytes.

## 2026-09-01 — exercise cache disable and expiry over local DNS

Inspecting resolver options proved configuration wiring, but not Hickory's
runtime behavior. A fixed local UDP DNS server now returns one loopback A
record with a 60-second wire TTL. It uses a bounded receive timeout, emits only
the two expected responses, and never contacts the public network.

The zero-capacity case performs two identical lookups and requires two UDP
queries. The expiry case keeps caching enabled, caps the otherwise 60-second
answer at one second, requires an immediate repeat to consume no second server
response, waits 1.2 seconds, and then requires a second upstream query. An
initial 20 ms ceiling was rejected after the first run: ordinary scheduling
overhead allowed the immediate repeat to outlive that window, so it consumed
the server's second response and the later lookup timed out. The wider interval
passed six native repetitions and the exact Linux factory.

The local fixture originally walked every DNS label even though Hickory emits
one fixed question. Replacing that loop with a direct terminator search reduced
the conformance addition from 526/1,584 to 524/1,577 whole-tree SCC 4.0.0
structural/cognitive complexity. Production behavior is unchanged apart from
extracting the already-tested cache-option assignment, so no throughput claim
or benchmark was made.

The native and exact Rust 1.88 Linux factories passed all 125 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 3,992 KiB RSS in
1,141 ms. The rootless runner reproduced 125/125 as UID/GID 65534; image
`sha256:60982d7e314041fdbfdc60990e822f70034501c40887f4c67758b48c318ec99e`
measured 40,456,922 bytes.

## 2026-09-01 — prove negative DNS cache expiry on the same wire seam

The local UDP responder now accepts a fixed response function. A new response
returns NXDOMAIN with a 60-second SOA negative TTL. Under the host-configured
one-second ceiling, the first lookup fails from the wire, the immediate repeat
fails from cache without consuming another response, and a lookup after 1.2
seconds must query the server again. The focused case passed six native runs.

Using the existing responder seam added no SCC 4.0.0 complexity: the whole tree
remained at 524/1,577 structural/cognitive. No production behavior or default
changed, so no throughput benchmark was run. Positive and negative TTL expiry,
zero-capacity behavior, effective option wiring, and cross-run policy rechecks
are now all deterministic cache conformance; the cache item was removed from
the hardening backlog.

The native and exact Rust 1.88 Linux factories passed all 126 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 4,036 KiB RSS in
1,070 ms. The rootless runner reproduced 126/126 as UID/GID 65534; image
`sha256:ee432090b710a2dba21e2115f1b451be0295d08778440c38c18b7bfeb0c09efa`
measured 40,465,442 bytes.

## 2026-09-01 — keep diagnostic loss outside the close boundary

The diagnostic reporter already used a bounded caller-owned channel,
`try_send`, static reason codes, a process-wide hard rate ceiling, and
saturating suppression accounting. Unit proofs showed those mechanisms in
isolation. They did not prove that contention on the public denial path could
not delay lease task completion and therefore certified close.

A new real-socket concurrency case holds a zero-capacity diagnostic channel
open without receiving. Sixty-four clients synchronize, send valid CONNECT
requests denied by hostname policy, and require the stable `403 host-denied`
response. Every request must terminate; `Lease::close` must then succeed within
its deadline with exactly 64 accepted, 64 denied, zero completed, and zero
active connections. The channel must remain empty. This passed ten consecutive
focused native runs and the exact Linux factory.

The first full factory rejected a direct `usize`-to-`u32` cast in the test under
the project's strict Clippy profile. The case now uses a checked conversion;
no lint exemption or production change was retained. Whole-tree SCC 4.0.0
complexity moved from 524/1,577 to 527/1,584 structural/cognitive, entirely in
the public conformance proof. No data path changed, so no throughput claim or
benchmark was made.

The native and exact Rust 1.88 Linux factories passed all 127 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 3,960 KiB RSS in
1,073 ms. The rootless runner reproduced 127/127 as UID/GID 65534; image
`sha256:363646ffdc786d3b2057edad46211b0f9a3fac68f0a1d767d2a91106bba71de6`
measured 40,486,738 bytes.

## 2026-09-01 — define multi-lease global saturation as fail-fast

The admission audit compared the shared listener with Lens and Nono. All three
bound connection work before spawning it. Sandbox Egress uses
`try_acquire_owned`, so global saturation creates no queued task or permit
waiter for `Lease::close` to discover and cancel. A fair waiting queue would
therefore add a new lease-owned lifecycle phase; Tokio semaphore fairness alone
would not make the complete proxy fair.

The retained contract is smaller: global admission is fail-fast, every refusal
is charged to the source identity's current lease, and a new socket can be
admitted after capacity is released. A dual-stack public test attaches separate
IPv4 and IPv6 leases to one listener and one global permit. IPv4 holds the
permit in the header phase; an IPv6 attempt must become terminal with zero
accepts and one denial on only the IPv6 lease. Certified IPv4 close releases
the permit, and the IPv6 retry must then become active before its own close
returns exactly one accept, one denial, and zero active connections. The case
passed ten focused native repetitions and both factories.

No production behavior changed, and no reserved-share option was added without
a demonstrated integration requirement. The hardening backlog now names that
as optional scheduling semantics rather than implying the current fail-fast
contract is accidentally fair. Whole-tree SCC 4.0.0 complexity moved from
527/1,584 to 533/1,596 structural/cognitive, all in conformance code; no data
path benchmark was claimed.

The native and exact Rust 1.88 Linux factories passed all 128 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 500-lease lane returned from eight
descriptors and five threads to four and two, finishing at 3,988 KiB RSS in
1,061 ms. The rootless runner reproduced 128/128 as UID/GID 65534; image
`sha256:1b29c3f1c3fc757112dc4e4591ee011ee5fbfe5d9aaefa96095d359f9db80b5d`
measured 40,491,607 bytes.

## 2026-09-01 — measure trusted management contention without weakening Drop

The command audit separated guest and host inputs. Network connections dispatch
directly from the listener and cannot enqueue commands. The unbounded channel
is reachable only through trusted `Proxy` and `Lease` handles. It also carries
nonblocking Drop cleanup: merely replacing it with a bounded `try_send` queue
could discard `Reap` when full and strand identity ownership. That rewrite was
therefore rejected without a durable overflow mechanism.

An opt-in resource lane now measures the actual open question instead. In each
of four batches, 64 host threads attach distinct identities together, hold all
leases while the process is sampled, then issue close together. It requires
descriptor and thread recovery after every batch and after proxy shutdown;
RSS is reported as allocator high-water evidence, not compared with a brittle
absolute threshold. The case passed its initial native run plus five repeated
two-batch runs.

The first exact Linux factory exposed a measurement bug: Cargo ran the serial
identity churn and concurrent control churn tests in parallel. Each test's
post-shutdown baseline therefore still contained the other test's proxy; the
control lane correctly rejected eight descriptors against a four-descriptor
process baseline. The runner now forces `--test-threads=1`. No resource claim
from that overlapping run was retained.

On the corrected Rust 1.88 Linux run, the control lane peaked at 69 threads and
5,216 KiB RSS with all 64 callers attached. Each batch returned to five runtime
threads and eight descriptors; shutdown returned to two threads and four
descriptors at 4,680 KiB RSS in 166 ms. The following 500-lease lane independently
returned to two threads and four descriptors at 4,680 KiB in 972 ms. On the M1
baseline, the four-batch control lane peaked at 10,464 KiB, returned to five
runtime threads and 13 descriptors after every batch, and shut down at two
threads and nine descriptors in 480 ms.

No production code or deterministic case count changed. Whole-tree SCC 4.0.0
complexity moved from 533/1,596 to 538/1,610 structural/cognitive, entirely in
the opt-in resource harness. The native and corrected Linux factories passed;
native dependency policy passed. The stripped conformance image excludes the
resource target and was therefore byte-identical at
`sha256:1b29c3f1c3fc757112dc4e4591ee011ee5fbfe5d9aaefa96095d359f9db80b5d`
(40,491,607 bytes); its rootless runner reproduced 128/128 as UID/GID 65534.

## 2026-09-01 — reject shared config ownership on the connection path

A fresh five-run sustained baseline measured 16,584–20,902 connections/second
with a 19,663 median. Each admitted task currently clones `ProxyConfig`; a
prospective three-line change instead stored it in `Arc` and cloned the pointer.
Five changed runs measured 16,277–22,260 connections/second with a 20,321
median, but the distributions overlapped and both contained large p99 host
scheduling outliers.

The first Criterion pair appeared positive: reverting from the shared pointer
to the value clone measured 4.0%–17.1% slower (`p < 0.05`). Reversing the order
did not reproduce it. With the clone build saved first, the shared build's
interval ranged from 6.6% faster to 20.2% slower (`p = 0.60`), explicitly no
detected change. The default configuration has no diagnostics and an empty
NAT64 vector, so cloning it does not allocate; replacing that copy with an
atomic strong-count update has no clear theoretical win either.

The source change was discarded. No correctness, resource, complexity,
factory, or throughput claim changed. The negative result is retained so a
future optimization pass does not infer a win from the first noisy comparison.

## 2026-09-01 — add a direct TCP control before another hot-path guess

Rather than attempt a second micro-optimization, the accepted CONNECT path was
bracketed with a direct loopback TCP control using the same upstream listener
and reset-on-drop behavior. Three paired Criterion runs also retained the
existing allowed CONNECT and hostname-denial cases.

The first two direct-control intervals were 34.74–38.30 and 35.48–37.95
microseconds. The first hostname-denial interval was 71.02–79.80 microseconds,
and allowed CONNECT was 108.99–127.77 microseconds. This coarse decomposition
is consistent with roughly one TCP handshake for the direct control, listener
scheduling plus bounded parse/policy/response for the denial, and a second TCP
handshake for the allowed upstream dial.

The third direct interval itself rose to 43.42–60.56 microseconds. Allowed and
denied measurements were noisy in the same sequence. The control therefore
demonstrated why subtracting a precise parser cost or claiming a small proxy
regression would be unsound on this host. No data-path code or configuration
changed. Whole-tree SCC 4.0.0 complexity moved from 538/1,610 to 539/1,613
structural/cognitive, entirely in the benchmark control.

The native and exact Rust 1.88 Linux factories passed all 128 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 64-caller control lane peaked at 5,284 KiB
RSS and returned to four descriptors and two threads in 176 ms. The following
500-lease lane returned to four and two at 4,736 KiB in 1,122 ms. The benchmark
is excluded from the stripped conformance runner, so the rootless 128/128 image
remained byte-identical at
`sha256:1b29c3f1c3fc757112dc4e4591ee011ee5fbfe5d9aaefa96095d359f9db80b5d`
(40,491,607 bytes).

## 2026-09-01 — pin authoritative address-registry drift, not generated policy

The default destination floor was rechecked against IANA's authoritative
[IPv4](https://www.iana.org/assignments/iana-ipv4-special-registry) and
[IPv6](https://www.iana.org/assignments/iana-ipv6-special-registry)
special-purpose registries. Both still report 2025-10-09 as their last update.
No new entry or changed reachability flag creates a gap in the current IPv4
unsafe-prefix table, IPv6 `2000::/3` global-unicast floor and unsafe children,
or translated-IPv4 handling. No policy change was made.

The audit previously depended on prose and a review date. A new opt-in command
downloads the two official CSVs to a temporary directory and compares their
SHA-256 values with the reviewed versions: IPv4
`e3e39e76d00b1677335db8e9a805c7b9480ea2f4dc9e33f0b93cd3a905128d73`
and IPv6
`775feea0621dec8735a44fbf30f762e721e8f0a1b3ab7eb341961a88cfce2139`.
The live command passed for both. It supports `sha256sum` and macOS `shasum`,
uses a `mktemp` directory with cleanup, and prints the authoritative URL when a
pin changes.

This is intentionally a drift alarm, not a generator. Registry semantics still
require review, and automatically allowing a newly global special-purpose
range would weaken a conservative SSRF floor. The command is outside every
test and factory lane, so normal development remains deterministic and offline.
No Rust source, deterministic case count, benchmark, or complexity changed.

The native and exact Rust 1.88 Linux factories passed all 128 deterministic
cases, doctests, documentation, and package verification; native dependency
policy checks also passed. Linux's 64-caller control lane peaked at 5,252 KiB
RSS and returned to four descriptors and two threads at 4,708 KiB in 180 ms.
The following 500-lease lane returned to four and two at 4,716 KiB in 1,136 ms.
Because the change only adds documentation and a maintainer script, the
rootless 128/128 conformance image remained byte-identical at
`sha256:1b29c3f1c3fc757112dc4e4591ee011ee5fbfe5d9aaefa96095d359f9db80b5d`
(40,491,607 bytes).

## 2026-09-01 — pin trusted recursive DNS configuration and TCP recovery

The production resolver previously always snapshotted the host operating
system configuration. That is a reasonable default but leaves a Firecracker
supervisor unable to bind proxy resolution to its own controlled recursive
service. `ProxyConfig::with_dns_server` now adds trusted process-wide socket
addresses before startup. At least one explicit server bypasses both host
resolver configuration and the hosts file; every configured port is used for
UDP and TCP. The guest and individual leases have no resolver-selection input.

The list deduplicates exact socket addresses and startup rejects more than
eight, port zero, or a scoped IPv6 address. The scope rejection is deliberate:
Hickory's name-server configuration stores an `IpAddr` and cannot faithfully
carry `SocketAddrV6::scope_id`. Silently discarding it could route a link-local
server differently from the operator's request. The default system-configured
mode remains unchanged except that UDP errors are now allowed to retry over
TCP.

Tests were written red first for the public configuration and resolver route.
A controlled recursive server on a nonstandard loopback port proves explicit
mode resolves without a host configuration or hosts-file path. A second local
server returns a valid truncated UDP response, accepts Hickory's length-prefixed
TCP retry on the same port, and returns the complete A answer. That transport
case passed ten consecutive focused runs and has a two-second bounded accept
failure rather than hanging if fallback regresses. Validation cases pin
deduplication and every new startup bound.

Whole-tree SCC 4.0.0 complexity moved from 539/1,613 to 554/1,667
structural/cognitive. The production cost is one bounded configuration vector,
validation, and one resolver-construction branch; most of the increase is the
dual-protocol test server. The native and exact Rust 1.88 Linux factories
passed all 133 deterministic cases, three README compile examples,
documentation, and package verification; native dependency policy checks also
passed. Linux's 64-caller control lane peaked at 5,268 KiB RSS and returned to
four descriptors and two threads at 4,732 KiB in 196 ms. The following
500-lease lane returned to four and two at 4,740 KiB in 1,087 ms. The rootless
133/133 conformance image is
`sha256:c9579b68fd0652097bcc5c2dcb6bd43d4e4bd5c0497e3eceb97a24595f4e9f9f`
(40,524,986 bytes).

## 2026-09-01 — prove real resolver cancellation at the wire boundary

The controlled `TestResolver` seam already proved that dropping a lookup future
prevents a late answer from reaching the connector. Hickory documents that its
resolver drives network exchanges in background tasks, however, so that seam
did not prove the dependency stops retry traffic after our future is cancelled.
This matters to the strongest interpretation of certified close: a run that no
longer owns DNS work must not cause a new query after revocation.

A new real-proxy case uses the explicit local recursive server and waits for
Hickory's parallel A and AAAA UDP queries. The lease is successfully closed
before the server releases valid SERVFAIL responses for both request IDs. For
400 ms after release, the server polls for both new UDP packets and new TCP
connections and requires zero. The guest socket must also terminate and final
usage must report one accepted and zero active connections. The result pins the
relevant Hickory behavior: dropping the lookup closes its completion channel,
the background exchange removes the active request, and a late failure cannot
enter retry or dialing logic.

The observation watches TCP as well as UDP because the preceding cycle enabled
transport fallback. It passed ten consecutive focused runs. Whole-tree SCC
4.0.0 complexity moved from 554/1,667 to 564/1,716 structural/cognitive,
entirely in the wire server and conformance case; production code is unchanged.
The native and exact Rust 1.88 Linux factories passed all 134 deterministic
cases, three README compile examples, documentation, and package verification;
native dependency policy checks also passed. Linux's 64-caller control lane
peaked at 5,284 KiB RSS and returned to four descriptors and two threads at
4,752 KiB in 168 ms. The following 500-lease lane returned to four and two at
4,748 KiB in 1,094 ms. The rootless 134/134 conformance image is
`sha256:7d595c29f08dae162dfd2a17654d7f03d5cf89d4a064821b4748886f8bc2e8d7`
(40,531,681 bytes).

## 2026-09-01 — add the missing usage observation barrier

The first exact Rust 1.88 Linux run of the resolver-module simplification
exposed a race in `global_capacity_rejection_is_attributed_and_retry_recovers`.
The rejected IPv6 socket was terminal, but the immediate point-in-time usage
snapshot still observed zero denials. Socket destruction and the atomic denial
increment are both ordered under the lease phase lock before certified final
usage, but the public live snapshot does not promise which becomes observable
first to two different threads.

The conformance case now waits, with a one-second deadline, for the expected
atomic denial count just as it already waits for active admission. Production
ordering was left unchanged; strengthening it would add a contract that lease
correctness does not need. The focused case then passed 50 consecutive native
runs. The corrected exact Linux factory passed all 134 deterministic cases,
resource lanes, documentation, and package verification, and the rootless
134/134 conformance run passed in the combined candidate image. Deterministic
case count and production complexity are unchanged.

## 2026-09-01 — isolate resolver machinery without adding abstraction

The explicit-server work left resolver construction, transport/cache options,
backend dispatch, and bounded answer collection inside the already large
lifecycle module. Those responsibilities were moved verbatim into one private
`resolver` module. `proxy` still owns listener admission, per-lease deadlines,
cancellation, address policy, dialing, and certified close. The test-only
resolver trait moved with its production counterpart; it remains crate-private
and cannot become a guest-selected backend.

This is deliberately a module extraction, not a new public type or trait layer.
SCC 4.0.0 remains exactly 564/1,716 structural/cognitive. `proxy.rs` fell from
3,538 to 3,467 lines and from 783 to 773 cognitive points; `resolver.rs` is 91
lines and 10 cognitive points. The whole tree gained 22 source lines for module
documentation, imports, and the module edge. The measured benefit is a smaller
lifecycle ownership surface with identical total machinery, which is useful to
future agents working on resolver behavior without competing in the core
lifecycle file.

The native and corrected exact Rust 1.88 Linux factories passed all 134
deterministic cases, three README compile examples, documentation, and package
verification; native dependency policy checks also passed. Linux's 64-caller
control lane peaked at 5,304 KiB RSS and returned to four descriptors and two
threads at 4,772 KiB in 174 ms. The following 500-lease lane returned to four
and two at 4,764 KiB in 1,104 ms. The rootless 134/134 conformance image is
`sha256:88e887c52519019718f7febf43e5a50d8ed84672257d99880833661b1b5053ad`
(40,529,915 bytes), 1,766 bytes smaller than the pre-extraction image.

## 2026-09-01 — pin both sides of CONNECT success under transport failure

The tunnel suite already proved an upstream reset after five uploaded bytes,
but two backlog edges were still implicit. A new pre-establishment case binds
and releases a local destination, then requires the proxy to return the stable
502 `dial-failed` denial without ever emitting `200 Connection Established`.
Final usage is exactly one accepted, one denied, zero completed, zero active,
and zero bytes. A refused destination therefore remains a bounded policy
outcome rather than masquerading as an established tunnel.

The symmetric post-establishment case uses a continuously writing local
upstream. It waits until the proxy's download counter advances, arms an
immediate guest reset, and requires the upstream writer to observe a terminal
socket error. Certified close then preserves nonzero bytes already read while
reporting neither completion nor policy denial. This pins the documented
accounting boundary: counters measure bytes read by the proxy, including bytes
whose following write reaches a broken pipe; they do not claim application
delivery.

Both cases passed 25 consecutive focused runs. Production code is unchanged.
Whole-tree SCC 4.0.0 complexity moved from 564/1,716 to 568/1,724
structural/cognitive, entirely in conformance code. The native and exact Rust
1.88 Linux factories passed all 136 deterministic cases, three README compile
examples, documentation, and package verification; native dependency policy
checks also passed. Linux's 64-caller control lane peaked at 5,324 KiB RSS and
returned to four descriptors and two threads at 4,796 KiB in 171 ms. The
following 500-lease lane returned to four and two at 4,816 KiB in 1,066 ms.
The rootless 136/136 conformance image is
`sha256:e815594c685173018879d705817342fef924bf255ede92a241c19548215f6dcf`
(40,535,243 bytes).

## 2026-09-01 — make post-establishment byte ceilings exact

The metered tunnel reader previously read whatever fit in Tokio's copy buffer,
then rejected the whole read if its cumulative total crossed the policy limit.
That was fail-closed but made useful allowance depend on kernel coalescing. A
red end-to-end case gave an eight-byte upstream write a seven-byte download
budget and received no payload instead of the permitted seven-byte prefix.

While budget remains, the retained reader now caps only that socket read to the
remaining allowance. Tokio forwards the permitted bytes normally. A following
nonempty read is still counted, crosses the ceiling, and produces the existing
single `transfer-limit` denial without forwarding excess. The unlimited path
does not resize or initialize the copy buffer, and the implementation adds no
allocation or staging buffer. Paired upload and download cases each write
`allowed!` in one call, require exactly `allowed` at the peer, and require eight
accounted bytes, zero completions, and one denial. They passed 25 consecutive
focused runs. Over-limit bytes already coalesced with CONNECT deliberately
retain their stronger pre-DNS whole-request denial.

A detached worktree at the preceding commit provided a same-host comparison of
the unlimited path. Across three warm 8-by-32 MiB runs, baseline median upload
and download were 3,201 and 3,368 MiB/s; candidate medians were 3,281 and 3,432
MiB/s. The short local distributions overlap, so this is recorded only as no
measurable regression, not a speedup. The temporary worktree and its 333.8 MiB
build tree were removed after measurement.

Whole-tree SCC 4.0.0 complexity moved from 568/1,724 to 573/1,742
structural/cognitive. Production `proxy.rs` accounts for 4/16 points; the
remaining 1/2 is conformance code. The native and exact Rust 1.88 Linux
factories passed all 138 deterministic cases, three README compile examples,
documentation, and package verification; native dependency policy checks also
passed. Linux's 64-caller control lane peaked at 5,340 KiB RSS and returned to
four descriptors and two threads at 4,800 KiB in 182 ms. The following
500-lease lane returned to four and two at 4,820 KiB in 1,082 ms. The rootless
138/138 conformance image is
`sha256:911cf1e1b00046aeed79dad19d252c52863ada036992da6a0b201b8494ab6e97`
(40,555,530 bytes).

## 2026-09-01 — remove solved work from the hardening backlog

The hardening inventory had accumulated items that were already pinned by
named conformance cases and documented invariants. Examples included every
revocation phase, close reply races, global and per-lease pre-spawn admission,
mixed allowed/forbidden DNS sets, checked-address-only dialing, transition
address forms, the CONNECT authority matrix, TLS fragmentation and GREASE,
counter saturation, and the Docker/resource/complexity factory itself. Leaving
them as open bullets would direct future contributors toward duplicating work.

The backlog now states its lifecycle rule and links to the testing record and
this log for completed evidence. Thirty-one solved or purely methodological
bullets were removed or consolidated. The remaining list emphasizes unresolved
host-kernel identity limits, broader parser and resolver behavior, evolving TLS
compatibility, terminal-path resources under active tunnels, admission
fairness, control-plane saturation, and deployment integration. No security
claim, public API, source, test, dependency, or factory behavior changed.

## 2026-09-01 — soak real connection terminal paths

The resource factory previously measured empty identity churn and concurrent
host management, but neither lane repeatedly owned guest and upstream sockets.
A third opt-in lane now keeps one proxy, lease, and local echo listener alive.
Each iteration completes a one-byte CONNECT tunnel with graceful half-close,
then sends a hostname denial that must stop before DNS. Every batch waits for
the lease's active count to return to zero before sampling resources; certified
close must report exactly one completion and one denial per iteration.

At the standard small-factory setting, the macOS lane ran 500 completed tunnels
plus 500 denials in 282 ms. It held the active baseline's descriptor envelope
at 13–14 during batches and returned to 9 descriptors and 2 threads after
shutdown. The exact Rust 1.88 Linux lane ran the same 1,000 connections in 253
ms: it began active at 9 descriptors and 6 threads, returned to 8 and 5 after
the second batch, and finished at 4 and 2. RSS finished at 4,944 KiB. This scope
is explicit: repeated reset, timeout, transfer-limit, long-lived, and
backpressured resource lanes remain open rather than being implied by these two
terminal paths.

Whole-tree SCC 4.0.0 complexity moved from 573/1,742 to 580/1,758
structural/cognitive, entirely in the opt-in measurement target. Production
code and deterministic case count are unchanged. The native and exact Linux
factories, package verification, and rootless 138/138 conformance run passed.
Because the resource executable is not shipped in the stripped runner, the
image remains
`sha256:911cf1e1b00046aeed79dad19d252c52863ada036992da6a0b201b8494ab6e97`
(40,555,530 bytes).

## 2026-09-01 — extend socket soak through reset and transfer denial

The active-socket lane now exercises four terminal classifications per
iteration under one immutable one-byte upload policy. A one-byte echo closes
gracefully; a two-byte upload forwards exactly one byte and hits the transfer
ceiling; a local upstream uses a channel barrier to wait until the guest has
received CONNECT success before resetting; and a hostname denial stops before
DNS. The barriers avoid timing sleeps and ensure the reset cannot be confused
with pre-establishment refusal.

At 500 iterations the lane owns 2,000 guest connections and requires exact
final counters: 2,000 accepted, 500 completed, 1,000 denied, 1,500 uploaded
bytes, 500 downloaded bytes, and zero active. The 500 resets must be neither
completed nor denied. On macOS this took 425 ms, held batches at 13–14
descriptors and 5–6 threads, and returned to 9 descriptors and 2 threads. The
exact Rust 1.88 Linux lane took 433 ms, held 8–9 descriptors and 5–6 threads,
and returned to 4 and 2 at 5,136 KiB RSS.

Whole-tree SCC 4.0.0 complexity moved from 580/1,758 to 582/1,764
structural/cognitive, entirely in the opt-in resource target. Production code,
deterministic case count, and stripped image are unchanged. The native and
exact Linux factories, package verification, and rootless 138/138 conformance
run passed; the image remains
`sha256:911cf1e1b00046aeed79dad19d252c52863ada036992da6a0b201b8494ab6e97`
(40,555,530 bytes).

## 2026-09-01 — reject forbidden addresses after real DNS aliases

The resolver boundary now has a real DNS-wire proof for an allowed hostname
whose CNAME terminates at the link-local metadata address. A first version of
the fixture accidentally returned the original alias again when Hickory queried
the CNAME target; that correctly ended at the bounded DNS deadline with no dial,
but did not prove the intended address-floor behavior. The corrected fixture
distinguishes question name and type. It observes A and AAAA questions for both
`alias.test.` and `metadata.test.`, returns `169.254.169.254` only for the
target's A question, and gives the target's AAAA question an empty successful
answer.

The original CONNECT hostname remains the immutable policy authority; the
terminal address is independently checked against the special-purpose floor.
The request is denied as `resolved-address-denied`, the test connector records
zero attempts, and certified close reports exactly one denial. The focused case
passed 25 consecutive runs. This establishes the one-hop forbidden-address
case without claiming coverage of longer chains, loops, or malformed replies.

Production code is unchanged. Whole-tree SCC 4.0.0 complexity moved from
582/1,764 to 590/1,790 structural/cognitive, entirely in deterministic test
code. The native and exact Rust 1.88 Linux factories passed 139 deterministic
cases, documentation, and package verification. Linux's 64-caller control lane
peaked at 5,332 KiB RSS and returned to four descriptors and two threads at
4,844 KiB in 175 ms. The 500-lease lane returned to four and two at 4,860 KiB
in 1,060 ms. The 2,000-connection terminal lane returned to four and two at
5,160 KiB in 436 ms. The rootless 139/139 conformance image is
`sha256:ed2e8ed16d0d4667e8084c8c8bbd777a3ad74ce3762516142c332ed45fc5fd70`
(40,557,141 bytes).

## 2026-09-01 — bound incomplete DNS wire replies

A local UDP authority now returns only the two-byte transaction identifier for
every A and AAAA question, omitting even the DNS header. The production Hickory
path makes exactly six questions—three attempts across both address
families—then reports a protocol failure. The proxy returns `502 dns-failed`
within the one-second test bound, records no connector attempts, and certified
close reports exactly one denial. The focused case passed 25 consecutive runs.
Broader malformed response matrices remain open; this commits one minimal
failure shape and its retry amplification rather than generalizing from it.

The first assertion expected the lease's 200 ms DNS deadline to win and exposed
the actual immediate `dns-failed` classification. The first server helper then
accepted any positive query count. Measuring six stable questions allowed that
loop and stop channel to be replaced by the existing exact-count UDP fixture.
The final proof therefore adds no measured structural or cognitive complexity:
the whole tree remains at 590/1,790, and production code is unchanged.

The native and exact Rust 1.88 Linux factories passed 140 deterministic cases,
documentation, and package verification. Linux's 64-caller control lane peaked
at 5,320 KiB RSS and returned to four descriptors and two threads at 4,860 KiB
in 177 ms. The 500-lease lane returned to four and two at 4,868 KiB in 1,111
ms. The 2,000-connection terminal lane returned to four and two at 5,180 KiB
in 433 ms. The rootless 140/140 conformance image is
`sha256:44d85c05ace8552dd073130eada044287415097f5dc5dd293b647d14e60c21df`
(40,558,939 bytes).

## 2026-09-01 — bound CNAME cycles and isolate wire conformance

A real UDP authority now alternates valid CNAME answers between `loop.test.`
and `target.test.`. Supplying only four replies left the production resolver
active until the lease deadline, yielding the expected no-dial `dns-timeout`
but not the desired chain-bound proof. Inspection of the pinned Hickory 0.26.1
source identified its maintained eight-hop CNAME depth. Supplying the complete
A and AAAA paths produces exactly 16 questions, then `502 dns-failed`, zero
connector attempts, and one certified denial. The focused case passed 25
consecutive runs.

The alias-to-metadata, incomplete-header, and CNAME-cycle fixtures moved into
`src/proxy/tests/dns_wire.rs`. Their shared listener and packet framing remain
in the parent test module; production visibility and behavior are unchanged.
This removes 123 lines from `proxy.rs` and moves its measured
structural/cognitive complexity from 238/815 to 230/789. With the new cycle
proof included, whole-tree complexity moves from 590/1,790 to 593/1,788. The
small cognitive decrease is a file-boundary property of SCC's estimate, not a
runtime optimization claim.

The native and exact Rust 1.88 Linux factories passed 141 deterministic cases,
documentation, and package verification. Linux's 64-caller control lane peaked
at 5,344 KiB RSS and returned to four descriptors and two threads at 4,856 KiB
in 177 ms. The 500-lease lane returned to four and two at 4,868 KiB in 1,088
ms. The 2,000-connection terminal lane returned to four and two at 5,132 KiB
in 453 ms. The rootless 141/141 conformance image is
`sha256:8be96646ef39241618e47f579291fe4444d635712b5e11b62f4e5dea63c2df15`
(40,561,917 bytes).

## 2026-09-01 — bound process-wide outbound dialing

Outbound connection establishment now has a process-wide budget independent
of total admitted connections and DNS work. `ProxyConfig` defaults to 256
concurrent dials and exposes `with_max_concurrent_dials`; extreme values are
clamped before Tokio constructs the semaphore. A connection acquires its permit
only after the complete resolved-address set passes policy. Waiting consumes
the existing absolute handshake deadline and expires as the distinct
`503 dial-capacity` denial. The permit is dropped immediately after connection
establishment, before CONNECT success, TLS inspection, or tunnel lifetime.

Three proofs pin the ownership boundary. Five pending connections with a limit
of two produce exactly two connector calls; certified close cancels those two
live attempts and the three queued permit waits, stops every client, and leaves
zero active work. A separate dual-stack, two-identity case lets one lease hold
the only permit while the other exhausts its handshake deadline; the waiting
lease receives one attributed capacity denial and never enters the connector.
Finally, a public system-dial test establishes and simultaneously holds two real
loopback tunnels with a limit of one, proving the permit is not retained by an
established tunnel. Both internal cases passed 25 consecutive runs and the
public case passed 10 consecutive runs.

The first implementation used an owned semaphore permit. A borrowed permit is
sufficient because the dial helper's stack frame encloses every attempt, so the
owned form and its `Arc` traffic were removed. An explicit uncontended branch
was also tried, measured, and discarded: it added one structural and six
cognitive complexity points without a stable latency benefit. The retained
single deadline-wrapped acquisition is the smaller implementation.

A detached `9ec6256` worktree supplied the before measurement and was removed
with its build artifacts afterward. Short paired end-to-end runs crossed in
both directions. Two longer unnormalized runs suggested a 4.6–7.6 microsecond
loopback setup cost, but the direct-TCP control moved at the same scale. The
final control-normalized proxy medians were 71.54 and 72.91 microseconds versus
80.71 and 72.22 before the change, with overlapping intervals. No performance
change is claimed. The budget is paid once per outbound setup and does not add
per-byte or tunnel-lifetime work.

Whole-tree SCC 4.0.0 complexity moves from 593/1,788 to 602/1,809
structural/cognitive. `proxy.rs` moves from 230/789 to 233/797; the remaining
increase is the public configuration and three deterministic proofs. The
native and exact Rust 1.88 Linux factories passed 144 deterministic cases,
documentation, package verification, and dependency policy checks. Linux's
64-caller control lane peaked at 5,332 KiB RSS and returned to four descriptors
and two threads at 4,852 KiB in 180 ms. The 500-lease lane returned to four and
two at 4,856 KiB in 1,098 ms. The 2,000-connection terminal lane returned to
four and two at 5,108 KiB in 448 ms. The rootless 144/144 conformance image is
`sha256:92ace025f196b32512a25d581f114b2ece9dca0b9c1af3fd803950d726a21d7e`
(40,615,724 bytes).

## 2026-09-01 — close the absolute-deadline response gap

An admission and shutdown audit found one handshake operation outside every
deadline: after a checked upstream dial, the proxy wrote the 39-byte CONNECT
success response with an unbounded `write_all`. A normal TCP send buffer makes
that write usually immediate, but "usually" is weaker than the crate's
absolute accept-to-handshake contract. Smokescreen's pinned implementation also
reinforces the operational need by placing response writes under a configured
write timeout, although its timeout model and lifecycle boundary differ.

The audit also exposed a narrower runtime semantic. Tokio's timeout polls its
inner future before its timer, so work that is immediately ready may be polled
even when the supplied deadline was already elapsed. The retained
`complete_before_deadline` helper first rejects an already-expired instant,
then delegates in-flight timing to Tokio. Headers, DNS capacity and lookup,
dial capacity and attempts, initial upload, ClientHello work, lease close, and
proxy shutdown now use that one rule. The CONNECT success writer joins them;
deadline expiry records the static `connect-response-timeout` denial and closes
the socket rather than entering tunnel work.

Two small proofs pin the boundary. An immediately ready future behind an
expired deadline is never polled. The production response helper is then given
a one-byte-capacity stream and a 20 ms deadline; it can expose only a strict
prefix of the 39-byte response before returning. Both cases passed 25
consecutive runs, and the existing deadline matrix continued to pass across
headers, DNS, dial, initial upload, TLS inspection, close, and shutdown.

A first implementation used a deadline-first `select` at every timeout site.
It made the strict tie-breaking rule easy to state, but three
control-normalized local CONNECT pairs were 1.5–13.7 microseconds slower. That
variant was removed. The retained elapsed check plus maintained timeout was
compared with `7f4195d` in five alternating three-second runs. Excluding one
visible host outlier, previous proxy-minus-control medians were 77.10–84.39
microseconds and retained medians were 79.12–82.71 microseconds; reversed-order
differences crossed zero. No performance change is claimed. The detached
baseline worktree and its artifacts were removed after measurement.

Whole-tree SCC 4.0.0 complexity moves from 602/1,809 to 605/1,816
structural/cognitive. `proxy.rs` moves from 233/797 to 236/804; the two proof
functions add no measured complexity. The native and exact Rust 1.88 Linux
factories passed 146 deterministic cases, documentation, and package
verification; the native factory also passed dependency policy checks. Linux's
64-caller control lane peaked at 5,268 KiB RSS and returned to four descriptors
and two threads at 4,800 KiB in 181 ms. The 500-lease lane returned to four and
two at 4,808 KiB in 1,079 ms. The 2,000-connection terminal lane returned to
four and two at 5,244 KiB in 438 ms. The rootless 146/146 conformance image is
`sha256:a030d1124f767858c5f9b8d1187750c164a5f1a9b0d23558719b0512977c6b6c`
(40,663,581 bytes).

## 2026-09-01 — expire silent tunnels by immutable policy

The founding requirements call for absolute handshake deadlines, not merely
idle socket timeouts. Reviewing that distinction against Smokescreen exposed
the other half of the operational boundary: Sandbox Egress bounded every
pre-tunnel phase and byte count, but a successfully established tunnel could
remain silent while retaining global and per-lease connection capacity for the
entire run. `PolicyBuilder::idle_timeout` now supplies an optional nonzero
duration frozen into one lease. It is disabled by default so the crate does not
silently break applications that legitimately hold quiet connections.

The clock begins only after CONNECT success and optional ClientHello
inspection. The two metered readers share one Tokio watch value; every
successful nonempty read in either direction replaces its timestamp. One
future sleeps to the observed deadline and rechecks the channel generation,
without spawning another task. Using the generation rather than timestamp
equality ensures two reads sharing one platform clock tick still count as
activity. Expiry records the static `tunnel-idle-timeout` denial and drops the
bidirectional copy and both owned sockets. A biased copy branch prevents a
simultaneously ready graceful completion from being mislabeled. Lease
cancellation drops the entire tunnel future first, so certified close still
preempts the idle waiter and remains the stronger guarantee.

The first activity case used upload followed by an immediate echo. Review
caught that this proved upload activity but not a download-only flow, so a
separate one-way sender was added before the claim was documented. Silent
expiry, upload-and-echo activity, download-only activity, and close preemption
each passed 25 consecutive focused runs. The cases require exact terminal
behavior on both endpoints and exact counters: one idle denial, no completion,
and only the bytes actually read. Zero and unrepresentable durations fail at
policy construction.

A detached `2f1e883` worktree supplied five alternating data-plane comparisons
and was removed afterward. Each direction moved 1 GiB through eight established
loopback tunnels. The previous default medians were 3,369 MiB/s upload and
3,446 MiB/s download; the current disabled-default medians were 3,335 and
3,464 MiB/s (-1.0% and +0.5%). That crossing difference is within host noise,
so no default-path throughput change is claimed. With a 1,000 ms idle timeout,
continuous traffic measured 3,249 and 3,406 MiB/s, 2.6% and 1.7% below the
current default. The opt-in timestamp update cost is retained; the default
allocates no activity channel.

Whole-tree SCC 4.0.0 complexity moves from 605/1,816 to 620/1,860
structural/cognitive. `proxy.rs` moves from 236/804 to 241/820; the remaining
increase is immutable policy validation, three deterministic tunnel proofs,
and the opt-in measurement switch. The native and exact Rust 1.88 Linux
factories passed 150 deterministic cases, documentation, and package
verification; the native factory also passed dependency policy checks. Linux's
64-caller control lane peaked at 5,264 KiB RSS and returned to four descriptors
and two threads at 4,792 KiB in 191 ms. The 500-lease lane returned to four and
two at 4,824 KiB in 1,109 ms. The 2,000-connection terminal lane returned to
four and two at 5,132 KiB in 450 ms. The rootless 150/150 conformance image is
`sha256:9fdd4d14741dce47c20c0f047f40432a66ea43a342ca9c3f498e34a56d0f59c4`
(40,701,820 bytes).

## 2026-09-01 — measure concurrent near-limit TLS parser ownership

The TLS suite pinned the maximum byte boundary and cancellation of one partial
hello, but it did not show process behavior when many connections hold parser
state simultaneously. A new opt-in lane establishes 64 inspected loopback
tunnels by default. Each sends 60,020 bytes of legal TLS records describing an
incomplete 65,535-byte ClientHello, then remains open. The test does not sample
until aggregate lease upload accounting equals every input byte and the active
gauge equals the connection count. Those are deterministic barriers that every
parser buffer is live, unlike a setup sleep or accepted-socket count.

Certified lease close must then return exact final accounting: all connections
accepted and inactive, none completed or denied, all 3,841,280 uploaded bytes,
and zero downloaded bytes. Every guest and upstream socket must be terminal.
Descriptors and threads must recover with the proxy alive and after shutdown;
RSS is recorded rather than thresholded because the allocator can retain freed
pages. The runner takes the TLS connection count as its fourth argument.

A native 1, 32, 64, 128, and 256-connection sweep measured peak RSS of 9,536,
14,304, 19,056, 28,512, and 47,472 KiB. The one-to-256 slope is about 149 KiB
per connection for 60 KiB of wire input, including the retained wire image,
Rustls state, task/socket state, and allocator effects. Peak descriptors were
exactly 18, 142, 270, 526, and 1,038 while threads remained six. Ten repeated
64-connection release processes held peak RSS to 19,008–19,088 KiB and always
returned from 270 descriptors/six threads to 13/five after close and nine/two
after shutdown. No speculative parser change was retained: preserving the
exact approved wire image and using a maintained incremental parser account
for distinct bounded state, while existing connection and byte ceilings bound
their product.

The first Rust 1.88 Linux factory run exposed a test-bound issue adjacent to
the prior constrained-forwarding proof. Its real 250 ms handshake deadline
closed the guest correctly, but the one-second channel wait could expire while
the deliberately saturated upstream queue drained. The target now has its own
five-second read bound and the parent uses the same post-deadline observation
bound; the security deadline is unchanged. The corrected proof passed 25
native repetitions and the repeated Linux factory.

The full default-size native resource run also exposed an existing measurement
limit after tens of thousands of connections to the same loopback tuple: the
terminal lane eventually received `502 connect-failed` as local ephemeral
ports accumulated. This is recorded as a harness reproducibility issue, not a
proxy-capacity result; the bounded Linux resource smoke passed. A following
cycle will separate the long identity-churn count from the smaller terminal
socket count so the default factory does not measure host `TIME_WAIT` capacity
by accident.

Whole-tree SCC 4.0.0 complexity moves from 642/1,934 to 648/1,946
structural/cognitive, entirely in test and measurement code. Production code
and the 155 deterministic cases are unchanged. The native and exact Rust 1.88
Linux factories passed lint, documentation, package verification, five
resource lanes, and five doctests; native dependency policy checks also
passed. Linux TLS state peaked at 14,900 KiB RSS, 265 descriptors, and six
threads, recovered after close to 10,004 KiB/eight/five, and returned after
shutdown to 6,108 KiB/four/two. The rootless 155/155 image is
`sha256:2844e92a2ba01365f126b10301e4cf823c2f4963e31987c96526767024a20bad`
(40,717,070 bytes).

## 2026-09-01 — separate socket soak from host port exhaustion

The new TLS lane made the full resource runner retain more test history in one
process, which helped expose two factory assumptions rather than production
defects. First, the script passed its 2,000-by-four management-churn dimensions
to the terminal lane too. Each terminal iteration opens four guest sockets and
three upstream sockets to the same loopback tuples. A native run completed two
batches and then received `502 connect-failed` during the third as local
ephemeral ports accumulated. That was measuring the host's same-tuple
`TIME_WAIT` capacity, not descriptor, thread, or owned-task recovery.

The terminal lane now has independent dimensions: 500 iterations by four
batches by default, exposed as the fifth and sixth resource-runner arguments.
The ordinary default still attaches and closes 8,000 management leases, while
the terminal lane exercises 2,000 each of completion, upload-limit denial,
upstream reset, and pre-DNS denial—8,000 guest and 6,000 upstream connections.
The corrected native default completed in 16.19 seconds. Its terminal lane
returned from 14 descriptors/six threads during work to 13/five with the proxy
alive and nine/two after shutdown. Larger terminal dimensions remain explicit
when host-kernel port behavior is the intended experiment.

Second, merely widening the constrained TLS proof's post-deadline observation
from one to five seconds failed on a repeated Linux factory. The target applied
its 1 KiB receive-buffer request only after `accept`, racing the connector's
send-queue saturation. Some runs could therefore stage megabytes in the
default receive window and spend the observation bound draining test setup.
The listener now receives the bound before any connection exists, so accepted
sockets inherit it. Across 25 native repetitions the connector then prefills
exactly 1,024 bytes and the 250 ms deadline permits only 48,128 or 49,152 of the
roughly 64 KiB ClientHello before cancellation. The five-second read remains
an independent harness failure bound; it is no longer expected to compensate
for an unbounded setup race.

Production code, deterministic case count, and whole-tree 648/1,946 SCC 4.0.0
complexity are unchanged. The native factory, corrected default five-lane
resource run, and exact Rust 1.88 Linux factory passed. Linux terminal churn
now covers 2,000 instances of every path and returns to four descriptors and
two threads; the TLS lane returned from 265/six to four/two. The rootless
155/155 image is
`sha256:c669bc5cb96beb81c33ae8e4b93424aa574c826354135cbdf81b7108bcae98c2`
(40,717,343 bytes).

## 2026-09-01 — soak certified close after bidirectional saturation

One deterministic tunnel case proved that close terminates hostile writers in
both directions, but a one-shot proof cannot expose accumulation across close,
task teardown, and source-identity reuse. The resource target now repeats that
state 64 times in four batches under one proxy. Each cycle attaches the same
source identity, opens one real loopback tunnel, and starts one guest and one
upstream writer while neither application reads.

The first version merely waited for positive accounting. That did not prove
the finite buffers had filled before close, so it was strengthened before
retention. Both writers now use nonblocking sockets and must independently
observe `WouldBlock`; the lease must also account positive bytes in each
direction. Only then does the test call certified close. The resulting final
snapshot must report exactly one accepted connection, no active, completed, or
denied connection, and positive upload and download totals. Both writers must
return a terminal socket error, and the newly certified source identity must
attach again on the next iteration.

Ten fresh native release processes completed the 64-cycle lane in 4.03–7.35
seconds. The first saturated cycle used 9,056–9,168 KiB RSS, 18 descriptors,
and seven threads. After the last close every run held 9,392–9,488 KiB, 13
descriptors, and five threads; after shutdown they held 9,184–9,296 KiB, nine
descriptors, and two threads. In the full six-lane native process, the first
saturated cycle was 23,152 KiB and the last batch 23,216 KiB, a 64 KiB rise
after earlier allocator high-water marks. RSS remains reported rather than
asserted; exact ownership/counters and descriptor/thread recovery are gates.

This lane adds 229 lines and moves whole-tree SCC 4.0.0 complexity from
648/1,946 to 669/2,016 structural/cognitive, entirely in the opt-in resource
target. That is larger than the other resource cases. It was retained because
the two independent full-queue barriers, per-cycle certificate, source reuse,
and process recovery are one distinct end-to-end ownership signal; none of the
ordinary conformance tests substitute for its repeated state. Production code
and the 155 deterministic cases are unchanged.

The native factory and 23.65-second default resource run passed. Exact Rust
1.88 Linux completed the pressure lane in 445 ms: 6,224 KiB RSS, 13
descriptors, and seven threads at the first saturated cycle; 6,260 KiB,
eight/five after 64 closes; and 5,784 KiB, four/two after shutdown. The rootless
155/155 image remains byte-identical because only the opt-in resource target
and documentation changed:
`sha256:c669bc5cb96beb81c33ae8e4b93424aa574c826354135cbdf81b7108bcae98c2`
(40,717,343 bytes).

## 2026-09-01 — make DNS memory defaults reflect decoded response size

The returned-address ceiling bounds Sandbox Egress's own `Vec<IpAddr>`, but it
does not run until Hickory has decoded the complete DNS message. Auditing the
pinned Hickory 0.26.1 decoder found that query and record vectors reserve
directly from the unsigned 16-bit section counts. The current upstream source
has the same behavior and exposes no resolver option for a decoder count or
byte ceiling. On this arm64 target, `Record` is 272 bytes and `Query` is 88
bytes, so an answer count of 65,535 asks the allocator for 17,825,520 bytes of
record capacity before the first record is decoded.

That number initially looked worse than the observed behavior. Ten alternating
process runs compared a transaction-ID-only reply with a complete header
claiming 65,535 answers and no body. Both peaked around 12.1 MB RSS and
completed in 30 ms. The untouched capacity is a virtual reservation on this
allocator, so multiplying it by DNS concurrency would have overstated resident
memory.

A valid dense response exposed the durable cost. A temporary measurement case
decoded a 65,532-byte message containing 4,368 minimal A records. Five process
runs that decoded and dropped 64 messages peaked at 8,044,544 bytes RSS after
the cold run. Five runs retaining the same 64 decoded messages peaked at
83,755,008 to 83,771,392 bytes. The incremental 75,710,464 bytes is about
1,182,976 bytes per response. Source inspection confirmed why this matters:
for an ordinary matching answer, Hickory caches the complete decoded `Message`,
and its configured capacity counts responses rather than bytes. The prior
8,192-entry ceiling was finite in name but could retain an impractical amount
of memory. The temporary measurement source was removed; it tested dependency
representation rather than a Sandbox Egress contract.

Accepted. Resolver caching is now disabled by default. A host may opt in, but
the hard ceiling is 64 responses rather than 8,192, corresponding to roughly
75.7 MB of incremental RSS in the dense measurement instead of a multi-gigabyte
configuration. Default concurrent resolver work falls from 128 to 32 to reduce
transient decoder amplification by four while remaining host-configurable.
This is an explicit security/performance choice: uncached network names perform
a resolver lookup on each connection unless the host enables a small cache.
The hardening backlog retains a byte-aware upstream decoder/cache bound because
response-count ceilings remain an approximation.

A real UDP wire case now returns the maximum answer-section count with no
records. It must finish as one `502 dns-failed` denial after the resolver's six
bounded questions, make zero dial attempts, and close with no active work. It
and the neighboring incomplete-reply case each passed 25 consecutive focused
runs. The configuration proof pins the new zero-entry, zero-TTL, 32-lookup
defaults and the 64-entry opt-in maximum.

Four pre-change and three post-change local-hostname benchmark runs had
overlapping intervals: pre-change point estimates ranged from 144.47 to 156.47
microseconds and post-change from 153.03 to 157.23 microseconds. Criterion did
not detect a change in the three post-change runs, so no ordinary-path
regression or improvement is claimed. A remote uncached lookup is intentionally
not represented by that local hosts-file benchmark.

Whole-tree SCC 4.0.0 complexity remains exactly 633/1,893
structural/cognitive; production control flow is unchanged. The native and
exact Rust 1.88 Linux factories passed 153 deterministic cases, five doctests,
documentation, and package verification; native dependency policy checks also
passed. Linux's idle lane peaked at 8,396 KiB RSS, 521 descriptors, and six
threads, then returned to 6,020 KiB, four descriptors, and two threads after
shutdown. The control, 500-lease, and 2,000-terminal-connection lanes likewise
returned to four descriptors and two threads at 6,640, 6,524, and 5,404 KiB.
The rootless 153/153 image is
`sha256:239f534ed18ee39b1c9ddb50a51030069a5e8a314bc3fe09201b07d312e8f5e1`
(40,705,015 bytes).

## 2026-09-01 — drain buffered TLS records before reading again

The TLS suite covered every split inside one record and a large hello split
across legal records, but its helper always supplied some bytes coalesced with
the CONNECT request or delivered the remaining slice in a favorable shape. A
new roughly 60 KiB `ClientHello` case began with an empty initial buffer and
let the async reader return ordinary 4 KiB chunks. Its first run failed with
`UnexpectedEof` even though the complete valid hello was already in the
crate-owned buffer.

Rustls's incremental `Acceptor::read_tls` may consume only one complete TLS
record from a cursor that also contains bytes from the next record. Sandbox
Egress correctly retained the unconsumed suffix and tracked its offset, but
after `accept()` returned incomplete it read the socket again instead of first
feeding that suffix. Once the reader reached EOF, the function returned the
EOF result while still holding parseable bytes. The fix is one branch: when
the feed offset trails the buffer length, loop back to the parser before any
size check or socket read.

Two deterministic neighbors pin the boundary. The original empty-initial
shape now recovers the expected SNI and exact 60 KiB wire image. A custom async
reader then delivers the same legal multi-record hello one transport byte at a
time. Both passed 25 consecutive focused runs. In an optimized test binary, 25
process launches put the bytewise case at a 10.95 ms median versus 4.69 ms for
an empty harness, about 6.26 ms for roughly 60,000 incremental reads. This is a
bounded linear compatibility cost, not a throughput claim.

The adjacent ECH deframer audit found one complete handshake-body allocation
and copy after Rustls had already accepted the hello. The extension walk only
needs a borrowed slice while its local reassembly vector is alive, so the
deframer now truncates that vector to the declared handshake length and lends
the body directly. A detached pre-change worktree received the same feed-loop
fix, and both versions ran 500 large parses per sample. After two warmup
samples, the last eight of ten runs had medians of 16.92 microseconds before
and 15.49 microseconds after, an 8.4% reduction. The simpler allocation shape
was retained and the temporary measurement case and worktree were removed.

The first Rust 1.88 Linux factory run exposed a second false assumption in the
test harness. Requesting 1 KiB socket buffers did not guarantee backpressure;
Linux loopback occasionally accepted the complete 64 KiB hello, leaving the
guest open. Before the parser fix, the same case could pass with zero upstream
bytes because it failed during parsing rather than at the forwarding barrier.
The test connector now fills its bounded send queue to an observed
`WouldBlock` before returning the socket, records that exact prefill, and
asserts only bytes beyond it as ClientHello forwarding. The corrected barrier
passed 25 native runs and the repeated Linux factory.

Whole-tree SCC 4.0.0 complexity moves from 633/1,893 to 642/1,934
structural/cognitive. `tls.rs` moves from 57/166 to 60/175 for the feed branch
and two reader fixtures; the exact send-queue saturation harness moves
`tls_tests.rs` from 13/30 to 19/62. Production parser control flow adds only
the one backlog check. The native and exact Rust 1.88 Linux factories passed
155 deterministic cases, five doctests,
documentation, and package verification; native dependency policy checks also
passed. Linux's idle lane peaked at 8,400 KiB RSS, 521 descriptors, and six
threads, then returned to 4,804 KiB, four descriptors, and two threads. The
control, 500-lease, and 2,000-terminal-connection lanes returned to four and
two at 5,280, 5,280, and 5,484 KiB. The rootless 155/155 image is
`sha256:08fd76dd094047897fa22f50fac894e690a6ea675b9297fce78a9989822403ff`
(40,717,296 bytes).

## 2026-09-01 — expire bidirectionally backpressured tunnels

A silent socket is the simplest idle case, but it does not establish what
happens when hostile peers keep invoking writes after the network stops making
forward progress. A new tunnel proof makes guest and upstream write
continuously while neither consumes the opposite stream. The proxy initially
reads and accounts bytes in both directions. Once the finite TCP and copy
buffers fill, successful reads stop, the shared 100 ms activity clock expires,
and both writers must receive a terminal socket error.

The harness bounds both endpoint send and receive buffers to 16 KiB to reduce
host autotuning noise and gives each writer a five-second failure timeout.
Timeout is not an accepted terminal result. The first cold run took 2.26
seconds while later same-process runs settled near 0.53 seconds, reinforcing
why the proof asserts behavior rather than a narrow latency. The case passed 25
consecutive runs. Certified close afterward reports positive upload and
download accounting, exactly one idle denial, zero completions, and zero active
work.

Production code and the opt-in measurement targets are unchanged. Whole-tree
SCC 4.0.0 complexity moves from 628/1,880 to 633/1,893
structural/cognitive, entirely in the deterministic proof.
The native and exact Rust 1.88 Linux factories passed 151 deterministic cases,
documentation, and package verification; the native factory also passed
dependency policy checks. Linux's idle, control, lease-churn, and terminal
resource lanes each returned to four descriptors and two threads, at 6,120,
6,760, 6,476, and 5,528 KiB RSS respectively. The rootless 151/151 conformance
image is
`sha256:5418a9c56fb9f64072386d6216af93e761bcc379493b4e40ec4701a7b05adc24`
(40,705,142 bytes).

## 2026-09-01 — measure simultaneous idle-tunnel recovery

The idle policy had deterministic two-endpoint closure proofs but not a
process-level resource measurement under many simultaneous timers. The opt-in
resource target now establishes 128 real loopback tunnels under one immutable
lease and holds all guest and upstream sockets silent. It samples the live
peak, waits for policy expiry, requires terminal reads on all 256 endpoints,
and then samples recovery with the proxy alive and after shutdown. Final usage
must report exactly 128 accepted and denied connections, zero active or
completed connections, and zero bytes in either direction.

The lane uses one upstream thread that retains every accepted socket rather
than a thread per connection. It therefore exposes accidental proxy thread
growth separately from harness growth. A two-second idle interval keeps all
tunnels simultaneously active during setup without timing sleeps standing in
for proof: the lease's active gauge must equal the configured batch before the
peak sample. Guest and upstream reads carry five-second failure bounds, and a
blocked timeout is not accepted as terminal cleanup.

The 128-connection case passed ten consecutive release runs. In the final
native four-lane resource run, the idle peak was 13,600 KiB RSS, 526
descriptors, and six threads. After expiry it held 13,792 KiB, 13 descriptors,
and five threads with the proxy alive; after shutdown it held 13,632 KiB, nine
descriptors, and two threads. RSS is reported rather than thresholded because
the process allocator may retain released pages; descriptor, thread, active
ownership, and exact counters are enforced.

Whole-tree SCC 4.0.0 complexity moves from 620/1,860 to 628/1,880
structural/cognitive, entirely in the opt-in resource target and its script.
Production code and the 150 deterministic cases are unchanged. The native and
exact Rust 1.88 Linux factories passed documentation and package verification;
the native factory also passed dependency policy checks. Linux's idle lane
peaked at 8,412 KiB RSS, 521 descriptors, and six threads. With the proxy alive
it recovered to 7,340 KiB, eight descriptors, and five threads; after shutdown
it returned to 4,992 KiB, four descriptors, and two threads. The rootless
150/150 conformance image remains byte-identical because production and the
shipped deterministic cases are unchanged:
`sha256:9fdd4d14741dce47c20c0f047f40432a66ea43a342ca9c3f498e34a56d0f59c4`
(40,701,820 bytes).

## 2026-09-01 — pin independent ClientHello compatibility samples

The maintained parser already had generated Rustls-compatible ClientHellos,
every record split and truncation, large multi-record messages, GREASE, ECH,
and malformed-length cases. Those shapes are useful adversarial boundaries,
but generated test messages alone do not establish that independent deployed
clients remain compatible.

Two complete first TLS records are now fixed as offline test inputs. One came
from OpenSSL 3.6.3 `s_client`; the other came from Apple curl 8.7.1 using
SecureTransport. Both connected to a local listener that sent no response and
used `fixture.example` as SNI. The OpenSSL record is 1,546 bytes with SHA-256
`228f135c07a4d5491653e229e5c73302f51b589dcbce17c3695aad0ac91ec78f`;
the SecureTransport record is 325 bytes with SHA-256
`6c801c49925112cd01849a1ea4a0983ef740fd3a7bd049af6a49dd5d809142ed`.
The recorded invocations live beside the fixtures. Random and public ephemeral
key-share bytes are intentionally frozen; no server response or secret input
is present.

The compatibility case requires mature parsing, exact normalized SNI, explicit
absence of ECH, and byte-for-byte wire retention for each record. It passed 25
consecutive focused repetitions. Ordinary tests do not run either client or
access the network, so local client upgrades cannot silently change the
evidence. A broader versioned corpus remains open rather than treating these
two samples as representative of every deployed client.

Production code and runtime dependencies are unchanged. Whole-tree SCC 4.0.0
complexity moves from 669/2,016 to 670/2,019 structural/cognitive, entirely in
the small test loop. The native and exact Rust 1.88 Linux factories passed 156
deterministic cases, five doctests, documentation, and package verification;
native dependency policy checks also passed. Linux's six resource lanes passed.
The TLS lane peaked at 14,980 KiB RSS, 265 descriptors, and six threads, then
returned after shutdown to 5,920 KiB, four descriptors, and two threads. The
rootless 156/156 conformance image is
`sha256:2431bfc656ff9d10117af0e3c49e7a4f14802bedc62e6ba66f0d41f40ccb1d63`
(40,722,834 bytes) and runs as UID/GID 65534.

## 2026-09-01 — make explicit network denials authoritative

Smokescreen's separate allow/deny address controls exposed a useful missing
core rule. Sandbox Egress could grant a normally forbidden CIDR, but a lease
could not exclude a particular public destination range after allowing its
hostname. The first test deliberately failed to compile because
`PolicyBuilder::deny_network` did not exist.

The immutable policy now accepts explicit denied networks. A denial is checked
before the ordinary public-address behavior and before any overlapping grant.
One listener-level case gives the policy both a public `/24` denial and an
`0.0.0.0/0` grant. An allowed hostname resolving inside the `/24` receives
`resolved-address-denied`; the equivalent IP literal receives
`ip-literal-denied`. Neither path calls the connector, and certified close
returns exactly two accepted, two denied, and zero active connections.

The first implementation checked only the address spelling and would have let
a denied IPv4 range reappear through IPv4-mapped, compatible, or DNS64 forms.
The pure policy proof now covers those two forms, the well-known NAT64 prefix,
and a configured RFC 6052 prefix while both IPv4 and IPv6 catch-all grants are
present. Translation interpretation is shared with the default SSRF floor, so
the two policy paths cannot drift independently. Both focused cases passed 25
consecutive repetitions.

The initial translated-address draft raised whole-tree SCC 4.0.0 complexity
from 670/2,019 to 680/2,047 structural/cognitive. Centralizing the translation
walk reduced the retained result to 676/2,028: a net +6/+9 for the public rule,
its two proofs, and shared interpretation. No dependency, runtime task, or
connection-path allocation was added.

Two separate three-pair A/B runs compared the allowed-hostname benchmark with
a detached `e9387c0` worktree. In the final run, candidate intervals spanned
138.17–167.09 microseconds and baseline intervals spanned 136.91–155.73. The
first pair separated with the candidate slower; the next two overlapped, while
the earlier run moved in both directions. No stable regression or improvement
is claimed.

The native and exact Rust 1.88 Linux factories passed 158 deterministic cases,
five doctests, documentation, package verification, and all six Linux resource
lanes; native dependency policy checks also passed. Linux's TLS lane peaked at
15,068 KiB RSS, 265 descriptors, and six threads, then returned after shutdown
to 6,180 KiB, four descriptors, and two threads. The rootless 158/158 image is
`sha256:5cf7e1f8b27a91525e06a3aca1eb2cc647947fbca93262aa0d17836677b3b36f`
(40,737,201 bytes) and runs as UID/GID 65534.

## 2026-09-01 — add authoritative hostname carve-outs

Smokescreen's global deny rules highlighted the hostname counterpart to the
previous network-denial cycle. A wildcard grant is convenient for a sandbox
run, but it needs a small, explicit way to exclude a sensitive subdomain
without enumerating every permitted sibling. The first test deliberately
failed to compile because `PolicyBuilder::deny_host` did not exist.

The immutable policy now accepts denied hostname patterns using exactly the
same canonical ASCII exact and left-most-wildcard grammar as grants. A denial
wins over every exact or wildcard grant. It cannot create access by itself.
The pure policy proof combines a wildcard grant, an overlapping exact grant,
and an exact denial; the carve-out loses access while shallow and deep sibling
subdomains remain allowed and the wildcard apex remains closed.

A listener-level proof requests the denied name through a real CONNECT socket.
It receives the stable `host-denied` response before either the capturing
resolver or rejecting connector is called. Certified close returns exactly one
accepted, one denied, and zero active connections. Both focused proofs passed
25 consecutive repetitions.

Three alternating A/B benchmark pairs compared the empty-denial allowed-host
path against a detached `3c2456c` worktree. Candidate intervals spanned
145.73–183.13 microseconds and baseline intervals spanned 145.37–173.68. The
second pair overlapped tightly, opposite-side outliers widened the overall
ranges, and Criterion detected no candidate change in any pair. No stable
regression or improvement is claimed.

Whole-tree SCC 4.0.0 complexity moves from 676/2,028 to 678/2,035
structural/cognitive: +2/+7 for the rule, its proofs, and documentation. The
production representation adds one empty vector to each immutable policy; it
adds no dependency, runtime task, or connection-path allocation. The native
and exact Rust 1.88 Linux factories passed 160 deterministic cases, five
doctests, documentation, and package verification. Linux's six resource lanes
passed; the TLS lane peaked at 15,184 KiB RSS, 265 descriptors, and six
threads, then returned after shutdown to 6,604 KiB, four descriptors, and two
threads. The rootless 160/160 image is
`sha256:99c20d16acd0cf9f70e09a720c7c9d4874d374655e2c3c08b8ebfaa29cb4e5b7`
(40,740,380 bytes) and runs as UID/GID 65534.

## 2026-09-01 — remove the mirrored policy-builder state

This cycle intentionally pursued simplification rather than another feature.
`PolicyBuilder` mirrored all twelve `Policy` fields and then reconstructed the
same object field by field in `build()`. The builder now owns the not-yet-frozen
policy directly. Its methods mutate that private value; `build()` validates it
and transfers it intact. The public API, defaults, validation order, immutable
result, dependencies, tasks, and allocation shape are unchanged.

The first attempted refactor instead combined resolved-host and literal-IP
decisions behind an `allow_public_by_default` boolean. It saved one code line
but obscured a meaningful deny-by-default distinction and raised SCC cognitive
complexity from 2,035 to 2,039. That version was discarded. The retained
builder change removes 18 production lines while leaving structural/cognitive
complexity at 678/2,035.

Three alternating A/B lifecycle pairs compared the retained result with a
detached `4587c07` worktree. Candidate intervals spanned 1.3720–1.3973
milliseconds and baseline intervals spanned 1.3752–1.4071. Every pair
overlapped, so no performance change is claimed. The comparison worktree was
removed after measurement.

The native and exact Rust 1.88 Linux factories passed the unchanged 160
deterministic cases, five doctests, documentation, package verification, and
all six Linux resource lanes; native dependency policy checks also passed.
Linux's TLS lane peaked at 14,860 KiB RSS, 265 descriptors, and six threads,
then returned after shutdown to 5,948 KiB, four descriptors, and two threads.
The rootless 160/160 image is
`sha256:7cb8903747c512e24b99f20037960ecf0b859884c3fbb69f15fbaf93eee71222`
(40,740,438 bytes) and runs as UID/GID 65534.

## 2026-09-01 — compare and harden hostname normalization

The pinned Smokescreen, lens-sandbox-core, and nono references were refreshed.
Smokescreen remained at `d4da883a`, Lens remained at `2bc4ecc5`, and nono
advanced from `8f15fc86` to `7989b578`. Nono's intervening change concerns
environment expansion for credential local-socket paths and does not alter its
proxy. The reproducible pin and comparison notes were updated.

Both Smokescreen and nono explicitly prove that case changes and a trailing
DNS root dot cannot bypass hostname denials. Nono additionally proves wildcard
denial boundaries and supports compound host-and-port denies. Sandbox Egress
retains its simpler orthogonal hostname/port model: one implementation's
compound rule is not enough evidence to enlarge the core API. The shared
normalization and wildcard lessons did identify a conformance gap in the new
hostname-denial proof.

The policy case now requires a wildcard denial to reject a deep subdomain but
not its apex. The real-listener case sends both
`AdMiN.ExAmPlE.TeSt.:443` and a deep name under a wildcard denial. Both receive
`host-denied` before the capturing resolver or connector is called. Certified
close returns exactly two accepted, two denied, and zero active connections.
The two focused cases passed 25 consecutive repetitions.

Production source and dependencies are unchanged, so no performance benchmark
was run. Whole-tree SCC 4.0.0 complexity moves from 678/2,035 to 679/2,038
structural/cognitive, entirely in the expanded proof. The native and exact
Rust 1.88 Linux factories passed the unchanged 160 deterministic cases, five
doctests, documentation, package verification, and all six Linux resource
lanes; native dependency policy checks also passed. Linux's TLS lane peaked at
14,972 KiB RSS, 265 descriptors, and six threads, then returned after shutdown
to 6,172 KiB, four descriptors, and two threads. The rootless 160/160 image is
`sha256:2b68af6bc83f31743126eaeb72220a3214bec49684e00b749086e767a54cac92`
(40,740,569 bytes) and runs as UID/GID 65534.

## 2026-09-01 — reduce the initial CONNECT-header reservation

Every accepted connection previously requested 4 KiB of vector capacity before
reading its first CONNECT bytes. Ordinary requests in the benchmark are much
smaller, while the connection remains admission-owned through parsing, DNS,
and dialing. The initial request is now 1 KiB. Larger headers use the existing
bounded growth path and retain the same absolute byte limit, parser, deadline,
and revocation behavior. The change reduces requested starting capacity by
3 KiB per concurrent header-phase connection; allocator RSS is not inferred
from vector capacity.

Three alternating A/B pairs compared the candidate with a detached `f2f19c6`
worktree. Allowed CONNECT intervals were 109.77–138.52 microseconds for the
candidate and 103.88–146.14 for baseline; denied-host intervals were
73.43–85.45 and 68.25–89.74. Oversized 1 MiB intervals were 650.26–680.53 and
643.13–756.52; near-terminator 1 MiB intervals were 652.12–673.39 and
639.62–673.23. Every workload overlapped and results moved both directions, so
no latency change is claimed. The comparison worktree was removed.

Whole-tree SCC 4.0.0 complexity remains 679/2,038 structural/cognitive. No
dependency, task, limit, or public API changed. The native and exact Rust 1.88
Linux factories passed 160 deterministic cases, five doctests, documentation,
package verification, and all six Linux resource lanes; native dependency
policy checks also passed. Linux's TLS lane peaked at 14,748 KiB RSS, 265
descriptors, and six threads, then returned after shutdown to 6,096 KiB, four
descriptors, and two threads. The rootless 160/160 image is
`sha256:b0f1affa4b01e315eeee9c88f9cbc2b1bb8df47f31583e2cf79437ba9e7ba826`
(40,740,585 bytes) and runs as UID/GID 65534.

## 2026-09-01 — share immutable connection configuration

This performance/simplification cycle found that every admitted connection
cloned and retained the complete `ProxyConfig` for its full lifetime. That
object includes owned DNS-server and NAT64-prefix vectors even though proxy
startup freezes it and connection tasks only read it. The runtime now owns one
`Arc<ProxyConfig>` and each task clones that handle. The production change is
three lines and does not alter the public API, task count, limits, or security
decisions.

Three alternating 50,000-connection A/B pairs against detached `9ae7c31`
worktrees moved in both directions. Candidate throughput ranged from 17,056 to
19,718 connections/second and baseline from 18,578 to 19,578; latency ranges
also overlapped. No end-to-end performance change is claimed, and the detached
worktree was removed.

An added reproducible microbenchmark isolates the actual ownership operation
with eight configured DNS servers and six NAT64 prefixes. Direct `ProxyConfig`
cloning measured 58.900–61.846 ns; shared-handle cloning measured
9.858–10.029 ns. This supports the narrow structural claim that the admitted
task no longer allocates and copies populated vectors. Whole-tree SCC 4.0.0
complexity moves from 679/2,038 to 683/2,048 structural/cognitive, entirely in
the benchmark; production complexity is unchanged.

The native and exact Rust 1.88 Linux factories passed 160 deterministic cases,
five doctests, documentation, package verification, and all six Linux resource
lanes; native dependency policy checks also passed. Linux's TLS lane peaked at
14,892 KiB RSS, 265 descriptors, and six threads, then returned after shutdown
to 5,800 KiB, four descriptors, and two threads. The rootless 160/160 image is
`sha256:fcba3c575f7247c504f2ea6cd2872ce52e08b35b733bf93e6f0fda394929b2f6`
(40,738,534 bytes) and runs as UID/GID 65534.

## 2026-09-01 — deny Azure's public-looking host endpoint

The comparison rotation cloned ressrf at its already documented `52fc89cf`
pin and reviewed its provider data alongside official provider documentation.
AWS and GCP metadata endpoints are already covered by Sandbox Egress's
link-local and non-global IPv6 floor. Azure WireServer is different:
Microsoft documents `168.63.129.16` as a stable virtual public address for
host-platform and VM-agent services. The existing IANA-only classifier allowed
it.

A test first demonstrated that gap. The retained change adds the single `/32`
to the reviewed default floor. A real-listener test then resolves an allowed
hostname to that address, requires `resolved-address-denied`, proves zero dial
attempts, and certifies final 1/1/0 accepted/denied/active counters. It passed
25 consecutive focused repetitions. Broad provider domain suffixes and service
range imports were not retained: the proxy already pins and checks every DNS
answer, and those larger mutable datasets would expand policy without closing
this specific public-address hole. A trusted host can still grant the `/32`
explicitly under the existing floor-override contract.

Three allowed-hostname A/B pairs against detached `5dc2754` all overlapped.
Candidate intervals were 148.57–241.49, 154.23–170.30, and 156.27–173.33
microseconds; baseline intervals were 152.38–166.38, 154.97–168.98, and
154.88–174.07. No latency change is claimed. The comparison worktree was
removed. Whole-tree SCC 4.0.0 complexity remains 683/2,048
structural/cognitive.

The first full-factory run exposed a proof-harness bug: the prefix-boundary
test formed its host mask with `u32::MAX >> length`, which overflows when the
table first contains a `/32`. Production matching was unaffected. The proof now
uses checked shifts for both `/32` and a future IPv6 `/128`, and the complete
factory was restarted rather than accepting only the focused tests.

The native and exact Rust 1.88 Linux factories then passed 161 deterministic
cases, five doctests, documentation, package verification, and all six Linux
resource lanes; native dependency policy checks also passed. Linux's TLS lane
peaked at 15,116 KiB RSS, 265 descriptors, and six threads, then returned after
shutdown to 5,956 KiB, four descriptors, and two threads. The rootless 161/161
image is
`sha256:2940aa20f2fc68d460ca2dcfa7baf1ee3f5d34ff0a15f796275aaba9f3931934`
(40,738,584 bytes) and runs as UID/GID 65534.

## 2026-09-01 — borrow phase permits inside bounded work

This simplification rotation found an inconsistency between the DNS and dial
budgets. Both operations acquire a permit and finish entirely inside one helper,
but DNS cloned its semaphore `Arc` and requested an owned permit while dialing
borrowed its permit. DNS now borrows too, and both helper signatures accept
`&Semaphore` rather than exposing shared ownership they do not retain. The
change removes one production line and one atomic shared-owner operation from
each hostname lookup without changing a task, deadline, permit lifetime, or
public API.

The focused DNS-capacity cancellation and DNS-deadline cases pass. Three
alternating allowed-hostname A/B pairs against detached `7b63b9a` all overlap;
candidate intervals were 139.56–162.71, 144.74–157.94, and 147.66–169.33
microseconds, while baseline intervals were 141.71–159.46, 148.88–165.85, and
147.75–163.44. Medians moved in both directions, so no latency change is
claimed. The comparison worktree was removed. Whole-tree SCC 4.0.0 complexity
moves from 683/2,048 to 683/2,046 structural/cognitive.

The native and exact Rust 1.88 Linux factories passed the unchanged 161
deterministic cases, five doctests, documentation, package verification, and
all six Linux resource lanes; native dependency policy checks also passed.
Linux's TLS lane peaked at 14,792 KiB RSS, 265 descriptors, and six threads,
then returned after shutdown to 5,840 KiB, four descriptors, and two threads.
The rootless 161/161 image is
`sha256:94ab401a97e01bded1a3b3810d3e62ad0153f38b22d2bdf2626dac0446b52651`
(40,736,538 bytes) and runs as UID/GID 65534.

## 2026-09-01 — certify revocation after a tunnel half-close

The comparison rotation cloned motosan-sandbox at the documented `13eab245`
pin and reviewed its CONNECT proxy. Its use of Tokio's `copy_bidirectional`
correctly preserves ordinary directional EOF. Its shutdown handle aborts the
listener task, while connection handlers are spawned without retained join
ownership. This exposed a useful conformance question for Sandbox Egress:
could certified close terminate the still-live half of a tunnel after the
other half had already finished?

A controlled upstream now reads until the guest's upload FIN propagates, then
deliberately withholds its response. Lease close must return in under 500
milliseconds without releasing that upstream, freeze zero active ownership,
make the guest read terminal, and make the released upstream writer observe a
terminal socket error. Final completion and denial counters remain zero because
revocation, not normal EOF or policy refusal, ended the tunnel. The proof passed
25 consecutive focused repetitions.

The first focused run tried to install the guest read timeout after close had
already made the socket terminal; macOS rejected that test-side operation with
`EINVAL`. Moving the timeout before the half-close removed the harness race.
The upstream result wait was also given a two-second scheduling margin while
its own write is still bounded to one second. Neither correction changes the
500-millisecond close requirement.

Production source and dependencies are unchanged, so no performance benchmark
was run. Whole-tree SCC 4.0.0 complexity moves from 683/2,046 to 686/2,055
structural/cognitive, entirely in the new conformance proof. The native and
exact Rust 1.88 Linux factories passed 162 deterministic cases, five doctests,
documentation, package verification, and all six Linux resource lanes; native
dependency policy checks also passed. Linux's TLS lane peaked at 14,840 KiB
RSS, 265 descriptors, and six threads, then returned after shutdown to 6,156
KiB, four descriptors, and two threads. The rootless 162/162 image is
`sha256:a495ddba8cb9ea129e90ec76a15b2e7927ebac79b6eff6006bfbaedeceacc8eb`
(40,739,469 bytes) and runs as UID/GID 65534.

## 2026-09-01 — rejected monolithic connection-runtime ownership

This performance rotation tested whether each admitted task should retain one
shared `ConnectionRuntime` instead of cloning the four independently shared
resolver, connector, phase-budget, and configuration handles. The candidate
also placed the DNS and dial semaphores directly inside that runtime. It removed
three production lines, three per-connection `Arc` clones, and several startup
`Arc` allocations while preserving the task and permit lifetimes. All 112
library cases and strict linting passed.

Six alternating 50,000-connection measurements used 64 clients and 16 loopback
destinations. Candidate throughput was 18,613, 18,433, 17,787, 19,843, 18,247,
and 17,910 connections/second. The pinned `d452613` baseline produced 19,364,
19,650, 19,067, 19,337, 19,265, and 18,483. The order was reversed for the
last three pairs; five of six still favored the baseline. Median candidate
throughput was approximately 18,340 connections/second versus 19,301 for the
baseline, and candidate p95 latency was not consistently better.

The candidate was therefore discarded in full. The existing independently
shared handles remain: reducing shared-owner operations in isolation was not a
measured end-to-end improvement. No production source, dependency, public API,
test count, or whole-tree 686/2,055 structural/cognitive complexity changed.

## 2026-09-01 — unify proxy-shutdown error ownership

This simplification rotation removed three repeated constructions of a
`ShutdownError` carrying the still-owned proxy. The synchronous shutdown path
now computes one `Result<(), ShutdownErrorKind>` from reply delivery and runtime
thread joining, then attaches ownership to an error in one place. Successful
join, deadline failure, disconnected runtime, and a panicked or unavailable
runtime thread retain their existing behavior; the fallible shutdown boundary
and retryable owner are unchanged.

The retained change removes 13 production lines. Whole-tree SCC 4.0.0
complexity moves from 686/2,055 to 685/2,050 structural/cognitive. Four focused
shutdown retry and proxy/lease race cases passed before the full factory. No
performance benchmark was run because the path executes once per process-wide
shutdown and changes no connection work, allocation, task, dependency, or
public API.

The native and exact Rust 1.88 Linux factories passed the unchanged 162
deterministic cases, five doctests, documentation, package verification, and
all six Linux resource lanes; native dependency policy checks also passed.
Linux's TLS lane peaked at 15,140 KiB RSS, 265 descriptors, and six threads,
then returned after shutdown to 6,316 KiB, four descriptors, and two threads.
The rootless 162/162 image is
`sha256:5221971ab058ede00994ea6f930495372b95acdf288f55f5619b4e9a5cfc02c3`
(40,740,442 bytes) and runs as UID/GID 65534.

## 2026-09-01 — deny recursive proxy destinations

This comparison rotation reviewed Smokescreen at the pinned `d4da883a`
revision. Smokescreen enumerates local interface addresses and rejects a
destination whose address is local and whose port is the proxy listener. A
new public proof demonstrated the corresponding Sandbox Egress gap: after an
otherwise legitimate policy explicitly granted loopback and the listener
port, a literal CONNECT back into the shared listener received `200` and could
start a nested proxy chain.

Sandbox Egress now freezes the actual post-bind listener address before it can
dispatch a connection. Literal and DNS results matching that endpoint are
rejected as `proxy-endpoint-denied` before policy grants and before dialing.
The comparison intentionally produced a smaller library-boundary guard rather
than importing interface enumeration: concrete listener addresses match
exactly after IPv4-mapped canonicalization, while wildcard listeners reject
loopback on the bound family and dual-stack IPv6 wildcard listeners reject
both loopback families. The documented host-cage contract remains responsible
for other local aliases exposed by wildcard or address-translated deployments.

The failing-before-change literal proof now receives `403` with exactly one
accepted, one denied, and zero active connections. A controlled hostname proof
resolving to the listener observes the same accounting and zero connector
calls. Both passed 25 consecutive focused repetitions; a unit matrix also
covers mapped addresses, wildcard loopback, remote exclusion, and the port
boundary.

Three alternating Criterion A/B pairs measured the allowed-loopback CONNECT
path against detached `dd2fbf1`. Candidate intervals were 108.11–123.16,
112.51–127.94, and 112.16–128.52 microseconds; baseline intervals were
113.58–131.39, 109.30–125.37, and 111.73–129.28. Every pair overlaps, so no
latency change is claimed. The comparison worktree was removed. Whole-tree SCC
4.0.0 complexity moves from 685/2,050 to 694/2,077 structural/cognitive,
including the production guard and its public and controlled proofs.

The first rootless run exposed a latent scheduling race in the older queued
identity-reuse proof: it started close before proving that the old socket was
queued, so cancellation could correctly win before the test's expected denial
was counted. The proof now queues and writes the old socket under command
pressure before starting close, with a larger scheduling margin. It passed ten
strengthened repetitions. The exact Linux factory was restarted rather than
accepting a retry, and then passed the corrected case again.

The native and final exact Rust 1.88 Linux factories passed 165 deterministic
cases, five doctests, documentation, package verification, and all six Linux
resource lanes; native dependency policy checks also passed. Linux's TLS lane
peaked at 14,700 KiB RSS, 265 descriptors, and six threads, recovered to
10,524 KiB, eight descriptors, and five threads, and finished at 5,928 KiB,
four descriptors, and two threads. The rootless 165/165 image is
`sha256:de0aba6cb82f87555845ed2d10097fb640e4c775e3957be9c4e78759fb4c3950`
(40,743,228 bytes) and runs as UID/GID 65534.

## 2026-09-01 — rejected inline single-address iterator

This performance rotation tested removing the one-element `Vec` allocation
from the common direct-IP path. The candidate represented a literal address
inline and retained a consuming vector iterator only for bounded DNS answers.
It preserved address ordering, exact-size fallback budgeting, and all phase
lifetimes; strict linting passed.

Three alternating Criterion pairs measured direct loopback CONNECT setup
against detached `5de90d0`. Candidate intervals were 104.18–117.22,
113.56–131.85, and 114.69–147.02 microseconds; baseline intervals were
111.05–116.33, 112.43–115.17, and 114.33–127.65. Every pair overlapped and
the median moved in both directions.

Four alternating 50,000-connection runs then used 64 clients and 16 loopback
destinations. Candidate throughput was 18,075, 17,517, 18,367, and 17,330
connections/second; baseline throughput was 17,728, 18,195, 16,989, and
18,816. Each side won two pairs. Median candidate throughput was approximately
17,796 connections/second versus 17,961 for the baseline, with no consistent
tail-latency improvement.

The candidate was discarded in full. One small allocation is below the
socket-and-scheduler variance of the measured path, while a custom iterator
would add production type surface. The comparison worktree was removed; source,
dependencies, public API, test counts, and whole-tree 694/2,077
structural/cognitive complexity remain unchanged.

## 2026-09-01 — flatten phase-permit ownership

This simplification rotation found two redundant shared-owner layers. Every
connection already retains one `Arc<PhasePermits>` for the full task, while
the DNS and dial semaphores inside that structure were each independently
wrapped in another `Arc` even though neither escapes the parent. They are now
owned directly by `PhasePermits` and borrowed by bounded work. Permit capacity,
queue cancellation, deadline behavior, and release-before-tunnel lifetimes are
unchanged.

The retained change removes two startup allocations and makes the ownership
graph match the actual lifetime. The focused DNS and dial concurrency and
cancellation proofs passed. No performance benchmark was run because this is
process-start allocation work and changes no per-connection clone, task, permit,
or data path. Whole-tree SCC 4.0.0 complexity remains exactly 694/2,077
structural/cognitive.

The native and exact Rust 1.88 Linux factories passed the unchanged 165
deterministic cases, five doctests, documentation, package verification, and
all six Linux resource lanes; native dependency policy checks also passed.
Linux's TLS lane peaked at 14,908 KiB RSS, 265 descriptors, and six threads,
recovered to 9,784 KiB, eight descriptors, and five threads, and finished at
6,140 KiB, four descriptors, and two threads. The rootless 165/165 image is
`sha256:87bc9c60d85fe6ec11807b0a987d00d716f9bd97c7840d2651e82dbb5a5658b1`
(40,742,174 bytes) and runs as UID/GID 65534.

## 2026-09-01 — operator-owned upstream HTTP proxy

This comparison-and-feature rotation reviewed NVIDIA OpenShell at pinned
revision `f7180c0f`. Smokescreen, Lens, nono, and OpenShell all support chaining
through an operator-controlled proxy. OpenShell supplied the decisive security
lesson: resolve and validate locally, then send only the approved numeric
address to the next proxy. Its optional hostname mode transfers resolution
authority upstream and was not adopted.

A public test was written first and failed to compile because
`ProxyConfig::with_upstream_proxy` did not exist. The retained implementation
adds one process-wide, host-configured numeric `SocketAddr` and cleartext HTTP
CONNECT transport. The existing sequence remains authoritative: parse the
guest request, apply hostname and port policy, resolve locally, reject the
entire answer set if any address is forbidden, then ask the upstream proxy for
one already-approved numeric destination. Guest headers are reconstructed
rather than forwarded. Ambient proxy variables, hostname CONNECT, bypass
rules, authentication, TLS transport, and CA configuration are deliberately
outside this first transport contract.

Upstream TCP setup and CONNECT negotiation share the existing dial permit and
absolute handshake deadline. The response uses `httparse`, permits at most 64
headers and 32 KiB, requires a 2xx status, and preserves response-coalesced
tunnel bytes for ordinary download accounting and ceilings. A configured
upstream endpoint equal to the actual shared listener is rejected at startup.
Because negotiation occurs inside the lease-owned connection task, certified
close cancels a stalled upstream without adding a second task registry or
shutdown protocol.

Five public real-socket cases prove the numeric authority exactly, including
IPv6 formatting and the absence of guest-supplied headers; coalesced response
payload preservation and exact accounting; stable refusal and oversized-header
denials; sub-500-millisecond cancellation of a silent upstream with terminal
sockets; and listener self-reference rejection. Each focused case passed ten
consecutive runs before the full factory.

The first correct implementation wrapped direct and proxied sockets in one
prefix-aware stream. Three alternating 64 MiB by eight-worker download pairs
then found the candidate consistently behind detached `e05fee6`: 3,136,
3,342, and 3,416 MiB/sec versus 3,647, 3,614, and 3,435. The branch checking an
empty prefix on every direct read was not retained. The final design matches
the direct or proxied transport once after dialing and calls a generic tunnel
routine, preserving a concrete, monomorphized `TcpStream` direct path.

Three warm alternating pairs after that simplification measured 3,183, 3,332,
and 3,243 MiB/sec for the candidate versus 3,252, 3,314, and 3,066 for the
parent. The candidate won two pairs and the medians differ by about 0.3%, so
the initial regression is removed and no speedup is claimed. The comparison
worktree was removed. Whole-tree SCC 4.0.0 complexity moves from 694/2,077 to
725/2,159 structural/cognitive, including the new transport and public proofs.

The native and exact Rust 1.88 Linux factories passed 173 deterministic cases,
six doctests, documentation, package verification, and all six Linux resource
lanes; native dependency policy checks also passed. Linux's TLS lane peaked at
14,876 KiB RSS, 265 descriptors, and six threads, recovered to 7,696 KiB,
eight descriptors, and five threads, and finished at 6,104 KiB, four
descriptors, and two threads. The rootless 173/173 image is
`sha256:5bdd4c12676d03f833666a25f080eeda6c28a5a8fefaf09f6325f70ac9cac00e`
(40,862,091 bytes) and runs as UID/GID 65534.

## 2026-09-01 — upstream negotiation benchmark

This performance follow-up adds a reproducible Criterion target for one full
guest CONNECT routed through a local upstream HTTP proxy. The controlled peer
requires the exact numeric request before returning success, so the benchmark
includes the second TCP setup, reconstructed request, bounded response parse,
and guest success response. It uses no public DNS or network and changes no
production source or dependency.

Three runs measured `connect_via_upstream_proxy` at 126.38–131.53,
131.06–145.81, and 129.58–133.82 microseconds. Three adjacent direct allowed
CONNECT controls measured 105.44–128.53, 112.20–125.25, and 109.35–137.79
microseconds. The centers consistently show the expected cost of another
loopback handshake and response parse, but two of three interval pairs overlap;
no precise overhead percentage or optimization claim is justified. This is a
baseline for later transport work, not a release gate.

Whole-tree SCC 4.0.0 complexity moves from 725/2,159 to 729/2,171
structural/cognitive, entirely in benchmark code. The benchmark target passed
strict all-target linting and three optimized measurement runs. The native and
exact Rust 1.88 factories passed the unchanged 173 deterministic cases, six
doctests, documentation, package verification, benchmark smoke, and all six
Linux resource lanes. Production behavior and the deterministic conformance
count remain unchanged, so the assembled rootless image remains
`sha256:5bdd4c12676d03f833666a25f080eeda6c28a5a8fefaf09f6325f70ac9cac00e`.

## 2026-09-01 — coalesced upstream payload ceiling proof

This security follow-up examined the boundary between upstream CONNECT
negotiation and metered tunnelling. A cooperative or hostile upstream may send
tunnel payload in the same socket read as its successful response. Those bytes
have already entered the proxy before the tunnel copier starts, so retaining
them outside the ordinary download ceiling would create a small policy leak.

A new real-socket case sends `secret` immediately after the upstream response
header while the lease permits one downloaded byte. The guest receives exactly
`s`, final accounting records all six observed bytes, the connection is denied
rather than completed, and active ownership reaches zero. The case passed 25
consecutive runs.

Production behavior already held the invariant because the prefix-aware stream
sits inside the same metered reader as later socket bytes. No production source
or dependency changed and no performance benchmark was run. Whole-tree SCC
4.0.0 complexity moves from 729/2,171 to 730/2,173 structural/cognitive,
entirely in the public conformance proof.

The native and exact Rust 1.88 Linux factories passed 174 deterministic cases,
six doctests, documentation, package verification, benchmark smoke, and all six
Linux resource lanes; native dependency policy checks also passed. Linux's TLS
lane peaked at 14,988 KiB RSS, 265 descriptors, and six threads, recovered to
11,148 KiB, eight descriptors, and five threads, and finished at 6,620 KiB,
four descriptors, and two threads. The rootless 174/174 image is
`sha256:a08585327c02a6d62b347e5d5564de9a4ca1622193efd5de1891987f8948a65d`
(40,865,285 bytes) and runs as UID/GID 65534.

## 2026-09-01 — flatten connector modes

This simplification rotation removed an operational sum type hidden inside
another sum type. The connector previously had a `System` variant containing
`Option<SocketAddr>`, so both dialing and denial attribution repeated nested
`Some` and `None` patterns. It now has explicit `Direct` and `Upstream`
variants, with the process configuration converted once at startup. Test-only
connector injection remains a third explicit variant.

The retained change removes ten production lines and makes invalid combinations
unrepresentable without changing tasks, allocations, public API, or the hot
tunnel loop. Whole-tree SCC 4.0.0 complexity remains exactly 730/2,173
structural/cognitive.

Three alternating Criterion pairs measured allowed direct CONNECT against
detached `4eced6f`. Candidate intervals were 104.45–120.36, 109.51–130.04,
and 116.59–144.21 microseconds; parent intervals were 110.74–127.03,
115.06–118.20, and 116.12–144.48. Every pair overlaps, so no performance
change is claimed. The comparison worktree was removed.

The native and exact Rust 1.88 Linux factories passed the unchanged 174
deterministic cases, six doctests, documentation, package verification,
benchmark smoke, and all six Linux resource lanes; native dependency policy
checks also passed. Linux's TLS lane peaked at 14,724 KiB RSS, 265 descriptors,
and six threads, recovered to 12,780 KiB, eight descriptors, and five threads,
and finished at 6,564 KiB, four descriptors, and two threads. The rootless
174/174 image is
`sha256:4e2b625edafd9934118c9f71269038299e52358174b877a640f63cd4dd3a33c6`
(40,866,141 bytes) and runs as UID/GID 65534.

## 2026-09-01 — bound concurrent upstream negotiations

This comparison follow-up tested whether the new upstream response phase truly
belongs to the existing dial budget. Releasing a permit after the upstream TCP
connect but before its CONNECT response would let a hostile guest turn a small
dial ceiling into many simultaneous negotiations and 32 KiB response buffers.

A new public barrier starts four guest connections with two process-wide dial
permits. The local upstream accepts and verifies exactly two numeric CONNECT
requests, withholds both responses, and observes no third connection for 200
milliseconds. Certified lease close then cancels the two live negotiations and
the two queued permit waits; all four guest sockets are terminal and final
usage reports four accepted, zero denied, and zero active connections. The
focused case passed ten consecutive runs.

Production behavior already held the invariant because `connect_via` remains
inside the permit-owning connector future. No production source, dependency,
or data path changed, so no performance benchmark was run. Whole-tree SCC 4.0.0
complexity moves from 730/2,173 to 739/2,192 structural/cognitive, entirely in
the public concurrency proof.

The native and exact Rust 1.88 Linux factories passed 175 deterministic cases,
six doctests, documentation, package verification, benchmark smoke, and all six
Linux resource lanes; native dependency policy checks also passed. Linux's TLS
lane peaked at 14,564 KiB RSS, 265 descriptors, and six threads, recovered to
9,156 KiB, eight descriptors, and five threads, and finished at 5,956 KiB, four
descriptors, and two threads. The rootless 175/175 image is
`sha256:f8b4bb00ca2425eeefe5f7368caeecf8926de3709113d21442f06f0a0bbfa0aa`
(40,879,838 bytes) and runs as UID/GID 65534.

## 2026-09-01 — upstream response scan benchmark

This performance rotation identified a bounded but avoidable CPU shape in the
new upstream response reader. After every 1 KiB read it rescans the full
accumulated buffer for the four-byte terminator. A response at the 32 KiB
ceiling therefore repeats work across 32 increasingly large prefixes.

A new offline Criterion target makes that path reproducible. Its local peer
verifies the numeric CONNECT request, then sends exactly 32 KiB of repeated
`\r\n\rX` near matches with no terminator. The guest must receive the ordinary
bounded 502 denial. Three baseline runs measured 553.65–598.69,
553.73–592.69, and 558.32–627.98 microseconds.

The benchmark setup was generalized rather than duplicated and still runs the
successful upstream negotiation target. No production source or dependency
changed. Whole-tree SCC 4.0.0 complexity moves from 739/2,192 to 741/2,200
structural/cognitive, entirely in benchmark code and its documentation. The
measured baseline will be committed before changing the scanner so the next
rotation can compare against an exact parent.

The native and exact Rust 1.88 factories passed the unchanged 175 deterministic
cases, six doctests, documentation, package verification, both upstream
benchmark smokes, and all six Linux resource lanes. Production artifacts are
unchanged, so the rootless image remains
`sha256:f8b4bb00ca2425eeefe5f7368caeecf8926de3709113d21442f06f0a0bbfa0aa`.

## 2026-09-01 — incrementally scan upstream responses

The exact-parent comparison justified replacing full-prefix rescans with an
incremental scan. After each read, the scanner now resumes three bytes before
the old buffer end: the smallest overlap that still finds a four-byte header
terminator divided across reads. A unit matrix covers each possible divided
terminator position.

Three alternating optimized A/B pairs measured the 32 KiB near-match response
at 162.04–198.27 microseconds for this change versus 488.41–584.24
microseconds for parent `ef0f873`. The ordinary successful upstream path
remained overlapping: 123.40–154.02 versus 131.72–153.28 microseconds. This
retains a roughly threefold hostile-input improvement without claiming a
change to ordinary negotiation performance.

Whole-tree SCC 4.0.0 complexity is 742 structural and 2,203 cognitive, one and
three above the benchmark commit respectively; the production loop has fewer
branches and the increase belongs to its boundary test.

The native and exact Rust 1.88 Linux factories passed 176 deterministic cases,
six doctests, documentation, package verification, both upstream benchmark
smokes, and all six Linux resource lanes; native dependency policy checks also
passed. Linux's TLS lane peaked at 14,848 KiB RSS, 265 descriptors, and six
threads, recovered to 8,788 KiB, eight descriptors, and five threads, and
finished at 5,960 KiB, four descriptors, and two threads. The rootless 176/176
image is
`sha256:edef1e2f9de754b1e690dfea53a8bf2421b6d889632fdf85cf02a90aebde09a8`
(40,881,035 bytes) and runs as UID/GID 65534.

## 2026-09-01 — share HTTP header framing

This simplification rotation moved the identical four-byte HTTP header boundary
search into the CONNECT parsing module and reused it from both the guest and
upstream response readers. Their protocol-specific byte ceilings, read sizes,
errors, parser limits, and lifecycle ownership remain local. Production code
drops five lines; whole-tree SCC 4.0.0 moves from 742/2,203 to 741/2,200
structural/cognitive.

An initial unannotated shared helper produced unstable code generation: its
third A/B pair reached 190.90–216.42 versus 176.50–189.97 microseconds upstream
and 704.94–744.33 versus 662.43–719.05 microseconds on the 1 MiB guest case.
The shared pure primitive is now an explicit inline boundary. Three fresh
alternating comparisons against exact parent `69c9d1a` overlap on both paths:
the candidate spans 168.68–201.40 versus 174.40–198.46 microseconds upstream,
and 660.46–677.47 versus 650.75–704.19 microseconds for the guest reader. No
performance change is claimed.

The native and exact Rust 1.88 Linux factories passed the unchanged 176
deterministic cases, six doctests, documentation, package verification,
benchmark smoke, and all six Linux resource lanes; native dependency policy
checks also passed. Linux's TLS lane peaked at 15,156 KiB RSS, 265 descriptors,
and six threads, recovered to 10,524 KiB, eight descriptors, and five threads,
and finished at 5,800 KiB, four descriptors, and two threads. The rootless
176/176 image is
`sha256:0b95259bdfb24767fc3c28678a2ced6708054828189d220f35923ada331b66a7`
(40,880,595 bytes) and runs as UID/GID 65534.

## 2026-09-01 — compare validated-address fallback

The OpenShell upstream-proxy review exposed a proof-level gap: Sandbox Egress
already retried validated addresses, but its fallback test stopped at an
internal connector while the public upstream tests used one address. A new
shared-listener case performs one absolute hostname lookup, receives two
explicitly approved test-network addresses, and captures two operator-proxy
requests. The first numeric CONNECT receives 502, the second receives 200 with
coalesced tunnel bytes, and neither request contains the hostname. The guest
then completes a bidirectional exchange and certified close returns one
accepted, one completed, zero denied, zero active, and exact byte counts.

The focused case passed ten consecutive runs. Production source and the hot
path are unchanged, so no performance benchmark is warranted. Whole-tree SCC
4.0.0 complexity moves from 741/2,200 to 744/2,208 structural/cognitive,
entirely in controlled resolver and upstream-server conformance code.

The native and exact Rust 1.88 Linux factories passed 177 deterministic cases,
six doctests, documentation, package verification, benchmark smoke, and all
six Linux resource lanes; native dependency policy checks also passed. Linux's
TLS lane peaked at 14,804 KiB RSS, 265 descriptors, and six threads, recovered
to 11,092 KiB, eight descriptors, and five threads, and finished at 5,944 KiB,
four descriptors, and two threads. The rootless 177/177 image is
`sha256:aceae73647d83eade3bfaa3751e3f572f5ca0cb1d10af614be2bec37ca58ffce`
(40,888,181 bytes) and runs as UID/GID 65534.

## 2026-09-01 — stop fallback after revocation

The comparison follow-up exposed a narrow phase-transition race. The outer
connection select prioritizes cancellation, but one poll of the dial future can
receive a refusal and immediately enter its next loop iteration after that
select last observed the token. The dial loop now observes lease cancellation
before every address attempt.

A deterministic connector barrier starts the first address, cancels the token,
and only then releases that attempt as a failure. With the guard removed, the
case fails with attempts `[192.0.2.1:443, 192.0.2.2:443]`; restored, it passes
20 consecutive runs with only the first address. This makes the revocation
claim local to the transition instead of relying solely on an outer future
poll.

Three alternating optimized comparisons against exact parent `6d2f9ea`
overlap. Direct allowed CONNECT spans 109.91–141.59 microseconds for the
candidate versus 111.46–146.79 for the parent; upstream CONNECT spans
133.54–181.31 versus 135.64–164.93 microseconds. No performance change is
claimed. Whole-tree SCC 4.0.0 complexity moves from 744/2,208 to 750/2,229
structural/cognitive; one production branch provides the check and the rest is
the phase-barrier proof.

The native and exact Rust 1.88 Linux factories passed 178 deterministic cases,
six doctests, documentation, package verification, benchmark smoke, and all
six Linux resource lanes; native dependency policy checks also passed. Linux's
TLS lane peaked at 14,720 KiB RSS, 265 descriptors, and six threads, recovered
to 8,572 KiB, eight descriptors, and five threads, and finished at 5,956 KiB,
four descriptors, and two threads. The rootless 178/178 image is
`sha256:67616fd4ef80064a9b0c931ebdc0a9fc5f837915063cff37dfedfaadf3172e9f`
(40,896,008 bytes) and runs as UID/GID 65534.

## 2026-09-01 — reject borrowed byte-accounting counters

The next performance rotation began with five fresh 10,000-connection load
runs at concurrency 64 and sixteen loopback destinations. They completed at
15,724–22,035 connections/second (median 17,818); four p50 observations were
1,733–1,940 microseconds, while p95 and p99 showed host-scheduling tails. The
spread reinforces the rule that a small source-level reduction needs paired
microbenchmark evidence before it can be retained.

A candidate let each tunnel's metering wrappers borrow the lease counters
instead of retaining three `Arc` owners. All 120 library cases passed. Three
optimized allowed-CONNECT measurements were 114.07–125.06,
116.27–133.75, and 118.43–136.47 microseconds. The three immediately preceding
baseline intervals were 110.58–131.63, 110.08–125.45, and
117.25–133.52 microseconds. Every pair overlapped, Criterion detected no
change, and the candidate did not trend better.

The lifetime-bearing wrapper was discarded. The existing explicit shared
ownership remains simpler, no production source or test count changes, and no
performance improvement is claimed.

## 2026-09-01 — merge revocation phase and arrival generation

The simplification rotation observed that lease phase and the generation used
to certify an identity's quiet period lived behind two mutexes, even though
every generation mutation was already ordered by the phase lock. The
generation now belongs directly to `Phase::Revoking(u64)`. This makes the
state-machine relationship explicit, removes a mutex from every lease, removes
fifteen production lines, and eliminates nested lock ownership.

The two sensitive quiet-period cases each passed thirty focused repetitions.
The native and exact Rust 1.88 Linux factories passed the unchanged 178
deterministic cases, six doctests, documentation, package verification,
benchmark smoke, and all six Linux resource lanes; native dependency policy
checks also passed. Linux's TLS lane peaked at 14,944 KiB RSS, 265 descriptors,
and six threads, recovered to 8,008 KiB, eight descriptors, and five threads,
and finished at 6,748 KiB, four descriptors, and two threads.

Whole-tree SCC 4.0.0 complexity moves from 750/2,229 to 749/2,229
structural/cognitive. The rootless 178/178 image is
`sha256:3186a93287671d8533c0ef1c40857d1b283f243133609908de89029286c4f381`
(40,892,697 bytes) and runs as UID/GID 65534.

## 2026-09-01 — move legacy numeric proof onto the resolver wire

Ressrf's strict ambiguous-address parser prompted a comparison follow-up.
Sandbox Egress intentionally does not infer an effective IP from legacy text:
the host must allow that text as a hostname, and the resolver's result still
crosses the complete address floor. The existing conformance case proved this
with an injected answer, leaving the maintained system-resolver boundary
implicit.

The case now uses the production Hickory backend and an explicit local UDP DNS
server. Dotted shorthand, leading-zero dotted form, hexadecimal integer, and
single decimal integer each generate A and AAAA wire questions, receive a
loopback answer, return `resolved-address-denied`, and make zero connector
calls. It passed ten consecutive focused runs. This replaces rather than adds
a test, changes no production source or public contract, and keeps whole-tree
complexity at 749/2,229 structural/cognitive.

The native and exact Rust 1.88 Linux factories passed the unchanged 178
deterministic cases and all six Linux resource lanes. Linux's TLS lane peaked
at 14,928 KiB RSS, 265 descriptors, and six threads, recovered to 8,516 KiB,
eight descriptors, and five threads, and finished at 5,932 KiB, four
descriptors, and two threads. The rootless 178/178 image is
`sha256:1853aa84db5fa45ed8af6b5e4d5e36310f0b41ac534f6934e3fa6ab96105c87d`
(40,892,714 bytes) and runs as UID/GID 65534.

## 2026-09-01 — reject borrowed connection-task ownership

The next performance rotation tested whether an admitted task should borrow
its lease state and cancellation token directly from `Admission` instead of
retaining one owner of each. The candidate removed two hot-path shared-owner
operations and two production lines, and all 120 library cases passed.

Three baseline allowed-CONNECT intervals were 115.24–128.09,
116.03–130.69, and 119.21–125.42 microseconds. Three candidate intervals were
113.96–127.72, 116.58–133.18, and 118.07–135.12 microseconds. Every pair
overlapped and Criterion detected no change. The candidate was discarded; the
task's explicit independent ownership remains, and no performance, complexity,
or behavior claim changes.

## 2026-09-01 — extend forbidden CNAME proof to seven links

The DNS hardening inventory still called out longer noncyclic alias chains.
The existing one-hop CNAME-to-metadata fixture now returns seven successive
aliases before its terminal A record resolves to `169.254.169.254`. Hickory
must issue sixteen controlled A/AAAA wire questions, the connector must remain
untouched, and certified close must retain the single denial.

The strengthened case passed 25 consecutive focused runs. It replaces the
shorter proof, so the deterministic count stays at 178 and production code is
unchanged. Whole-tree SCC 4.0.0 complexity moves from 749/2,229 to 749/2,230
structural/cognitive, entirely in the controlled DNS response fixture.

The native and exact Rust 1.88 Linux factories passed all 178 deterministic
cases, six doctests, documentation, package verification, benchmark smoke, and
all six Linux resource lanes. The rootless 178/178 conformance image is
`sha256:26ec942dfca2d4cb451913598f404a28c56271f579e49f05d4d4c659e7544cd4`
(40,893,554 bytes) and runs as UID/GID 65534.

## 2026-09-01 — reject single-allocation denial responses

The next performance rotation tested constructing a denial response in one
formatted allocation instead of first formatting its body separately. Three
baseline denied-CONNECT intervals were 73.68–79.11, 71.09–88.42, and
75.24–90.38 microseconds. Three candidate intervals were 71.90–79.23,
70.52–73.50, and 71.06–78.51 microseconds.

The candidate trended lower but every comparison overlapped and Criterion
detected no change. It was discarded: one fewer small allocation is not enough
evidence to alter the denial path. Production source, complexity, and behavior
remain unchanged.

## 2026-09-01 — unify direct and proxied connected streams

The simplification rotation found that direct and upstream-proxied connections
entered identical tunnel setup through two enum arms. A single connected-stream
wrapper now represents either an empty prefix or bytes coalesced after a
validated upstream CONNECT response. This removes the transport enum and the
duplicated setup branch without weakening the approved-numeric-address handoff.

Three baseline direct-CONNECT intervals were 111.14–124.32, 113.19–118.32,
and 114.62–128.46 microseconds. Three candidate intervals were 111.46–126.49,
113.66–121.18, and 113.07–131.08 microseconds. Every comparison overlapped and
Criterion detected no change, so no regression is claimed. All 120 library
cases pass. Whole-tree SCC 4.0.0 complexity moves from 749/2,230 to 748/2,228
structural/cognitive.

The native and exact Rust 1.88 Linux factories passed all 178 deterministic
cases, six doctests, documentation, package verification, benchmark smoke, and
all six Linux resource lanes. Linux's TLS lane peaked at 14,920 KiB RSS, 265
descriptors, and six threads, recovered to 11,116 KiB, eight descriptors, and
five threads, and finished at 5,856 KiB, four descriptors, and two threads.
The rootless 178/178 image is
`sha256:f161db7099386ae0278eb17e12517792c6bf77a7edb458d076b6b21cbe57cd89`
(40,861,824 bytes) and runs as UID/GID 65534.

## 2026-09-01 — bound listener-error retries without weakening close

The comparison rotation inspected listener failure at Smokescreen `d4da883a`,
Lens `2bc4ecc5`, current nono `46867b2f`, and Motosan `13eab245`. Smokescreen
inherits Go `net/http`'s 5 millisecond exponential delay with a one-second
ceiling. Lens and nono warn and immediately retry; Motosan ends its per-run
accept task. Sandbox Egress also immediately retried, and a failed accept in
the certified-close drain requeued its command. Persistent descriptor pressure
could therefore consume a runtime worker and circulate a management request.

Ordinary listener failures now arm a Tokio timer, doubling from 5 milliseconds
to a one-second ceiling while the same select loop continues servicing
management commands. A successful accept or listener drain resets the delay.
A failed mandatory drain returns `ListenerUnavailable` to close or replacement
attachment. It is never interpreted as an empty queue, and close retains lease
ownership so an uninspected old socket cannot inherit a replacement policy.

A deterministic state-level case pins the bounded delay and reset. All 121
library cases pass. Whole-tree SCC 4.0.0 complexity moves from 748/2,228 to
754/2,238 structural/cognitive; the increase is the explicit three-way drain
result and retry state required to distinguish empty, bounded-progress, and
listener-failure outcomes.

The native and exact Rust 1.88 Linux factories passed all 179 deterministic
cases, six doctests, documentation, package verification, benchmark smoke, and
all six Linux resource lanes. Linux's TLS lane peaked at 14,880 KiB RSS, 265
descriptors, and six threads, recovered to 11,620 KiB, eight descriptors, and
five threads, and finished at 6,304 KiB, four descriptors, and two threads.
The rootless 179/179 image is
`sha256:027431a73ad8feed861c02fe7a834a81c54a291af2b12a736b4bbd9cb33740af`
(40,877,983 bytes) and runs as UID/GID 65534.

## 2026-09-01 — reject doubling runtime workers

The next performance rotation tested four owned Tokio workers against the
existing two under 10,000 local CONNECTs, 128 concurrent clients, and 32
destinations. Five two-worker runs produced 16,630–19,915 connections/second
with a 17,727 median. Five four-worker runs produced 16,169–19,111 with a
17,855 median, only 0.7% higher and well inside run variance. Median p50 setup
latency moved from 3,949 to 4,151 microseconds.

The candidate would also add two steady threads to every embedded proxy. It
was discarded: this load is not improved by broader scheduling, and the fixed
two-worker runtime remains the smaller, more reproducible default. Production
source, complexity, and behavior remain unchanged.

## 2026-09-01 — make accept retry one state object

The simplification rotation revisited the listener hardening immediately. Its
first secure form kept the current delay and optional retry deadline in two
separately mutable variables and maintained them through free helper functions.
`AcceptBackoff` now owns both values and exposes the three lifecycle operations:
failure schedules, timer expiry resumes, and a successful accept or drain
recovers.

This removes eight production lines and makes invalid delay/deadline pairings
unrepresentable inside the runtime loop. Fifty repeated backoff-boundary cases
and the two sensitive identity-drain cases pass. Whole-tree SCC 4.0.0 remains
754/2,238 structural/cognitive; the simplification is state ownership rather
than a claimed metric reduction.

The native and exact Rust 1.88 Linux factories passed all 179 deterministic
cases, six doctests, documentation, package verification, benchmark smoke, and
all six Linux resource lanes. Linux's TLS lane peaked at 14,676 KiB RSS, 265
descriptors, and six threads, recovered to 11,284 KiB, eight descriptors, and
five threads, and finished at 8,668 KiB, four descriptors, and two threads.
The rootless 179/179 image is
`sha256:03e37c866319b40374ceb5520399ea45c656e3ac22778db23c583bed203c5b82`
(40,878,564 bytes) and runs as UID/GID 65534.

## 2026-09-01 — compare provider floors and pin absolute header time

The next comparison rotation checked current nono `46867b2f` and ressrf
`52fc89cf` provider inventories. AWS IMDS, ECS task metadata, and the AWS IPv6
metadata endpoint are already covered by Sandbox Egress's link-local and
non-global floors. Azure WireServer is already the explicit globally
classified exception. Provider hostname literals add no stronger guarantee at
this boundary because hostname access is allowlisted and every resolution is
still checked before the approved numeric address is dialed. No blocklist
change was retained.

The hardening follow-up proves that continuous header activity cannot renew a
run's absolute handshake deadline. A bounded duplex stream delivers one byte
per millisecond for a 50 millisecond deadline and must still produce the stable
`408 header-timeout` denial. The case passed 50 consecutive focused runs; the
separate real-listener case passed 25 consecutive runs and retains exact wire
response and final-accounting coverage.

An initial combined real-socket test was deliberately discarded after one of
17 runs observed `ConnectionReset`: continuing to write after the proxy sends
its denial can make the kernel discard unread response bytes when closing with
pending input. That makes it unsuitable evidence for deadline semantics, not a
proxy deadline failure. Separating deterministic deadline behavior from the
real wire response removes that transport race without weakening either claim.

Generalizing the internal header reader from `TcpStream` to bounded
`AsyncRead` adds no public API or runtime path. The deterministic count moves
from 179 to 180. Whole-tree SCC 4.0.0 complexity moves from 754/2,238 to
757/2,253 structural/cognitive, all in the isolated test and generic bound.

The native and exact Rust 1.88 Linux factories passed all 180 deterministic
cases, six doctests, documentation, package verification, benchmark smoke, and
all six Linux resource lanes. Linux's TLS lane peaked at 14,736 KiB RSS, 265
descriptors, and six threads, recovered to 10,668 KiB, eight descriptors, and
five threads, and finished at 5,884 KiB, four descriptors, and two threads.
The rootless 180/180 image is
`sha256:f269178ebab0210338e0ad3241ed269f6c9c23214907dddf5250f83f44fba0c4`
(40,903,102 bytes) and runs as UID/GID 65534.

## 2026-09-01 — reject linear DNS-answer deduplication

The next performance rotation tested replacing the resolved-address hash set
with duplicate checks against the bounded output vector. The candidate removed
one allocation and one internal collection. Five baseline hostname-CONNECT
intervals were 147.11–151.55, 149.20–162.85, 150.36–152.56,
151.92–168.72, and 152.57–155.95 microseconds. Five candidate intervals were
150.18–176.31, 150.72–172.48, 151.54–165.51, 150.79–176.21, and
153.55–170.54 microseconds. Every comparison overlapped and Criterion detected
no change.

The candidate was discarded. It provided no measurable end-to-end benefit and
would replace expected constant-time duplicate checks with quadratic work for
a hostile maximum-size DNS answer. Production source, behavior, test count,
and complexity remain unchanged.

## 2026-09-01 — share one bounded header framer

The simplification rotation found two independent implementations of the same
security-sensitive framing loop: one for the guest CONNECT request and one for
an upstream proxy's CONNECT response. Both accumulated a bounded byte vector,
resumed scanning three bytes before the previous read boundary, distinguished
oversize from EOF, and returned bytes coalesced past the terminator.

One crate-private `read_bounded_header` primitive now owns those mechanics.
Each caller keeps its original ceiling, read chunk size, mature parser, and
denial mapping. No public API, allocation shape, task, or dependency changes.
Focused guest and upstream framing suites pass, including exact-limit,
split-terminator, 1 MiB near-terminator, and 32 KiB upstream near-terminator
cases.

Three exact-parent comparisons put allowed CONNECT at 107.42–140.55
microseconds for the candidate and 114.57–149.01 for the baseline. The
intervals overlap, so no ordinary-path improvement is claimed. Three 32 KiB
upstream near-terminator comparisons put the candidate at 173.79–199.96
microseconds and the baseline at 176.50–228.92; this rules out a measured
regression without treating the favorable direction as a durable speedup.

The consolidation removes ten production code lines. Whole-tree SCC 4.0.0
complexity moves from 757/2,253 to 751/2,236 structural/cognitive while the
deterministic test count remains 180.

The native and exact Rust 1.88 Linux factories passed all 180 deterministic
cases, six doctests, documentation, package verification, benchmark smoke, and
all six Linux resource lanes. Linux's TLS lane peaked at 14,948 KiB RSS, 265
descriptors, and six threads, recovered to 10,256 KiB, eight descriptors, and
five threads, and finished at 6,192 KiB, four descriptors, and two threads.
The rootless 180/180 image is
`sha256:41133bfefe81f2821cf16af889aebd453de02d38283d15081f2d70c7f8f6c63e`
(40,904,531 bytes) and runs as UID/GID 65534.

## 2026-09-01 — measure revocation of partial header pressure

The next comparison rotation revisited admission scope. Smokescreen exposes
separate request-processing and live-tunnel concurrency limits, while current
nono increments its active count before dispatch. Sandbox Egress deliberately
uses one global and one lease permit from admission through task destruction:
headers, DNS, dialing, and tunnelling consume the same bounded ownership. The
contract already held, but the resource factory did not isolate partial HTTP
headers as a peak-and-recovery lane.

A seventh opt-in resource case now opens 128 connections under one lease,
writes incomplete CONNECT headers, and waits until every admission is live.
Certified close must then make every guest socket terminal without waiting for
the 30-second header deadline, return 128 accepted with zero active, completed,
denied, upload, or download counts, and recover descriptor and thread counts
both before and after proxy shutdown. The focused case passed ten consecutive
runs. One representative macOS run peaked at 10,864 KiB RSS, 269 descriptors,
and five threads; it recovered to 11,088 KiB, thirteen descriptors, and five
threads after close, then 11,072 KiB, nine descriptors, and two threads after
shutdown.

Production source, API, allocation, and behavior are unchanged. Whole-tree SCC
4.0.0 complexity moves from 751/2,236 to 756/2,246 structural/cognitive,
entirely in the opt-in resource case; the deterministic count remains 180.

The native and exact Rust 1.88 Linux factories passed all 180 deterministic
cases, six doctests, documentation, package verification, benchmark smoke, and
all seven Linux resource lanes. The new Linux header lane peaked at 7,332 KiB
RSS, 264 descriptors, and five threads, recovered to 7,268 KiB, eight
descriptors, and five threads after close, and finished at 5,860 KiB, four
descriptors, and two threads. Production artifacts did not change, so the
rootless 180/180 image remains
`sha256:41133bfefe81f2821cf16af889aebd453de02d38283d15081f2d70c7f8f6c63e`
(40,904,531 bytes), running as UID/GID 65534.

## 2026-09-01 — reduce the initial header reserve

The next performance rotation measured the shared header framer's initial
allocation. It reserved 1 KiB for every admitted guest or upstream handshake
even though an ordinary CONNECT header is much smaller. The candidate reserves
256 bytes initially and retains the same configured ceiling, 4 KiB stack read
buffer, incremental scan, and vector growth for larger headers.

Five baseline allowed-CONNECT intervals were 110.56–122.09,
115.11–119.86, 113.34–133.08, 114.43–133.24, and 120.26–166.33
microseconds. Five candidate intervals were 112.10–114.02, 112.92–126.07,
114.93–154.87, 114.95–117.00, and 116.39–134.49 microseconds. The intervals
overlap, so no setup-latency improvement is claimed. Three 1 MiB
near-terminator baselines were 650.99–685.63 microseconds; three candidates
were 656.38–691.83 microseconds, also overlapping.

The resource effect was exact enough to retain. Three 512-connection partial
header baselines peaked at 16,912, 16,912, and 16,928 KiB RSS. Three candidate
runs peaked at 16,528, 16,544, and 16,464 KiB. Median peak RSS fell by 384
KiB, exactly 512 times the 768-byte reserve reduction. No public API, branch,
task, dependency, test count, or complexity changes.

The native and exact Rust 1.88 Linux factories passed all 180 deterministic
cases, six doctests, documentation, package verification, benchmark smoke, and
all seven Linux resource lanes. Linux's 128-connection header lane peaked at
7,284 KiB RSS, 264 descriptors, and five threads and recovered to eight
descriptors/five threads after close and four/two after shutdown. The TLS lane
peaked at 14,696 KiB RSS, 265 descriptors, and six threads, recovered to 9,928
KiB, eight descriptors, and five threads, and finished at 5,936 KiB, four
descriptors, and two threads. The rootless 180/180 image is
`sha256:352afe0968c2a281386adfdd6f7122385b6267b26ce59acf274d81449a90225c`
(40,904,544 bytes) and runs as UID/GID 65534.

## 2026-09-01 — keep shared framing buffers exact

The simplification follow-up audited the memory shape introduced by the shared
header framer. Its runtime `chunk_bytes` parameter preserved 1 KiB upstream
reads, but the async future always contained the function's 4 KiB array. That
quietly added 3 KiB of task state to every concurrent upstream-proxy handshake
even though the I/O contract and heap allocation were unchanged.

The chunk size is now a const generic. Guest and upstream callers still share
one source implementation, while the compiler emits futures with their exact
4 KiB and 1 KiB arrays. A temporary size measurement, run in both debug and
optimized builds and then removed, reported 4,184 bytes for the guest future
and 1,112 bytes for the upstream future: exactly 3,072 bytes apart. At 256
concurrent upstream negotiations this avoids roughly 768 KiB of unnecessary
task state.

Focused guest and upstream boundary suites pass. No public API, I/O behavior,
heap allocation, task count, dependency, deterministic test count, or
whole-tree 756/2,246 structural/cognitive complexity changes.

The complete native and exact Rust 1.88 factories pass: 180 deterministic
tests, seven opt-in resource lanes, six documentation examples, all feature
sets, formatting, lints, dependency duplication, package verification, and
release builds. On Linux, the 128 partial-header lane peaked at 7,328 KiB,
264 descriptors, and five threads before recovering to 6,440 KiB, eight
descriptors, and five threads. The 64 partial-ClientHello lane peaked at
14,820 KiB, 265 descriptors, and six threads before recovering to 9,824 KiB,
eight descriptors, and five threads. The rootless 180/180 image is
`sha256:39500504c0ae7f3bd6fd96b709e1e89a5b76e15e3f6c1d43eb01003607805678`
(40,904,092 bytes) and runs as UID/GID 65534.

## 2026-09-01 — pressure upstream-response revocation

The comparison rotation revisited the upstream-proxy behavior shared by
Smokescreen, Lens, nono, and OpenShell. Existing deterministic coverage proved
one stalled upstream response could be revoked, while the resource suite only
pressured direct CONNECT phases. That left concurrent ownership, parser state,
and descriptor recovery in the optional route unmeasured.

An eighth opt-in resource lane now creates 128 real guest sockets and 128 real
upstream-proxy sockets. Every upstream peer reads the locally generated numeric
CONNECT request, returns 900 bytes of a valid but unterminated response header,
and waits. A barrier proves every connection is live before sampling.
Certified lease close must make every socket on both sides terminal and return
exact counters: 128 accepted, zero active, completed, denied, uploaded, and
downloaded.

Ten fresh-process macOS runs passed. Peak resource use was 11,216–11,296 KiB
RSS, exactly 526 descriptors, and six threads. Certified close returned to 13
descriptors and five threads; shutdown returned to nine and two. RSS remained
at 11,280–11,392 KiB after shutdown and is recorded as allocator high-water
behavior rather than used as the cleanup oracle. The change is test and
documentation only; whole-tree SCC 4.0.0 structural/cognitive complexity moves
from 756/2,246 to 764/2,264.

The complete native and pinned Rust 1.88 factories pass: 180 deterministic
tests, eight opt-in resource lanes, six documentation examples, all feature
sets, formatting, lints, dependency policy, package verification, benchmark
smoke, and release builds. The new Linux lane peaked at 7,684 KiB, 521
descriptors, and six threads; certified close recovered to 6,348 KiB, eight
descriptors, and five threads, and shutdown returned to four descriptors and
two threads. The rootless 180/180 image is
`sha256:e42a64d0567d618a444c577c2365c4a022b08019b8cf71623e400fa6c46902c0`
(40,904,086 bytes) and runs as UID/GID 65534.

## 2026-09-01 — reject a zero-byte forwarding shortcut

The next performance rotation tested an early return from uninspected initial
upload forwarding when the CONNECT header ended exactly at the read boundary.
The candidate avoided one saturating atomic update and one empty `write_all`
future on the ordinary no-pipelined-upload path.

Five baseline `connect_allowed_loopback` intervals spanned 112.25–145.00
microseconds. Five candidate intervals spanned 111.91–148.05 microseconds, and
every Criterion comparison crossed zero (`p=0.38..0.81` in the candidate
runs). The end-to-end socket cost made the shortcut unmeasurable. The branch
was removed: the uniform accounting path is easier to read, and no performance
claim is retained. Production source, test count, dependencies, and complexity
are unchanged.

## 2026-09-01 — freeze policy deadlines from one instant

The simplification rotation found two separate `Instant::now()` calls while
`PolicyBuilder::build` checked the handshake and optional idle durations.
Policy freezing now captures one monotonic instant and validates both durations
against it. This makes the construction decision internally consistent at a
clock boundary, removes a redundant clock read, and leaves the public API and
ordinary accepted policy set unchanged.

The zero, ordering, handshake-overflow, and idle-overflow policy cases pass.
Whole-tree structural/cognitive complexity remains 764/2,264. No dependency,
task, allocation, connection-path, or deterministic test count changes.

The native and pinned Rust 1.88 factories pass all 180 deterministic tests,
eight Linux resource lanes, six documentation examples, formatting, lints,
dependency policy, package verification, benchmark smoke, and release builds.
The rootless 180/180 image is
`sha256:5f9072aebb39b1ac66adbfc39eaae9da305deecf5ee62023d0261817aa71614e`
(40,903,722 bytes) and runs as UID/GID 65534.

## 2026-09-01 — define byte-limit and transport-error precedence

The hardening rotation compared Sandbox Egress's metering with Smokescreen's
instrumented connection. Smokescreen accounts the byte count returned alongside
a Go I/O error; Sandbox Egress also has to decide whether an exact per-tunnel
ceiling or an independent transport failure owns the terminal classification.
The existing socket cases deliberately kept those paths separate, leaving that
adjacent boundary unstated.

A deterministic `AsyncRead` proof now drives both orderings. Three successful
bytes at a three-byte limit followed immediately by reset preserve the reset,
account exactly three bytes, and do not synthesize a policy failure. If a
successful fourth byte is observed first, it is accounted, not forwarded, and
returns the transfer-limit marker before the later reset can be polled. The
security and testing contracts now state that precedence, and the completed
backlog item is removed.

The focused case passes and production code is unchanged. The deterministic
test count rises from 180 to 181; whole-tree SCC 4.0.0 structural/cognitive
complexity moves from 764/2,264 to 767/2,275, entirely in the test-only proof.

The complete native and pinned Rust 1.88 factories pass all 181 deterministic
tests, eight Linux resource lanes, six documentation examples, formatting,
lints, dependency policy, package verification, benchmark smoke, and release
builds. The rootless 181/181 image is
`sha256:fd3da1b7eb0d0a7b7a2e5102fec03fc9ddd17355220383cb2364d27d4d1bb57d`
(40,919,749 bytes) and runs as UID/GID 65534.

## 2026-09-01 — locate the local CONNECT scaling knee

The performance rotation repeated the sustained CONNECT concurrency sweep
three times per point instead of optimizing from the original single run. At
1, 8, 32, 64, 128, and 256 callers, median rates were 6,348, 17,676, 19,284,
19,948, 17,505, and 16,570 connections/sec. Median p99 setup latency was 175,
419, 1,303, 3,042, 8,409, and 79,435 microseconds respectively. The 256-caller
p99 range widened from 41.7 to 160.1 milliseconds.

The measurement confirms saturation around 32–64 callers on this host. More
client concurrency beyond that range adds queueing and unstable tails without
raising aggregate setup throughput. No production tuning is retained. A small
`measure-load-sweep.sh` wrapper records the standard six-point, three-repeat
experiment so future changes can compare the whole curve rather than one
favorable point.

## 2026-09-01 — prove asymmetric full-duplex delivery

The comparison rotation reviewed Raincoat `811c8330` and canister `27434158`.
Both products own plain-HTTP framing and application-policy machinery that
would materially widen this CONNECT-only crate. That state is not imported.
The review instead exposed a gap in the shared tunnel core: performance cases
moved bytes in one direction per run, while simultaneous backpressure cases
proved cancellation but not successful complete delivery.

A deterministic real-socket case now moves 1,048,699 patterned bytes from the
guest while 3,145,771 different patterned bytes move from the upstream at the
same time. Each peer verifies the complete payload. Certified close must then
return the exact independent byte totals, one accepted and completed
connection, and zero active or denied connections. Ten consecutive focused
runs pass.

Production source, dependencies, and public API are unchanged. The
deterministic test count rises from 181 to 182; whole-tree SCC 4.0.0
structural/cognitive complexity moves from 767/2,275 to 770/2,282, entirely in
the integration proof.

The complete native and pinned Rust 1.88 factories pass all 182 deterministic
tests, eight Linux resource lanes, six documentation examples, formatting,
lints, dependency policy, package verification, benchmark smoke, and release
builds. The rootless 182/182 image is
`sha256:aa074cc361cfb510cca4fd8c42c117fc37d112a38c83b2e89b68abd6596a00c4`
(40,922,281 bytes) and runs as UID/GID 65534.

## 2026-09-01 — prove guest headers cannot select a lease

The next hardening rotation revisited CONNECT authority comparisons and found
no second destination selector in the parser. It did find that the stronger
identity promise—host-observed source address, never a guest header—was stated
but lacked a public real-socket proof.

The new case attaches a restrictive policy to `127.0.0.1` and a more permissive
policy to `127.0.0.2`. A real client observed as the first identity sends the
second address in `X-Run-ID` while requesting a destination only the second
policy would allow. The request receives the first policy's
`ip-literal-denied`, the destination accepts no connection, the observed lease
owns the one accepted and denied request, and the claimed lease remains at
zero. Ten consecutive focused runs pass.

Production source, dependencies, public API, and whole-tree SCC 4.0.0
structural/cognitive complexity remain unchanged at 770/2,282. The
deterministic test count rises from 182 to 183.

The complete native and pinned Rust 1.88 factories pass all 183 deterministic
tests, eight Linux resource lanes, six documentation examples, formatting,
lints, dependency policy, package verification, benchmark smoke, and release
builds. The rootless 183/183 image is
`sha256:5fb495f20016082b25062a8fdcb811bc8fed4c3ff9e751562fb24c4b0e2f5cb9`
(40,923,086 bytes) and runs as UID/GID 65534.

## 2026-09-01 — locate the fixed-work tunnel scaling knee

The next performance rotation held aggregate traffic at exactly 1 GiB per
direction while sweeping 1, 2, 4, 8, 16, and 32 established tunnels. Three-run
median upload rates were 2,548, 3,725, 3,550, 3,540, 3,233, and 2,241 MiB/sec;
download medians were 2,739, 3,877, 3,734, 3,712, 2,828, and 1,200 MiB/sec.

Two tunnels materially outperform one and align with the owned runtime's two
workers. Four through eight remain on the broad plateau, while 16 declines and
32 adds severe contention and variance. No production tuning is retained. A
small `measure-throughput-sweep.sh` wrapper now reproduces the fixed-work,
six-point, three-repeat experiment rather than changing behavior from a single
favorable measurement.

## 2026-09-01 — remove an unusable public policy type

The simplification rotation audited every direct dependency and exported type.
All eight runtime dependencies still own a deliberate maintained-parser,
resolver, runtime, cancellation, network, or error boundary. One exported type
did not: `HostPattern` was public even though no public API accepted or returned
it. Callers can only install hostname rules through `PolicyBuilder::allow_host`
and `deny_host`, which already parse and validate strings.

`HostPattern` and its parser are now crate-private. This removes an orphan
public compatibility obligation before 0.1 without changing the builder,
immutable policy representation, accepted rule set, allocations, tasks, or
data path. The deterministic test count and whole-tree 770/2,282
structural/cognitive complexity remain unchanged.

## 2026-09-01 — separate listener guarantees from cage guarantees

The comparison rotation reviewed RunSeal `001b0dd6`, whose black-box
proxy-mode conformance explicitly probes environment overrides, direct TCP and
UDP, unrelated loopback, host IPC, and inherited-socket bypasses. These are
valuable deployment tests, but they exercise the process and network cage: a
listener-only library cannot observe or revoke a connection that never reaches
its accept loop.

A new deployment contract now divides the guarantees precisely. Sandbox
Egress owns accepted-connection attribution, immutable policy, resolution,
accounting, cancellation, and certified close. The host owns unspoofable source
identity, direct-protocol confinement, resolver and upstream isolation,
descriptor inheritance, unrelated local endpoints, and identity-reuse order.
The README and security invariants link that contract, and the backlog now
calls for a black-box Linux/Firecracker harness covering the same escape paths.

No runtime behavior, public API, dependencies, tests, or complexity changed.
This pass deliberately does not put OS-specific cage machinery into the
embeddable proxy crate or weaken the meaning of successful `Lease::close`.

## 2026-09-01 — reject a hostname-index optimization

The next performance rotation tested whether the deliberately simple linear
hostname-rule vectors become a meaningful connection bottleneck. A temporary
end-to-end Criterion case installed 1,024 unmatched exact hostname grants and
requested a denied hostname. Three runs measured 64.54–75.58 microseconds. The
equivalent empty-policy control measured 65.33–79.86 microseconds across three
runs on the same tree.

No end-to-end regression was detectable: TCP setup, parsing, denial writing,
and close dominate this path. A hash index would add another representation,
hashing behavior, and small-policy overhead without measured benefit. The
candidate idea and temporary benchmark are discarded; the existing vectors
remain. Ordinary allowed-hostname control intervals were also recorded at
138.68–169.75 microseconds, with ordinary host resolution and upstream setup
still dominating. Production code, permanent benchmarks, and behavior are
unchanged.

## 2026-09-01 — normalize repeated hostname rules

The simplification rotation left the listener/control-plane state machine
intact: its single owner and select loop make the shutdown ordering easier to
audit than a set of helpers sharing mutable state. The policy audit did find
redundant retained state. Repeating the same canonical exact or wildcard grant
or denial previously stored another identical matcher for the lifetime of the
lease.

`PolicyBuilder::build` now canonically sorts and deduplicates each rule vector
before freezing it. This uses the existing representation and adds no matching
branch or secondary index. A focused case covers duplicate exact grants,
wildcard grants, and denials. Public API, rule precedence, and accepted
hostname behavior are unchanged. Structural/cognitive complexity remains
770/2,282, and the deterministic test count rises from 183 to 184.

## 2026-09-01 — stop diagnostic work after receiver disconnect

The comparison rotation refreshed nono from `46867b2f` to `d3c6f6b0`. Its only
intervening proxy change disables a standalone audit buffer that has no
consumer; otherwise the buffer fills and every later request produces more
operational noise. Sandbox Egress already uses a bounded nonblocking channel,
but the review exposed a smaller mismatch in its own promise: after receiver
disconnect, every denial still locked and advanced the rate state before the
send failed.

The reporter now records receiver disconnection in one atomic flag. The first
failed send performs the existing bounded work and disables the path; later
denials return before rate-state locking and event construction. A deterministic
case drops the receiver, reports twice, and requires only the first attempt to
consume a rate slot. Enforcement and denial accounting remain upstream of
diagnostic delivery and are unchanged. Fifty consecutive focused runs pass;
whole-tree structural/cognitive complexity moves from 770/2,282 to 771/2,285,
localized to the reporter and its proof. The deterministic test count rises
from 184 to 185.

The complete native factory passes all 185 deterministic tests, six
documentation examples, formatting, all-target lints, documentation,
dependency policy, package verification, benchmark smoke, and release build.
The pinned Rust 1.88 Linux factory also passes all 185 deterministic cases and
all eight serialized resource lanes. Its rootless conformance image is
`sha256:cb1212cd1aa40d4936e2d4185adfd61d39261a0a4df89a83785ea4c3e140b4c1`
(40,948,020 bytes) and runs as UID/GID 65534.

## 2026-09-01 — recheck the final connection curve

The final performance rotation reran the six-point, three-repeat fixed-load
sweep on the exact 185-case tree. Median rates for concurrency 1, 8, 32, 64,
128, and 256 were 6,350.8, 18,858.8, 20,095.5, 17,959.7, 18,974.8, and
15,515.1 connections per second. Median p99 latencies were 190, 413, 1,336,
3,963, 6,556, and 82,236 microseconds.

The middle points move within local scheduler and socket noise, but the shape
agrees with the earlier run: useful capacity saturates around 32–64 clients,
and 256 materially worsens tail latency while reducing throughput. The recent
policy and diagnostic changes do not justify a runtime tuning change. No
production code is retained from this measurement.

## 2026-09-01 — refresh prior-art provenance

The final comparison rotation rechecked every pinned upstream head. Smokescreen,
Lens, motosan-sandbox, ressrf, Raincoat, canister, eavs, and the previously
reviewed smaller projects remain at their recorded commits. Microsandbox moved
from `5b1c63d9` to `df4e1ead`; its three intervening commits cover immutable
runtime-image publication and dependency updates, including its guest TCP
stack, but no DNS-policy or egress-contract change. OpenShell moved from
`f7180c0f` to `4ef84234`; its ten commits cover CLI exit status, process
shutdown, release mechanics, policy provenance, and product resource cleanup,
with no proxy or egress-policy change.

The two table pins now point at the reviewed heads. No Sandbox Egress feature,
dependency, or behavior change is justified by these deltas.

## 2026-09-01 — reject impossible source identities

The final hardening review followed the host-cage contract back into the
attachment API. `PeerIdentity::SourceIp` accepted unspecified and multicast
addresses even though neither can appear as the peer of an accepted TCP
connection. Such a lease could never own traffic, but still received a sequence
and looked successfully installed to the supervisor.

Attachment now canonicalizes the address and rejects those impossible forms
with `AttachError::InvalidIdentity` before allocating a lease sequence or
sending a runtime command. A public matrix covers IPv4 and IPv6 unspecified,
multicast, and IPv4-mapped multicast forms, then requires the first concrete
source identity to receive lease ID 1. Ordinary unicast and loopback identities
remain unchanged. Twenty-five consecutive focused runs pass. The deterministic
test count rises from 185 to 186; whole-tree structural/cognitive complexity
moves from 771/2,285 to 775/2,297, mostly in the explicit five-shape proof.

The complete native factory passes all 186 deterministic tests, six
documentation examples, formatting, all-target lints, documentation,
dependency policy, package verification, benchmark smoke, and release build.
The pinned Rust 1.88 Linux factory also passes all 186 deterministic cases and
all eight serialized resource lanes. Its rootless conformance image is
`sha256:3e166c37dd39f39b837cfb95173e9c73d06f24114e4d9112b58e5e900c8f6bf9`
(40,957,451 bytes) and runs as UID/GID 65534.

## 2026-09-01 — reject an impossible destination grant

The next configuration-hardening pass found one remaining invalid value that
could be frozen into an immutable policy. Every network-facing configuration
already rejects destination port zero, and the CONNECT parser cannot accept
it, but `PolicyBuilder::allow_port(0)` previously succeeded as an unusable
grant. That made a host configuration look valid without permitting any real
connection.

Policy construction now rejects the value with the distinct
`PolicyError::InvalidPort` before a policy can be attached. This changes no
valid policy or wire behavior. A focused regression passed 25 consecutive
runs; formatting and all-target Clippy pass. The deterministic test count rises
from 186 to 187, and whole-tree structural/cognitive complexity moves from
775/2,297 to 776/2,300.

The complete native factory passes all 187 deterministic tests, six
documentation examples, documentation, dependency policy, package
verification, benchmark smoke, and release build. The pinned Rust 1.88 Linux
factory passes the same 187 cases and all eight serialized resource lanes. Its
rootless conformance image is
`sha256:fdac242f9973bb657373d8412e977fe4ea8fe19f29e1097624a284fba4d32296`
(40,958,603 bytes) and runs as UID/GID 65534.

## 2026-09-01 — recheck empty-lease lifecycle cost

The next performance rotation reran the original synchronous attach plus
certified-close benchmark three times on the exact 187-case tree. The three
95% confidence intervals were 1.360–1.375, 1.391–1.408, and 1.380–1.398
milliseconds. Criterion respectively reported noise-threshold movement, an
apparent small regression, and no detected change.

That 1.360–1.408 millisecond band overlaps the project's initial
1.357–1.368 millisecond baseline closely enough that scheduler variation is a
more credible explanation than a code effect. No tuning or production change
is justified by the measurement.

## 2026-09-01 — normalize repeated destination networks

The simplification pass applied the existing freeze-time normalization rule
consistently. Repeated CIDR grants and denials previously remained in the
immutable policy and could add redundant address checks for every matching
connection. `PolicyBuilder::build` now canonically sorts and deduplicates both
network vectors, just as it already does for hostname vectors.

The existing normalization proof now covers duplicate exact hosts, wildcard
hosts, host denials, network grants, and network denials. It passed 25
consecutive focused runs plus formatting and all-target Clippy. Public policy
semantics and the deterministic test count remain unchanged at 187;
whole-tree structural/cognitive complexity remains 776/2,300.

## 2026-09-01 — compare layered capacity and dial deadlines

The comparison rotation revisited Smokescreen's current request-concurrency
and CONNECT-tunnel limiters and nono's direct connector. Smokescreen keeps a
request-processing budget separate from a long-lived-tunnel budget. Nono gives
each resolved address its own fixed connect timeout. Both are reasonable at
their daemon boundaries, but neither is a stronger default for a run lease.

Sandbox Egress's global and per-lease connection permits deliberately span
headers, DNS, dialing, optional ClientHello inspection, and the live tunnel;
every admitted socket therefore stays bounded and owned until terminal. Its
separate dial semaphore is released after establishment, and sequential
addresses share one absolute handshake deadline with a bounded fair slice.
A second tunnel-only ceiling would add configuration and counters without
closing an unbounded phase, while per-address deadlines could multiply one
guest's total handshake time. Neither behavior is imported. The nono accept
loop citation is refreshed to its reviewed `d3c6f6b0` pin; production code and
the 187-case suite are unchanged.

## 2026-09-01 — reject limited-broadcast source identity

The hardening pass extended the impossible-identity boundary to IPv4 limited
broadcast. `255.255.255.255` cannot be the source of an accepted TCP
connection, but it previously consumed a lease sequence and appeared attached.
Attachment now rejects both its native and IPv4-mapped IPv6 spellings after the
same canonicalization used at socket acceptance.

The public identity matrix covers both new forms and still requires the first
valid source to receive lease ID 1, proving rejection happens before sequence
allocation or runtime mutation. Twenty-five consecutive focused runs pass with
formatting and all-target Clippy. The deterministic test count remains 187;
whole-tree structural/cognitive complexity moves from 776/2,300 to 778/2,311.
An attempted single-arm predicate measured worse cognitive complexity than the
explicit address-family match and was discarded.

## 2026-09-01 — reject a destination-network index

The next performance rotation temporarily added 1,024 distinct nonmatching
IPv4 `/24` denials to the allowed loopback CONNECT benchmark. A five-second
same-process run measured direct TCP at 42.80 microseconds and proxied setup at
128.32 microseconds, or about 85.52 microseconds of control-normalized proxy
work. After removing the temporary rules, direct TCP measured 41.29
microseconds and proxied setup 120.94 microseconds, or about 79.65 microseconds
of normalized proxy work.

The linear scan therefore cost roughly 5.9 microseconds at an intentionally
large 1,024-rule boundary. That is measurable, but does not justify a radix
tree, another dependency, or two immutable policy representations for the
expected small per-run rule sets. Freeze-time duplicate removal stays; a
network index and the temporary benchmark code are discarded. Production code,
the 187-case suite, and complexity are unchanged.

## 2026-09-01 — certify the final 187-case Linux factory

The pinned Rust 1.88 Linux factory passes all 187 deterministic cases, six
documentation examples, formatting, all-target lints, documentation, package
verification, benchmark smoke, and release build on the final implementation.
All eight serialized release resource lanes pass, including partial headers,
partial ClientHellos, partial upstream responses, simultaneous
backpressure, idle expiry, management churn, identity churn, and 2,000-cycle
terminal connection churn.

The stripped conformance image is
`sha256:d7739b3ddf7faac5406df48f9edb14f5fa87b580e3387183520dee7cf89f71da`
(40,980,588 bytes). It contains the assembled test executables rather than
Cargo or source and runs successfully as UID/GID 65534.

## 2026-09-01 — compare production rate controls

The next comparison rotation pinned current G3 `79e99f76` and Rama `cde3aa85`.
G3 checks a per-user request rate before acquiring its concurrent-request
permit. Rama documents the same composition explicitly: a shared token bucket
outside a concurrency guard provides cheap rejection without occupying an
in-flight slot. Smokescreen independently carries separate request-rate and
request-concurrency controls.

This exposes a real boundary in Sandbox Egress. Its global and per-lease
connection permits cap simultaneous work, but a guest can still generate rapid
terminal or denied connection churn below that ceiling. A future rate control
must be process-wide and per-lease, fail fast before task creation, attribute
the denial to the currently observed lease, and reset per-lease state on safe
identity reuse. That contract is added to the hardening backlog; no late token
bucket or public API is improvised in this pass.

The exact implementation also passes all 187 deterministic cases and both
benchmark smoke targets under optimized code generation. The package audit
lists 69 intended source, test, benchmark, documentation, and factory files;
there is no configured Git remote. Production behavior and complexity are
unchanged.

## 2026-09-01 — reproduce the final connection curve

The final performance rotation ran the six-point, three-repeat fixed-work
sweep on revision `0e634ef`. Median rates for concurrency 1, 8, 32, 64, 128,
and 256 were 6,792, 19,919, 21,295, 21,949, 20,524, and 17,742 connections per
second. Median p99 setup latencies were 165, 431, 1,231, 2,315, 5,932, and
68,420 microseconds.

The absolute numbers are somewhat better than the previous local sweep without
a causal hot-path change, so no speedup is claimed. The repeated shape is the
useful evidence: throughput saturates around 32–64 callers and 256 callers
materially worsen tail latency. No runtime tuning or production code is
changed.

## 2026-09-01 — preserve the rotating hardening cadence

The final process simplification makes the requested iteration loop durable in
`AGENTS.md`: performance evidence, simplification, prior-art comparison, then
feature or security hardening, repeated without forcing unsupported changes.
It also directs future contributors toward deterministic conformance matrices
and controlled inputs rather than randomized input-generation tooling.

The README diagram now accurately describes one shared `Proxy`, which may be
embedded in the supervisor process, rather than implying that the library
always owns a separate process. A nearby capability sentence is made
grammatically unambiguous. Runtime behavior, public API, tests, and complexity
are unchanged.

## 2026-09-01 — repeat the timing-sensitive lifecycle boundary

The next hardening pass repeated three timing-sensitive guarantees 25 times
each on the final implementation. A queued socket accepted under management
pressure never inherited a replacement policy; an arrival during revocation
always restarted the identity quiet period; and certified close always
terminated simultaneous upload and download backpressure.

All 75 focused runs passed. The first two exercise the listener-owner ordering
and deliberate quiet-period waits, while the third requires both hostile
writers to observe terminal sockets. No flake, ownership gap, or accounting
mismatch appeared, so production code and the 187-case suite are unchanged.

## 2026-09-01 — retain the two-worker owned runtime

A controlled runtime-sizing experiment temporarily changed the owned Tokio
runtime from its committed two workers to one and four. Each shape ran three
10,000-CONNECT samples at 64 clients and 16 loopback destinations. Median
throughput was 20,297, 19,623, and 17,972 connections per second for one, two,
and four workers respectively.

The one-worker result is only 3.4% above the contemporaneous two-worker median
and individual samples overlapped substantially; it is not evidence for a
speedup. Four workers were 8.4% below the two-worker median and add two steady
threads. Both candidates were removed. The committed two-worker runtime and
production behavior are unchanged.

## 2026-09-01 — remove completed work from the active map

The final documentation simplification reconciled the hardening backlog with
the conformance record. Accepted old sockets, queued sockets under command
pressure, and replacement-policy isolation are implemented and tested, so the
active lifecycle item now names only retransmitted or delayed SYNs arriving
after listener-level certification. That residual problem explicitly belongs
to the future host-cage and conntrack harness because the listener cannot
authenticate a packet's run generation after source-address reuse.

The roadmap now marks the completed repository spine and phase-revocation
milestones, and old nono commit references are labeled historical instead of
appearing to conflict with the reviewed table pin. This changes no security
claim or production behavior; it makes the next work selection more accurate.

## 2026-09-01 — compare ownership boundaries, not feature counts

The comparison rotation condensed the closest implementations around four
questions: what selects policy, who owns spawned handlers, what scope remains
alive after one run ends, and what a successful cleanup operation proves.
Smokescreen's process tracker, Lens's process-lifetime handlers, nono's accept
loop, and Motosan's per-run listener are coherent at their respective daemon or
sandbox lifetimes. None is a defective version of a lease.

The distinction matters only when one listener remains alive and a source
identity is reassigned. The prior-art document now puts that boundary beside
Sandbox Egress's per-run tracker, cancellation, permits, final counters, and
ownership-retaining failure. No implementation change follows from the table;
it prevents feature-count comparison from obscuring the lifecycle requirement.

## 2026-09-01 — make expired close deadlines revoke first

The hardening rotation checked the zero-wait edge of the public lifecycle API.
`Lease::close` already calls `begin_close` before it sends a management command
or computes the remaining wait, so an already-expired deadline revokes
admission and signals cancellation synchronously before returning the owning
`DeadlineExceeded` error. The suite did not state that ordering explicitly.

A new real-listener case supplies a deadline one second in the past, recovers
the lease, proves replacement attachment remains blocked, and opens another
socket. That socket is terminal, is charged as one unadmitted denial to the old
lease, and never increments accepted or active work. A later retry certifies
the exact snapshot. The focused case passed more than 25 repeated runs; the
deterministic suite grows from 187 to 188 cases without production changes.

## 2026-09-01 — retain the explicit coalesced-hello copy

A performance simplification trial reused the owned CONNECT header vector for
TLS inspection by draining the parsed header prefix, instead of copying the
bounded coalesced `ClientHello` suffix into a new vector. All 20 TLS-focused
cases passed, including exact wire retention, fragmentation, ECH, malformed
input, deadline, and close behavior.

Three one-second Criterion samples before and after used the end-to-end visible
SNI path. Baseline point estimates were 143.50, 146.88, and 150.08 microseconds;
candidate estimates were 142.70, 149.01, and 153.70 microseconds. The medians
move from 146.88 to 149.01 microseconds, and Criterion classified every pair as
no change or within its noise threshold. Saving one small allocation did not
improve the measured connection path, while prefix draining adds mutation and
partial-move mechanics. The candidate was removed and production code is
unchanged.

## 2026-09-01 — certify the 188-case Linux factory

The exact post-deadline-hardening tree passes the complete pinned Rust 1.88
factory: 188 deterministic cases, six doctests, all-target lint and benchmark
smoke, documentation, package verification, and all eight serialized release
resource lanes. The stripped runner repeats all 188 cases as UID/GID 65534.

Every resource lane returned to four descriptors and two threads at process
finish. Representative peaks were 521 descriptors for 128 silent tunnels, 69
threads for 64 simultaneous host callers, 14,524 KiB RSS for 64 partial
60,020-byte ClientHellos, and 521 descriptors for 128 partial upstream proxy
responses. The final image is
`sha256:48076b3ea27c62cadc99443f681354f7983a3bb6a01c2382cb07271e2636f015`
at 40,983,005 bytes. This is reproducibility evidence, not a production image
or a claim about resources outside the measured factory matrix.

## 2026-09-01 — make every factory consume the lockfile

A release-factory consistency audit found that the container used
`--locked`, while ordinary checks, conformance, benchmarks, load, throughput,
resources, Cargo aliases, and most CI lanes allowed dependency resolution to
rewrite `Cargo.lock`. That could make two clean contributors test different
transitive graphs even though the repository intentionally commits its lock.

Every Cargo invocation that resolves or builds repository dependencies now
uses `--locked`; formatting and dependency-policy commands do not resolve the
crate and remain unchanged. A negative search finds no unlocked check, Clippy,
test, benchmark, documentation, package, run, or build command in the public
scripts, README, Dockerfile, or CI workflow. The updated ordinary factory
passes all 188 deterministic tests, six doctests, benchmark smoke,
documentation, package verification, and dependency policy without modifying
the lockfile.

## 2026-09-01 — preserve room in the pre-release public API

The API simplification review found four externally exhaustive types at likely
extension points. `Usage` and `DiagnosticEvent` have public readable fields,
while `TlsAuthority` and `EchPolicy` expose the deliberately small current mode
set. Without an extension marker, adding a counter, bounded diagnostic field,
or future protocol mode would unnecessarily break downstream struct literals
or exhaustive matches.

All four are now non-exhaustive, consistent with `PeerIdentity` and the public
error enums. Existing fields, defaults, access, equality, and policy variant
construction remain available. The change is made before publication, when it
does not disrupt a released API. Formatting, all-target compilation, six
doctests, and warning-denied rustdoc pass on the locked dependency graph;
runtime behavior and measured complexity are unchanged.

## 2026-09-01 — distinguish declared portability from checked portability

The portability audit ran a locked all-target, all-feature check for
`x86_64-unknown-linux-gnu` from the pinned Rust 1.97.1 aarch64 macOS toolchain.
It passes, complementing the executed aarch64 Linux Rust 1.88 container and
native aarch64 macOS factories. Cargo metadata also confirms edition 2024 and
the declared Rust 1.88 minimum.

The repository declares Ubuntu, macOS, and Windows CI jobs, but a workflow file
is not execution evidence for Windows in this local repository. Windows
therefore remains an explicit portability backlog item until that job is run
in a published CI environment or an equivalent target is checked directly.
No conditional code, dependency, or public claim changes in this pass.

## 2026-09-01 — refresh the complexity comparison point

The durable complexity document previously stopped at the early 18-file
address-floor checkpoint. SCC 4.0.0 now records the exact 188-case tree at 28
Rust files, 13,481 lines, 12,244 code lines, and 780/2,317 aggregate
structural/cognitive estimates. `proxy.rs`, including its large `cfg(test)`
body, is 267/899; `policy.rs` is 58/185.

The checkpoint explicitly avoids treating the aggregate as shipped-binary
complexity: integration tests, resource lanes, benchmarks, and fixed TLS
fixtures are included. Its purpose is to give the next simplification pass a
stable same-tool comparison point. No threshold or implementation change is
introduced.

## 2026-09-01 — compare a transparent per-VM authority boundary

Torkbot's Sandbox was reviewed at commit `3dc0dd5c`. Its host-owned transparent
network service exposes default-deny per-flow policy over TCP, UDP, DNS, and
HTTP-family traffic. The HTTP path binds its authority decision to original
destination state, guest-scoped accepted DNS answers, and TLS metadata rather
than trusting the guest's `Host` header. That is a concrete example of the
stronger application-authority promise available to an implementation that
owns interception and TLS termination.

The lifecycle boundary is deliberately different: one `HostNetwork` belongs
to one VM, and `Drop` sets a shutdown flag and joins its network worker. It does
not need to detach and later reuse one source identity beneath a shared live
listener. The comparison therefore changes no production code. It reinforces
the current honest CONNECT-plus-visible-SNI promise and records transparent L7
enforcement as a wider host-integration design, not a feature to imply through
SNI inspection.

## 2026-09-01 — recheck the current connection curve

The exact post-comparison tree repeated the release load harness with 10,000
local CONNECT tunnels, concurrency 64, and 16 upstream destinations. Three
runs produced 18,929.5, 19,812.5, and 20,904.4 connections per second; the
median is 19,812.5. Median per-connection latency in those runs was 1,837,
1,868, and 1,804 microseconds, respectively.

The earlier two-worker runtime experiment measured a 19,623 connections per
second median under the same workload shape. The new median is 1.0 percent
higher and remains inside the observed run-to-run spread, so there is no
performance regression or optimization claim. Production configuration stays
unchanged.

## 2026-09-01 — certify the final local candidate

The exact 188-case tree passes the complete locked local factory: formatting,
all-target and all-feature compilation, pedantic lint, 188 deterministic tests,
benchmark smoke, six doctests, warning-denied documentation, a verified
69-file package, and dependency advisory, license, ban, and source policy.

Four scheduler-sensitive boundaries then passed 10 strict repetitions each:
a queued old socket cannot inherit a replacement policy, a revoking arrival
restarts the quiet interval, an already-expired close deadline revokes before
returning the owning lease, and close interrupts simultaneous bidirectional
backpressure. The repetition shell stopped on the first failure; all 40
complete cases passed.

The same 188 deterministic cases and both benchmark binaries also pass under
the locked optimized all-target build. This checks the release code-generation
path separately from the ordinary debug factory and introduces no profile- or
platform-specific exception.

Repository checks find no working-tree changes, remotes, placeholder markers,
prohibited Rust blocks, or prohibited test-harness references. Every shell
entry point parses and is executable, the package list remains 69 files, and
the commit graph passes full object verification. One unreachable blob from a
discarded experiment remains in the local object database; it is not reachable
from the committed history or package and has no release effect.

## 2026-09-01 — retain explicit Rustls TLS 1.2 support

A dependency simplification trial removed Rustls's explicit `tls12` feature
while retaining its `std` parser support. All 20 TLS-focused cases passed,
including independent OpenSSL, Rustls, and SecureTransport fixtures, record
fragmentation, malformed input, ECH, deadlines, and certified cancellation.

The locked optimized arm64 macOS wrapper did not shrink: it changed from
2,867,840 to 2,867,920 bytes, an 80-byte increase inside ordinary linker
variation. With no artifact benefit, removing the feature would only make the
intended deployed TLS compatibility less explicit. The candidate was reverted;
the normal dependency graph remains free of OpenSSL, native-tls, ring, AWS-LC,
and an HTTP client/server framework.

## 2026-09-01 — repeat every resource lane on the final tree

All eight locked optimized resource lanes pass on arm64 macOS after the final
dependency trial. The one-process sequence covers 8,000 attach/close identity
cycles, 2,000 iterations of each terminal connection shape, 64 bidirectionally
backpressured tunnels, 128 partial headers, 64 partial 60,020-byte
ClientHellos, 128 partial upstream-proxy responses, and 128 simultaneous idle
expiries.

Every lane finishes at nine process descriptors and two threads. Peak RSS in
the sequential test process reaches 23,136 KiB and finishes at 22,864 KiB;
this records allocator retention rather than claiming that resident memory
returns to its pre-allocation value. The existing isolated Linux lanes remain
the cross-platform release comparison, while this repetition proves the exact
final local tree releases live sockets and workers.

## 2026-09-01 — rebuild the final Linux certificate

The pinned Rust 1.88 Debian factory passes the full current tree, including all
188 deterministic tests, six doctests, benchmark smoke, documentation, package
verification, and the eight release resource lanes. The stripped runner then
repeats all 188 cases successfully as UID/GID 65534.

The resulting conformance image is
`sha256:85c1ecf02231e14a60d258ce0fecf6534fd5b9ca81797aa606ab4e2e2be79088`
at 40,982,993 bytes. It contains the checked executables rather than the
compiler, source, or build cache. The container package list has 68 files
because the build context intentionally lacks local Git metadata; the native
repository package remains the verified 69-file artifact.

## 2026-09-01 — sustain five million connections

One optimized proxy and lease completed 5,000,000 local CONNECT tunnels at
concurrency 64 across 16 upstream destinations. The fixed workload ran for
288.49 seconds at 17,424.2 connections per second, with p50/p95/p99 setup
latencies of 1,922/2,433/3,134 microseconds.

The harness checks every response, joins every worker and upstream, certifies
the lease's final counters, and shuts down the shared proxy only after the
whole workload. It passed without throughput collapse, capacity leakage, or
ownership failure. This is a sustained local loopback result, not an external
network or multi-host capacity claim.

## 2026-09-02 — turn Firecracker prior art into a host contract

Firecracker `4c998054`, n8n sandbox service `e7a7e728`, CubeSandbox `30e002cb`,
OpenSandbox `1eb8fffa`, and mvm `4ebd13d5` were compared at the host-network
ownership boundary. The common operational lesson is that the proxy lease is
only one part of a run generation: namespace/TAP/veth, firewall, NAT/conntrack,
VMM, cgroup, shaping, and source address need one external owner and explicit
restart reconciliation. Firecracker's snapshot contract also makes live
connection survival the wrong guarantee; a restore needs a fresh host path and
lease, while guest connections reconnect.

The resulting `docs/firecracker-integration.md` keeps this record outside the
public `Proxy / Policy / Lease` API. It specifies deny-first construction and
active readiness probes, fence-before-close teardown, no identity reuse before
kernel cleanup, fresh snapshot generations, and separate proxy-churn versus VM
packet/bandwidth controls. It explicitly lists the unproved TAP/KVM, IPv6,
UDP/DNS, inherited-descriptor, and NAT-port cases.

## 2026-09-02 — bound attributed connection churn before task creation

Deterministic integer token buckets now optionally apply process-wide and per
lease after source-IP attribution but before header parsing, concurrency
reservation, or task creation. Unit cases pin full-burst, fractional-refill,
capacity, overflow, and invalid-input behavior. Real listener cases pin exact
one-accepted/one-denied accounting and the stable `lease-rate` and
`global-rate` diagnostics. Close and reattach gives a new lease bucket but
cannot reset the proxy-lifetime global bucket.

The first implementation locked the lease lifecycle state even when both
limits were disabled. It lost all three initial 30,000-connection A/B pairs and
was removed. The retained default path branches around time and lock work; the
listener owns the global bucket directly. Eight A/B comparisons overlap the
detached baseline, so no default-path performance change is claimed. Enabling
both buckets measured an 18,355 versus 18,891 connections/second median and a
1,925 versus 1,912 microsecond p50 median. The optional cost is kept because it
closes a named terminal-churn gap; the rejected shape and measurements remain
in `docs/performance.md`.

## 2026-09-02 — certify the host fence and policy phase transition

The ordinary suite now proves an immutable phase change by allowing and
denying complementary destinations, certifying the old lease, reattaching the
same source identity, and repeating under the new policy with exact per-lease
counters. The privileged Linux lane adds separate host and guest namespaces, a
veth path, deny-first nftables rules, a blocked direct decoy, a successful
CONNECT tunnel, fence-before-close, zero host-side proxy sockets, same-address
reuse only on a fresh path, and named orphan cleanup. Wrapping that lane showed
conntrack 0/0/0 in its non-NAT topology and allocated files 704/736/704; the
zero conntrack result is a scope limit, not NAT recovery evidence.

A packaging review caught that Cargo omits a nested workspace package from the
root crate archive. The initial unpublished tool was therefore replaced by the
single-package `examples/linux_host_proxy.rs` fixture. The verified archive now
contains the Dockerfile, script, and source needed to reproduce the lane. This
removes a workspace and avoids publishing instructions for an absent fixture.

The exact privileged Alpine build passes the proxy-only path, fenced close,
identity reuse, and orphan-cleanup certificate. Docker initially exhausted its
internal storage while compiling; inspection showed four old stopped
containers were failed Sandbox Egress factory stages, so only those and the
current failed stage were removed. No unrelated container or image was
deleted.

## 2026-09-02 — certify the complete host-lifecycle slice

The final candidate before this prose-only evidence entry passes the native
factory and standalone hostile suite:
199 deterministic cases, six doctests, all-target/all-feature compilation and
Clippy, benchmark smoke, warning-denied documentation, the Linux example, a
verified 75-file package, and dependency policy. Repeated global-bucket
identity-reuse runs completed without a reset escape.

The pinned Rust 1.88 Linux factory repeats the same code paths, including the
example target and all eight serialized release resource lanes. Every resource
lane finishes at four descriptors and two threads. Representative peaks are
521 descriptors for 128 idle or partial-upstream tunnels, 69 threads for 64
simultaneous host callers, and 15,164 KiB RSS for 64 partial 60,020-byte
ClientHellos. The stripped unprivileged runner repeats all 199 deterministic
cases successfully. Its image is
`sha256:73ae963bed6ee87bc6c3616fe5a0876a17c2366edef9edb4fb7ae47cb44beb5f`
at 41,010,366 bytes.

The separately privileged Rust 1.88 Alpine certificate builds the packaged
example and passes again at
`sha256:761f0a0d051c0ee63d0d53f50e7eba4519b625ffc9cea27df7a5be832e76285c`.
Its measured lane returns allocated files from 736 to the 704 baseline and
leaves zero root-namespace conntrack, TCP, TIME_WAIT, or UDP entries. The
privileged image is a test environment, not a production deployment artifact.

## 2026-09-02 — keep Firecracker as an integration, not the product

The next review corrected an over-specific follow-up from the host-lifecycle
research. Sandbox Egress is a composable outbound-network policy library. Its
reusable boundary is a host-enforced single egress path, host-observed source
identity, immutable policy, and lease-owned shutdown. Firecracker is an
important consumer of that contract, but booting a VMM or proving a particular
snapshot implementation is not a core crate release gate.

The integration guide is now `docs/host-integration.md`. It states the generic
generation, deny-first readiness, fence-before-close, restoration, shaping,
and reconciliation rules, then maps Firecracker as one example. The active
backlog no longer asks this crate to grow a Firecracker/KVM harness. The
privileged namespace certificate remains because it tests the reusable host
ownership transition without adding sandbox or VMM machinery to the library.

The complexity workflow was already present. To make it harder to simplify by
deleting evidence, its documentation now distinguishes the whole-tree report
from the narrower crate-tree report, which still honestly includes colocated
unit tests and test seams. The current whole tree is 29 Rust files, 14,079
lines, 12,781 code lines, and 823/2,419 structural/cognitive points; `src`
alone is 18 files, 8,497 lines, 7,575 code lines, and 544/1,687 points.

One new lifecycle case starts revocation with an ownership-retaining expired
close, sends another old-run connection, certifies that lease, and attaches a
replacement. The replacement's first connection must still receive the sole
process-wide rate burst. This proves revoking traffic is rejected and charged
to the old lease before the global bucket is consulted. The case passed eleven
complete focused runs.

A performance candidate reused the already-captured accept timestamp for the
optional rate buckets, avoiding a second monotonic clock read. Five baseline
enabled-rate runs measured 17,182--21,482 connections/second with a 20,033
median; five candidate runs measured 18,686--21,647 with a 19,558 median. The
ranges overlap and the candidate median is 2.4% lower. The extra parameter was
removed and no optimization claim is retained.

## 2026-09-02 — keep denial delivery outside the backpressure boundary

A correctness review found that the required CONNECT-success response obeyed
the absolute handshake deadline, while pre-tunnel denial responses used an
unbounded asynchronous write. The denial body is diagnostic rather than part
of enforcement, so the retained design records the reason, makes at most one
nonblocking response write if the original deadline is still live, and then
shuts down the socket. Invalid deadline construction and tunnel-phase failures
close without attempting another response. A focused unit seam proves both the
one-shot live case and that expired work never invokes the write.

The first candidate reused the CONNECT-success timeout wrapper for denial
writes. It was correct but moved `connect_denied_hostname` from a detached
`c92ba1b` point estimate of 75.581 microseconds to 82.090 microseconds, about
9% slower, and was discarded. The retained nonblocking design measured 75.346
microseconds against that first 75.581 baseline. A second alternating pair was
72.211 versus 72.322 microseconds. These ranges do not support a performance
change claim; they do show that the enforcement fix need not pay the rejected
timer cost.

Passing `Denial` as one value also keeps the connection dispatcher below the
warning-denied function-length limit. Whole-tree Rust source moves from 14,079
to 14,129 lines and from 823/2,419 to 824/2,421 structural/cognitive points;
the `src` tree moves from 8,497 to 8,543 lines and from 544/1,687 to 545/1,689.
The measured increase is one production branch plus its focused proof. The
native factory and hostile suite pass all 201 deterministic cases and six
doctests, along with Clippy, docs, benchmarks, package verification, and
dependency policy.

## 2026-09-02 — reject a denial-formatting simplification

A follow-up performance cycle tried to construct the denial header and body in
one `format!` allocation instead of building the short body first. The focused
hostname-denial case and Clippy passed, but the same-target Criterion median
moved from 67.401 to 80.976 microseconds, with a reported 16.5--28.2% slowdown.
The source change was discarded. Response bytes, production complexity, and
the data path remain unchanged; this negative result is retained so a future
cleanup does not assume fewer visible allocations must be faster here.

## 2026-09-02 — skip all expired diagnostic construction, then inline it

The first bounded-denial implementation constructed its two small response
strings before checking whether the handshake deadline had already expired.
The retained version moves both allocation and the single nonblocking write
behind that check; denial accounting still happens first and shutdown still
happens afterward. The already-committed real-socket deadline cases prove zero
wire bytes after expiry, while ordinary listener cases prove live diagnostic
responses, so a synthetic closure-only unit case was removed.

An initial helper for the synchronous check raised whole-tree SCC complexity
from 824/2,421 to 827/2,427 despite reducing lines. Inlining its single branch
restored 824/2,421 and reduced whole-tree Rust source from 14,129 to 14,093
lines; `src` falls from 8,543 to 8,507 lines with its 545/1,689 complexity
unchanged. Alternating baseline/candidate denial medians ranged
67.029--76.222 and 74.571--77.222 microseconds respectively. The noisy ranges
overlap, so no throughput change is claimed; the retained gain is less expired
work and less source without additional measured decision complexity.

A subsequent tunnel-path trial returned early on zero-byte reads to avoid a
saturating atomic add of zero at EOF. `connect_allowed_loopback` moved from a
118.15 to 110.37 microsecond median, but its confidence interval was
−7.1--+7.2% and Criterion found no change. The candidate also raised
`src/proxy.rs` complexity from 275/923 to 276/926. It was discarded: one
unmeasured EOF optimization did not justify another branch and early return.

## 2026-09-02 — repeat every resource lane after denial hardening

The current optimized arm64 macOS tree passes all eight serialized resource
lanes after the response-lifetime changes. The one-process run covers 8,000
attach/close generations; 2,000 iterations each of completed, transfer-limited,
reset, and pre-DNS denied connections; 64 bidirectionally backpressured
tunnels; 128 partial headers; 64 partial 60,020-byte ClientHellos; 128 partial
upstream responses; and 128 simultaneous idle expiries.

Every lane finishes at nine descriptors and two threads. Observed peaks were
526 descriptors, 69 threads during 64 concurrent management callers, and
23,120 KiB RSS. The 8,000-generation lane held 22,880 KiB after every batch
and finished at 22,816 KiB; the backpressure lane finished at 22,832 KiB.
These are process-level allocator-retention observations, not a claim that RSS
returns to its initial value, while the live socket and worker counts do
return to their final baselines.

## 2026-09-02 — refresh the established-tunnel throughput checkpoint

Eight concurrent loopback tunnels transferred 256 MiB each in both directions
on the optimized arm64 macOS build. Without optional idle tracking, exact 2 GiB
transfers measured 3,229.9 MiB/s upload and 3,083.3 MiB/s download. With a
one-second idle policy they measured 3,257.5 and 3,346.4 MiB/s. The enabled
runs being faster shows ordinary local variation rather than a control-path
speedup; no comparative claim is made. The durable performance document now
records this post-hardening checkpoint and exact byte counters.

## 2026-09-02 — distinguish guest addresses from proxy identity

PandaStack `1147f535` was reviewed as a current pooled-snapshot comparison.
Its NATID path keeps one baked guest IP/MAC/gateway inside each isolated
namespace, then SNATs egress to the slot's unique veth address before shared
root conntrack. That makes an important integration rule explicit:
`PeerIdentity::SourceIp` is the address the Sandbox Egress listener actually
observes, not an address copied from guest configuration or snapshot metadata.

The same source documents three repaired allocator generations involving slot
leaks and concurrent double-free, plus stale namespaces that could answer ARP
after slot reuse. The host guide now calls for one authoritative slot owner,
atomic transfer from a uniquely owned prebuilt sentinel, destroy-first and
free-last release, and reconciliation before pool refill. These rules stay in
the supervisor contract; the public library remains `Proxy / Policy / Lease`
with no VMM, network-pool, or durable-store API.

The public `SourceIp` rustdoc, README example, and security invariant now use
the same listener-observed wording. This is documentation of the existing
socket lookup behavior, not a public API or runtime change.

The comparison also exposed an adjacent integration ambiguity. Selecting a
lease by source IP does not prevent the root-side proxy from reaching another
sandbox slot when a policy deliberately grants broad private address space.
The host and security guides now say to retain an independent east-west
firewall and layer higher-priority tenant/host subnet denials over narrow
private-service grants. No destination rule was silently added to the crate;
the supervisor remains responsible for naming its own tenant address pools.

## 2026-09-02 — refresh the connection-scale checkpoint

The optimized current tree completed 100,000 local CONNECT tunnels through one
proxy and lease at concurrency 64 across 16 destinations. It ran for 5,450
milliseconds at 18,348.2 connections/second, with p50/p95/p99 setup latencies
of 1,883/2,338/3,132 microseconds. Every response was checked and final lease
accounting was certified after all workers and upstreams joined. This is a
post-hardening regression point, not a comparison with the longer sustained
five-million-connection run.

## 2026-09-02 — freeze ports into contiguous immutable storage

The policy builder previously retained a `BTreeSet<u16>` after freeze even
though ports never mutate at runtime. It now appends during construction, then
sorts and deduplicates one `Vec<u16>` alongside the other immutable policy
rules. Runtime membership uses binary search. A duplicate-port assertion pins
the unchanged builder semantics.

Two detached alternating allowed-loopback pairs measured baseline medians of
108.01 and 113.08 microseconds against candidate medians of 109.99 and 114.97
microseconds. The intervals overlap, so no end-to-end latency change is
claimed; socket and scheduling work dominate one port lookup. `src/policy.rs`
keeps exactly 59/188 structural/cognitive points and adds four test-bearing
lines. The retained gains are contiguous ownership, no per-port tree node, and
a simpler immutable representation.

## 2026-09-02 — certify the post-hardening Linux artifact

A cold pinned Rust 1.88 Linux build passed the complete offline factory after
the denial-lifetime and immutable-port changes. It ran 200 deterministic cases,
six documentation examples, formatting, warning-denied Clippy and rustdoc,
benchmark smoke, package assembly and verification, and all eight serialized
release resource lanes. Dependency policy remained in its intentionally
separate native/CI `cargo-deny` 0.20.2 lane; the container printed the expected
skip instead of downloading or compiling that maintenance tool.

Every Linux resource lane returned to four descriptors and two threads after
its proxy process ended. The largest descriptor peak was 521 during the
128-connection idle-expiry and partial-upstream-response lanes. The deliberate
64-caller synchronous-management lane peaked at 69 threads and returned to five
while its proxy remained live. The partial 60,020-byte ClientHello lane had the
largest observed RSS at 15,212 KiB and returned from 265 descriptors to eight
while the proxy remained live.

The stripped final image then repeated all 200 deterministic cases as
UID/GID 65534 without Cargo, a compiler, source, or build cache. Its local image
ID is `sha256:b0c73f7e4aad5755fe0687868d6d59793efd022221d76e831dd88c2b4e52f61a`
and Docker reports 41,037,511 uncompressed bytes. Those identifiers certify
this local build only; the durable claim is the reproducible command and
passing rootless conformance behavior.

## 2026-09-02 — reject an inconclusive header-scan dependency

A performance cycle replaced the standard-library four-byte CONNECT terminator
scan with `memchr` 2.8, already present in the development graph but new to the
production graph. The first same-target Criterion comparison moved the hostile
1 MiB near-terminator median from 637.54 to 626.56 microseconds. Later warmed
runs did not separate consistently: the candidate ranged from 653.94 to
1,846.5 microseconds while the detached baseline measured 681.98 and 693.98
microseconds under the same changing machine load.

The candidate also added 928 bytes to the stripped release executable. It was
discarded because the end-to-end evidence does not justify another production
dependency or artifact growth. The existing incremental scan, parser behavior,
dependency graph, complexity, and deployment binary remain unchanged.

## 2026-09-02 — use coverage to remove a dead parser decision

The first instrumented deterministic suite established a 96.56% production
line and 96.27% region baseline. Review followed uncovered security-relevant
spans rather than the aggregate score. Focused cases now prove both clamps on
the public ClientHello ceiling, DNS deadlines longer than the absolute
handshake are rejected, incomplete CONNECT and non-HTTP/1 syntax fail closed,
and numeric CONNECT authority cannot be paired with a hostname Host field.

At the runtime boundary, a successful resolver result containing no addresses
now has the same real-listener, zero-dial, exact-accounting proof as resolver
I/O failure. A controlled header transport reset pins `header-read-failed`, and
the buffered-upload seam proves an immediate upstream write error remains
distinct while attempted bytes stay accounted. These are deterministic fixed
cases; no randomized input generation was added.

The HTTP/2 syntax case exposed an unreachable production branch. `httparse`
rejects that version before it can return a complete request, so the later
`unsupported-http-version` check could never execute. Removing it preserves the
existing fail-closed `malformed-header` result and lowers `src/connect.rs` from
44/108 to 43/106 structural/cognitive points. After replacing a test loop with
a straight fixture helper, the whole Rust tree is 824/2,422 versus 824/2,421;
all structural growth is avoided and the one cognitive point is test proof.
The instrumented suite rises to 96.72% line and 96.39% region coverage, with
`config.rs` and `connect.rs` at 100% line coverage.

## 2026-09-02 — make coverage review reproducible but optional

The useful part of the coverage pass was the uncovered-code inventory, not the
percentage. `scripts/measure-coverage.sh` now pins `cargo-llvm-cov` 0.9.0 and
runs the deterministic workspace suite with locked dependencies. It remains
outside the default factory and CI gates: instrumentation slows the ordinary
loop, and a percentage threshold would reward low-value test growth or removal
of legitimate defensive branches. Contributor guidance instead asks reviewers
to inspect uncovered lifecycle, policy, parser, and cancellation paths. The
first run of the committed command completed in 13 seconds and reported 96.74%
line and 96.40% region coverage.

## 2026-09-02 — unify immutable connection runtime ownership

Every accepted connection previously cloned four independent `Arc` owners for
the resolver, connector, DNS/dial semaphores, and proxy configuration. Those
values have exactly the same process lifetime, so `ConnectionShared` now owns
them behind one `Arc`. Dispatch performs one reference-count increment instead
of four, and proxy startup makes one shared-state allocation instead of four.
The global admission semaphore remains separately owned because its permit
must move into each task.

Alternating control-normalized Criterion pairs did not separate: comparable
baseline proxy overhead was 75.83 and 77.42 microseconds versus 77.46 for the
candidate. No latency improvement is claimed. Production complexity remains
545/1,690 structural/cognitive points, while the stripped local release binary
fell from 2,885,136 to 2,867,920 bytes. The change is retained as a lifetime and
ownership simplification with fewer atomic operations, not a benchmark win.

## 2026-09-02 — audit the maintained DNS decoder boundary

Hickory 0.26.1 and current main `8c7b8780` were compared at the wire decoder.
Both reserve query and record vectors directly from untrusted 16-bit section
counts before parsing. Sandbox Egress's address-cardinality check necessarily
runs later, and Hickory exposes query EDNS sizing but no response-byte or decode
allocation ceiling. The existing inflated-count wire case proves fail-closed
behavior and zero connector calls, not byte-aware decoding.

Five fresh debug test processes handling the fixed 65,535-answer/no-record
reply completed in 30--50 milliseconds and reported 12,337,152--12,386,304
bytes maximum RSS. An adjacent ordinary malformed reply reported 12,288,000
bytes. This does not demonstrate an immediate RSS problem; it precisely scopes
the residual. Lookup concurrency is bounded at 32 by default, cache storage is
disabled by default, and the returned-address and dial-attempt vectors remain
bounded.

No production wrapper was retained. Interposing before Hickory would require
stateful UDP transaction handling plus length-prefixed TCP fallback, while
vendoring would fork a maintained security parser. The backlog now prefers an
upstream decoder capacity limit or supported response ceiling and preserves
the fixed-wire case as a regression sentinel.

## 2026-09-02 — canonicalize approved transport destinations

Destination policy and proxy self-address checks already understood
IPv4-mapped IPv6, but DNS deduplication and the connector still used the
original `SocketAddr`. A resolver answer containing both `127.0.0.1` and
`::ffff:127.0.0.1` therefore produced two fallback attempts to the same
effective endpoint when policy explicitly granted both forms. A new test first
failed with both connector calls.

The retained path evaluates policy against each original address, then
canonicalizes the approved socket destination before deduplication and dialing.
The test now observes one connector call for the two DNS answers. The same
post-policy normalization is applied to an authorized mapped literal. No new
production branch or dependency is added; the connection attempt bound becomes
a bound on distinct effective socket destinations rather than wire spellings.
After simplifying the proof onto the existing counting connector, the whole
Rust tree remains 824/2,422 structural/cognitive points.

## 2026-09-02 — reject linear resolved-address deduplication

A simplification trial removed the temporary `HashSet` and checked the ordered
approved-address `Vec` directly. This would save one small allocation for the
common one- or two-answer lookup. Alternating hostname CONNECT medians were
161.32 and 163.91 microseconds for the detached baseline versus 159.86 for the
candidate, with overlapping intervals. No end-to-end improvement was measured.

The process ceiling can deliberately be raised from 64 to 1,024 addresses.
Linear deduplication would make a full unique answer perform roughly 523,776
equality checks for every connection, while the existing hash set keeps
expected work linear and the separate vector preserves resolver order. The
candidate was discarded. Production source, complexity, and allocation shape
remain unchanged.

## 2026-09-02 — retain Tokio's tunnel copy-buffer default

Tokio 1.53.1's `copy_bidirectional` allocates one 8 KiB buffer for each tunnel
direction. A controlled data-plane trial tested both sides of that default.
Four KiB per direction saved 8 KiB for each established tunnel but every
alternating eight-tunnel pair was slower: the candidate delivered roughly
1.83--1.99 GiB/s while the unchanged proxy delivered 2.54--2.77 GiB/s upload,
and 1.80--2.08 versus 2.56--2.92 GiB/s download.

Sixteen KiB per direction doubled the copy-buffer footprint and also failed to
improve throughput. After the host settled, candidate upload runs delivered
2.89--3.20 GiB/s versus 3.22--3.27 GiB/s for the alternating default; download
delivered 3.02--3.16 versus 3.09--3.38 GiB/s. Earlier 64-tunnel measurements
also contained extreme scheduler outliers, including one 11-second run, so
they are retained only as evidence that this local host is unsuitable for
fine-grained high-concurrency comparisons.

Both candidate implementations were discarded. The proxy retains Tokio's
maintained copier and its 16 KiB aggregate buffer cost per established tunnel,
with no new configuration surface, branch, or dependency.

## 2026-09-02 — close wildcard-listener aliases by port

The Lens review advanced to `9f04f2e`. Its new multi-listener work treats the
set of addresses reaching each proxy lane as an authority boundary and rejects
unspecified extra addresses. That sharpened a documented Sandbox Egress
residual: the self-endpoint guard recognized a wildcard listener's loopback
aliases but allowed other addresses on the same port. If an explicit network
grant covered another local interface, a guest could open a nested proxy path;
the proxy process's source address could then select a different lease.

A failing unit matrix first required both an ordinary public address and a
private-interface candidate to match a wildcard listener's port. The retained
rule is intentionally fail-closed: an unspecified bind rejects every
destination on its assigned port. A concrete bind still rejects only its exact
canonical endpoint. The compatibility cost is therefore limited to wildcard
deployments that need an unrelated destination on the listener port; those
callers can bind one concrete guest-facing address.

A real dual-stack-listener case resolves an allowed hostname to an otherwise
ordinary public address on the assigned port. It receives the stable
`proxy-endpoint-denied` response, records one accepted and one denied
connection, closes with zero active work, and makes zero connector calls. The
literal and DNS paths continue to share one pre-dial helper. Production logic
loses three conditions and adds no dependency or host-state snapshot.

## 2026-09-02 — retain explicit Rustls TLS 1.2 support

The production dependency graph already disables Hickory and Rustls defaults
and enables only the features used by the crate. A detached trial removed
Rustls's `tls12` feature on the theory that the server `Acceptor` might retain
enough syntax support for ClientHello-only inspection. All 20 focused TLS
cases passed, including the fixed OpenSSL and Apple SecureTransport fixtures,
fragmentation, ECH, cancellation, and constrained forwarding.

The stripped release executable changed from 2,884,456 to 2,884,432 bytes: a
24-byte reduction. That does not justify making TLS 1.2 compatibility depend
on incidental parser behavior behind a disabled feature. The candidate was
discarded; the manifest continues to state the intended TLS-version support
explicitly, with no source or lockfile change.
