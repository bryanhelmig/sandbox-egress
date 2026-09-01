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
- Resource soak: repeated identity churn and concurrent host management with
  sampled RSS, thread count, and descriptor count. Platform-specific
  collectors report unsupported rather than silently passing.
- Parser robustness: deterministic malformed-input matrices and ordinary
  regression tests for every discovered defect.
- Executable contract: process-level tests pin the no-policy usage error and
  prove that stdin EOF starts, revokes, and cleanly closes the embedded lease.
- Boundary validation: extreme but type-valid durations are rejected by the
  public construction APIs before they can overflow runtime deadline math.
  Zero header timeouts are rejected as unusable. Extreme global connection and
  DNS limits are clamped to the runtime semaphore maximum and proven to start;
  the exact per-lease maximum builds while one larger returns a typed error.

No test may depend on the public internet. DNS and upstream behavior must be
locally controlled so failures are reproducible.

Hostname policy cases pin wildcard depth explicitly: `*.example.com` matches
both `api.example.com` and `deep.api.example.com`, but neither `example.com`
nor `notexample.com`. The wildcard therefore means any nonempty sequence of
complete left-hand labels, not TLS certificate wildcard semantics.

Canonicalization cases accept ASCII case and one trailing root dot, explicit
ACE/punycode text, 63-byte labels, and a 253-byte unrooted name. They reject a
64-byte label, longer names, multiple root dots, underscores, edge hyphens,
raw Unicode, a Unicode confusable, and IP literals. An end-to-end controlled
resolver case requires the same lowercase absolute name that the system
resolver receives, preventing local search-suffix behavior from disappearing
behind a test-only backend difference.

Port policy cases require an empty builder to allow no port and an HTTP-only
builder to allow 80 without inheriting 443. A real listener repeats the latter
case against CONNECT and requires the stable `port-denied` response.

Header conformance distinguishes a byte-ceiling violation (`431
header-too-large`), early EOF (`400 header-eof`), and the absolute slow-header
deadline (`408 header-timeout`). Each case must close with one denial and no
active connection; lease close during a still-pending header is tested
separately. A parser boundary case accepts 64 fields and rejects field 65 with
the stable `too-many-headers` response and diagnostic code, without copying
attacker-controlled names or values into the event. The byte ceiling accepts a
complete terminator whose last byte lands exactly at the configured limit and
rejects the same terminator shifted one byte beyond it.

A fixed parser matrix rejects obsolete field folding, NUL and other control
bytes, whitespace before a field name or colon, and non-ASCII CONNECT or Host
authority spellings. The mature request parser may identify UTF-8 in the
request target before the authority parser rejects it; the resulting stable
reason is `invalid-authority`, not generic malformed syntax.

The connection benchmark also sends a full 1 MiB header made from repeated
`\r\n\rX` near matches. It must remain an ordinary bounded 431 denial and makes
linear-scan CPU drift visible beside the existing all-`a` input.

Authority cases require bracketed IPv6 to remain accepted while bracketed DNS,
IPv4, and IPvFuture text return the exact `invalid-ipv6-literal` reason. A real
listener case verifies that reason is preserved in the bounded 400 response.
HTTP/1.1 cases reject missing, duplicate, malformed, hostname-mismatched, and
port-mismatched Host fields. Compatible case and IP spellings, a Host value
without the CONNECT port, and Host-less HTTP/1.0 remain accepted. A real socket
requires `host-header-mismatch` before policy or DNS and closes with one
accounted denial.

A unit boundary case places the four-byte header terminator at every split
around the proxy's 4 KiB read boundary and proves buffered tunnel bytes are
preserved. The connection benchmark sends a full 1 MiB unterminated header and
observes the real 431 response, guarding the incremental scan's CPU shape as
well as its parsing result. The scan remains a non-inlined code-generation
boundary because whole-program LTO otherwise made its optimized loop sensitive
to unrelated policy-constructor changes; benchmark comparison must accompany
any change to that boundary.

Paired capacity cases hold one admitted slow header open, then prove the next
socket is terminal under either the global or per-lease ceiling. The rejected
connection must add one denial without adding an accepted, active, or spawned
connection task. A dual-stack two-lease case saturates the one-slot global
budget through the IPv4 lease, requires the IPv6 refusal to affect only the
IPv6 lease, certifies release through IPv4 close, and then requires an IPv6
retry to be admitted.

