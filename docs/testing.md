# Testing strategy

The suite is organized by claimed invariant rather than by source module.

- Unit tests: hostname and authority canonicalization, forbidden ranges,
  builder validation, counters, and state transitions.
- Integration tests: real listener, real CONNECT client, pinned local upstream,
  allow/deny behavior, limits, accounting, and shutdown.
- Concurrency tests: attach collisions, admission-versus-close races, many
  simultaneous tunnels, and identity reuse after certified close.
- Hostile conformance tests: deterministic phase barriers for headers, DNS,
  dial, ClientHello, and tunnel. Each phase must prove close returns only after
  its work is gone.
- Resource soak: repeated abuse with sampled RSS, thread count, and descriptor
  count. Platform-specific collectors report unsupported rather than silently
  passing.
- Fuzzing: parsers and policy normalization, added once their internal seams
  stabilize. Seed regressions remain ordinary tests.

No test may depend on the public internet. DNS and upstream behavior must be
locally controlled so failures are reproducible.

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
