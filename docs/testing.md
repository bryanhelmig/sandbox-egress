# Testing strategy

The suite is organized by claimed invariant rather than by source module.

- Unit tests: hostname and authority canonicalization, forbidden ranges,
  builder validation, counters, and state transitions.
- Integration tests: real listener, real CONNECT client, pinned local upstream,
  allow/deny behavior, limits, accounting, and shutdown.
- Concurrency tests: attach collisions, admission-versus-close races, many
  simultaneous tunnels, global and per-lease saturation, and identity reuse
  after certified close.
- Hostile conformance tests: deterministic phase barriers for headers, DNS,
  dial, ClientHello, and tunnel. Each phase must prove close returns only after
  its work is gone.
- Resource soak: repeated abuse with sampled RSS, thread count, and descriptor
  count. Platform-specific collectors report unsupported rather than silently
  passing.
- Parser robustness: deterministic malformed-input matrices and ordinary
  regression tests for every discovered defect.
- Boundary validation: extreme but type-valid durations are rejected by the
  public construction APIs before they can overflow runtime deadline math.

No test may depend on the public internet. DNS and upstream behavior must be
locally controlled so failures are reproducible.

Header conformance distinguishes a byte-ceiling violation (`431
header-too-large`), early EOF (`400 header-eof`), and the absolute slow-header
deadline (`408 header-timeout`). Each case must close with one denial and no
active connection; lease close during a still-pending header is tested
separately.

Paired capacity cases hold one admitted slow header open, then prove the next
socket is terminal under either the global or per-lease ceiling. The rejected
connection must add one denial without adding an accepted, active, or spawned
connection task.

Post-establishment upload and download ceiling cases send bytes across a
zero-byte budget, prove the peer receives none, preserve attempted-byte
accounting, and require exactly one policy denial. This is separate from socket
reset tests so transport failure cannot masquerade as policy enforcement.

Counter boundary tests seed cumulative atomics immediately below `u64::MAX`,
then require accepted, completed, denied, upload, and download totals to
saturate without a debug-build panic. The active gauge must still return to
zero.

Diagnostic limiter tests use an injected monotonic instant rather than sleeps.
They prove a fixed-window excess and a full channel are both nonblocking and
appear in the next delivered event's saturating suppression count. A public
real-socket case queues one hostname denial, closes the lease, reuses the same
source IP, and queues another. The events must carry distinct proxy-assigned
lease sequences, source identity, and the fixed `host-denied` reason—not either
guest-controlled hostname.

A direct sequence-boundary case sets the next internal lease sequence to
`u64::MAX` and requires typed attachment failure. It may not wrap into a value
that could alias an earlier run.

Performance gates begin as recorded baselines, not brittle absolute numbers.
Benchmarks cover attach/close, policy matching, admission contention, and
accounting overhead. Macrobenchmarks later report connections/sec, throughput,
p50/p95/p99 setup latency, peak RSS, threads, and file descriptors.

The initial opt-in resource harness runs identity churn with the proxy still
alive and samples each batch. On Linux it reads `/proc`; on macOS it uses `ps`
and `lsof`; other targets compile and report unsupported counters as absent.
Run `./scripts/measure-resources.sh [runs-per-batch] [batches]`.

The committed tunnel conformance lane currently checks zero and exact download
ceilings, independent per-tunnel budgets, idle tunnel shutdown, an uploader
whose upstream never reads, and a downloader whose guest never reads. Terminal
socket assertions reject timeouts: a peer that merely remains blocked is not
accepted as evidence of revocation.

The resolver seam is internal to tests, so production callers cannot replace
host-authenticated policy with a guest-selected backend. Controlled resolver
tests hold lookups pending, measure the exact concurrency ceiling, cancel both
active and queued work, and deliver an answer after close to prove no dial
consumer remains.

An oversized-answer case supplies one address beyond the default cardinality
ceiling and uses a recording connector. It requires the bounded
`dns-answer-too-large` denial, one denial counter increment, and zero dial
attempts; the implementation may not silently truncate the answer.

The equivalent internal connector seam holds a dial future pending after
recording the exact checked `SocketAddr`. Tests release it only through lease
cancellation or the absolute handshake deadline and observe its drop directly,
avoiding platform-dependent assumptions about unroutable addresses.

The TLS conformance module uses Rustls to accept incrementally fragmented
ClientHello records, plus a focused extension walk only after syntactic
acceptance to detect ECH. Real proxy tests prove that a matching coalesced
CONNECT and ClientHello arrives upstream byte-for-byte, while mismatched SNI
and strict ECH send zero tunnel bytes upstream. Separate phase tests hold a
partial ClientHello open and prove that both lease close and the absolute
handshake deadline end the client, upstream socket, parser work, and active
connection count.

A constrained-forwarding case uses a valid roughly 64 KiB ClientHello split
across bounded TLS records and reduces both upstream socket buffers. The peer
accepts but does not read. The absolute deadline must cancel the incomplete
upstream write, send less than the full hello, record one denial, and finish
with no active connection. This distinguishes a real forwarding barrier from
a parser-only timeout.

The uninspected path separately fills a bounded in-memory upstream to force
backpressure before tunnelling. Its original absolute handshake deadline must
cancel the buffered upload write and retain accounting for bytes already read
from the guest.

`docker build -t sandbox-egress:dev .` runs the standard factory and a small
Linux `/proc` resource smoke on the declared Rust 1.88 MSRV. Running the image
executes the serialized hostile conformance lane. The container is a clean-room
reproducer, not a substitute for the native OS matrix.

The container builds debug and release dependencies from the locked manifest
before copying project sources. This cache boundary makes source-only rebuilds
fast without changing the commands or exact Rust version used for verification.
Factory scripts and container metadata are included in `cargo package` output,
so a source package does not contain documentation for missing commands.

`./scripts/measure-load.sh [connections] [concurrency] [destinations]` drives
one lease through many concurrent real loopback CONNECTs in release mode. It
reports aggregate connections per second and p50/p95/p99 client-observed setup
latency. Setup latency ends at the `200 Connection Established` response;
aggregate time also includes deterministic tunnel teardown.

Each client sends a marker after setup. The controlled upstream consumes it
and resets that completed tunnel, preventing rapid repetitions from exhausting
the host's ephemeral ports with `TIME_WAIT` sockets. Dials are distributed
across several upstream destination ports as a second guard against per-tuple
limits. Sustained capacity still remains host-specific evidence.

`./scripts/measure-throughput.sh [MiB per tunnel] [concurrency]
[upload|download|both]` opens every CONNECT tunnel before releasing a shared
start barrier. Controlled peers then move bounded chunks in one direction and
perform an explicit teardown exchange. The test checks aggregate byte counters
exactly after certified close, including one marker byte per tunnel in the
opposite direction. This is a same-host regression measure, not a network
bandwidth claim.