A phase-synchronized identity contention case releases 32 host threads into
`Proxy::attach` together. Exactly one immutable policy must acquire the source
identity, all 31 other calls must return `IdentityInUse`, and another attach
must remain refused until the winner completes certified close. This pins the
single listener-owner command loop as the synchronization boundary without a
second shared registry lock.

The close-success phase barrier receives the final snapshot while identity
ownership is deliberately still retained, injects another unadmitted socket,
and requires that socket to close without changing any final counter. It also
proves cleanup readiness alone is not registry-release readiness, then retries
with a deadline shorter than the configured quiet period and requires the same
snapshot immediately. The case runs repeatedly because the original failure
was an ordering race, not a parser value bug.

A separate drain-barrier case injects a normal admission 100 milliseconds into
a 200 millisecond revocation interval. Cleanup must not complete at the
original deadline; it returns only after a new full interval and includes the
rejected socket in final usage. This proves observed backlog activity extends
both explicit close and the shared dropped-lease cleanup barrier.

The public lifecycle case connects an old-source socket during revocation and
sets the caller deadline between the original and restarted completion times.
It requires `DeadlineExceeded`, recovers the still-owning lease, observes the
denial, gets `IdentityInUse` from replacement attachment, then retries close
successfully only after old traffic stops.

The accept-queue barrier case keeps the biased management branch continuously
ready while an old-source socket waits in the kernel queue. The pre-fix loop
certified close and returned `200 Connection Established` under the replacement
policy. The retained case requires the listener-owned drain to reject and count
that socket against the old lease, restart quiet time, and leave it terminal
after replacement attachment. It is deterministic and does not depend on
public traffic or probabilistic scheduling.

An already-quiesced retry has a separate state-level case. It must request a
fresh listener drain before returning the stored final snapshot, but does not
repeat the quiet interval or alter final counters.

A proxy-wide shutdown case first records one real denied CONNECT, then joins
the runtime while retaining its lease handle. Calling `Lease::close` afterward
must return the committed snapshot with one accepted, one denied, and zero
active connections instead of reporting `RuntimeStopped`.

Two proxy-wide failure cases pin the commit boundary. A dial future whose drop
blocks beyond the first deadline must return `ShutdownError` with the owning
proxy, reject a new identity as `ProxyStopping`, then certify on retry and leave
the retained lease with a consumable closed snapshot. A separate case removes
the success receiver before an empty shutdown; the runtime must remain alive in
stopping state until a later caller actually receives the certificate.

Legacy numeric host spellings (`127.1`, leading-zero dotted form, hexadecimal
integer, and single decimal integer) are exercised end to end. Each is allowed
only as a hostname, resolved to loopback by a controlled resolver, rejected by
the forbidden-address floor, and required to produce zero connector calls.

Lease Drop is exercised while stack unwinding with a pending dial: cancellation
must complete, the guest socket must become terminal, and the same identity
must become attachable again after best-effort cleanup. Replacement attachment
can precede processing of the pointer-checked stale release, so the proof waits
separately and boundedly for the old state's final strong owner to disappear. A
second case stops and joins the proxy runtime first, then requires lease Drop
to remain non-panicking and release its final local state owner even though the
command receiver is gone.

A public lifecycle case records a real denied request, then forces three
consecutive close deadlines. Every error must return the same lease ID, retain
the source identity, and preserve the exact nonzero usage snapshot. A later
successful retry must certify that unchanged snapshot as final.

Four barrier-synchronized cases race explicit proxy shutdown and best-effort
proxy drop against both certified lease close and lease drop while a dial is
pending. Explicit shutdown must succeed in both cases; where lease close is
present, it must succeed too, regardless of command order. Drop paths must
still destroy the dial; best-effort proxy drop must join the owned runtime in
the test boundary, and lease drop must release the final strong lease-state
reference.

The Docker factory is multi-stage. Rust 1.88 first warms every locked build and
test dependency, then performs the complete check and resource lane with Cargo
offline. A checked collector reads Cargo's JSON artifact records, requires
exactly one executable for each conformance target, strips copies, and carries
only those binaries into a Debian runner. The CLI's compile-time executable
dependency is copied at its exact embedded path. The factory deletes its
compilation tree only after collection, before committing the source-dependent
layer. The final image runs as UID/GID 65534 and must reproduce all 128
deterministic cases without Cargo, source, or a build cache.

Source-identity cases prove an IPv4 address and its mapped IPv6 transport
spelling collide in the registry. A real dual-stack listener routes an IPv4
client to that canonical lease, while the IPv6 CONNECT case uses an IPv6
listener, IPv6 source identity, checked IPv6 destination, and IPv6 upstream.

Post-establishment upload and download ceiling cases send bytes across a
zero-byte budget, prove the peer receives none, preserve attempted-byte
accounting, and require exactly one policy denial. This is separate from socket
reset tests so transport failure cannot masquerade as policy enforcement.

Two ordinary FIN cases prove tunnel directions remain independent. After a
guest finishes upload, an upstream may still send a delayed response; after an
upstream finishes download, the guest may still send a late upload. Both paths
must preserve exact byte counters and count one normally completed tunnel. A
separate upstream RST case proves delivered upload bytes remain accounted while
the tunnel is neither completed nor classified as a policy denial.

Counter boundary tests seed cumulative atomics immediately below `u64::MAX`,
then require accepted, completed, denied, upload, and download totals to
saturate without a debug-build panic. The active gauge must still return to
zero.

Diagnostic limiter tests use an injected monotonic instant rather than sleeps.
They prove a fixed-window excess and a full channel are both nonblocking and
appear in the next delivered event's saturating suppression count. A public
real-socket concurrency case keeps a zero-capacity diagnostic channel full
while 64 admitted connections are denied, then requires successful certified
close and exact final accounting. A separate case queues one hostname denial,
closes the lease, reuses the same source IP, and queues another. The events must
carry distinct proxy-assigned lease sequences, source identity, and the fixed
`host-denied` reason—not either guest-controlled hostname.

A direct sequence-boundary case sets the next internal lease sequence to
`u64::MAX` and requires typed attachment failure. It may not wrap into a value
that could alias an earlier run.

Performance gates begin as recorded baselines, not brittle absolute numbers.
Benchmarks cover attach/close, policy matching, admission contention, and
accounting overhead. Macrobenchmarks later report connections/sec, throughput,
p50/p95/p99 setup latency, peak RSS, threads, and file descriptors.

The opt-in resource target first runs identity churn with the proxy still alive
and samples each batch. A second lane synchronizes 64 host threads, holds all
of their distinct attached leases, then releases their close calls together in
four repeated batches. It samples the intentional peak and requires descriptor
and thread recovery after every batch and shutdown. On Linux the collectors
read `/proc`; on macOS they use `ps` and `lsof`; other targets compile and
report unsupported counters as absent. Run
`./scripts/measure-resources.sh [runs-per-batch] [batches]`; the concurrent lane
can be adjusted with `SANDBOX_EGRESS_CONTROL_CONCURRENCY` and
`SANDBOX_EGRESS_CONTROL_BATCHES`.

The committed tunnel conformance lane currently checks graceful half-close in
both directions, upstream reset classification, zero and exact download
ceilings, independent per-tunnel budgets, idle tunnel shutdown, an uploader
whose upstream never reads, and a downloader whose guest never reads. Terminal
socket assertions reject timeouts: a peer that merely remains blocked is not
accepted as evidence of revocation.

A simultaneous-backpressure case makes the guest and upstream write
continuously while neither reads. Both accounting directions must advance
before certified close, both hostile writers must then observe a terminal
socket error, and final active ownership must be zero.

The resolver seam is internal to tests, so production callers cannot replace
host-authenticated policy with a guest-selected backend. Controlled resolver
tests hold lookups pending, measure the exact concurrency ceiling, cancel both
active and queued work, and deliver an answer after close to prove no dial
consumer remains.

A pending resolver that exceeds its configured deadline must yield `504
dns-timeout`, release its active work, and make zero connector calls. A
resolver that returns an I/O error remains `502 dns-failed` and likewise never
reaches the connector. These complement the separate `503 dns-capacity` proof
for work that cannot acquire a resolver permit in time.

Resolver construction tests inspect Hickory's effective options and require
the configured response-count ceiling plus the same maximum TTL for positive
and negative entries. Configuration can narrow the built-in 8,192-entry and
24-hour limits but cannot widen them. An identity-reuse case gives two runs the
same hostname and repeated loopback answer: the first policy explicitly grants
loopback and reaches the connector, while the replacement policy does not and
must deny without another connector call.

A fixed local UDP DNS server exercises the actual Hickory wire path without
using the public network. With zero cache capacity, two identical lookups must
produce two upstream queries. With an enabled cache and a one-second maximum
TTL, an immediate repeat is served from cache and a lookup after 1.2 seconds
must produce the second upstream query. A fixed NXDOMAIN response carrying a
60-second SOA negative TTL follows the same sequence, proving that the shared
ceiling constrains negative caching in behavior as well as configuration.

An oversized-answer case supplies one address beyond the default cardinality
ceiling and uses a recording connector. It requires the bounded
`dns-answer-too-large` denial, one denial counter increment, and zero dial
attempts; the implementation may not silently truncate the answer.

A duplicate-answer case supplies the same approved address in all 64 default
slots and requires exactly one dial attempt. The mixed allowed/metadata case
uses the same recording connector and requires zero attempts, proving the
entire set is validated before first-seen-order deduplication reaches dialing.
A deterministic failover case holds the first approved connector future
pending under a 400 ms absolute deadline and maps the second address to a local
listener. It must connect through the second address in resolver order; the old
single-deadline loop times out after observing only the first.

Network-specific NAT64 cases pin every RFC 6052 embedding layout against the
standard's published address examples. A controlled resolver also returns a
globally shaped IPv6 address that embeds `169.254.169.254` under a configured
`/96`; the ordinary `resolved-address-denied` path must run and the recording
connector must observe zero attempts. A nonstandard prefix length is rejected
when the proxy starts.

The equivalent internal connector seam holds a dial future pending after
recording the exact checked `SocketAddr`. Tests release it only through lease
cancellation or the absolute handshake deadline and observe its drop directly,
avoiding platform-dependent assumptions about unroutable addresses.

A direct connection-task case supplies an already-expired listener-accept
timestamp without sending any header bytes. It must immediately return the
bounded `header-timeout` denial, proving scheduler delay cannot reset either
absolute deadline when the spawned task begins.

The TLS conformance module uses Rustls to accept incrementally fragmented
ClientHello records, plus a focused extension walk only after syntactic
acceptance to detect ECH. Real proxy tests prove that a matching coalesced
CONNECT and ClientHello arrives upstream byte-for-byte, while mismatched SNI
and strict ECH send zero tunnel bytes upstream. A syntactically ambiguous SNI
list with two hostname entries must fail mature parsing, close the guest, count
one denial, and send zero bytes upstream. Separate phase tests hold a partial
ClientHello open and prove that both lease close and the absolute handshake
deadline end the client, upstream socket, parser work, and active connection
count.

A two-connection public-path case distinguishes a syntactically valid
ClientHello with no SNI from bytes that are not a TLS record. It requires the
stable `tls-sni-missing` and `client-hello-invalid` diagnostics respectively,
zero bytes at both upstreams, exact attempted-upload accounting, two denials,
and no completed or active connections.

A fixed valid ClientHello includes a GREASE cipher-suite value and a GREASE
extension before SNI. Rustls must accept it, the focused extension walk must
not mistake it for ECH, and the full inspected path must forward every byte
unchanged. This is an ordinary deterministic compatibility case, not a claim
that one fixture represents every deployed TLS client.

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
executes the serialized hostile conformance lane, including the thin wrapper's
process-level contract. The container is a clean-room reproducer, not a
substitute for the native OS matrix.

The container builds debug and release dependencies from the locked manifest
before copying project sources. This cache boundary makes source-only rebuilds
fast without changing the commands or exact Rust version used for verification.
Factory scripts and container metadata are included in `cargo package` output,
so a source package does not contain documentation for missing commands. The
ordinary local factory compiles that assembled package, matching the dedicated
CI release gate; a source-tree build alone cannot prove the include list is
complete.

`./scripts/measure-load.sh [connections] [concurrency] [destinations]` drives
one lease through many concurrent real loopback CONNECTs in release mode. It
reports aggregate connections per second and p50/p95/p99 client-observed setup
latency. Setup latency ends at the `200 Connection Established` response;
aggregate time also includes deterministic tunnel teardown.

The connection Criterion suite also pairs two hostname CONNECT paths around
the same valid ClientHello and upstream acknowledgement. One disables TLS
inspection and one requires visible SNI. The controlled upstream asserts that
both receive the exact ClientHello bytes, so the comparison includes policy
enforcement rather than timing only the early 200 response.

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
