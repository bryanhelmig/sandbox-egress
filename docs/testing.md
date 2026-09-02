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

The optional `scripts/check-iana-drift.sh` maintainer command is deliberately
outside the factory. It downloads the authoritative IPv4 and IPv6
special-purpose CSVs and compares them with reviewed SHA-256 pins. Drift fails
the command and requires a human policy review; it never changes source.

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

Hostname precedence is pinned with an allowed wildcard, an overlapping exact
grant, and an exact denial. The denial must win while unrelated shallow and
deep subdomains remain allowed. Through the real listener, the denied hostname
returns `host-denied` despite mixed case and a trailing DNS root dot. A deep
name under a wildcard denial is rejected while the wildcard apex stays outside
that denial. Neither listener denial produces a resolver or connector call;
certified close returns two accepted, two denied, and zero active connections.
Canonical duplicate exact and wildcard rules collapse during construction, so
an immutable policy does not retain repeated matching work.

Destination precedence is pinned both as a pure immutable-policy case and
through the listener. A denied public `/24` overlaps an explicit `0.0.0.0/0`
grant. An allowed hostname resolving inside that `/24` must return
`resolved-address-denied`; the equivalent direct literal must return
`ip-literal-denied`. Together they require zero connector calls, two exact
denials, and certified final counters. The pure policy matrix repeats the
denial through IPv4-mapped, compatible, well-known NAT64, and configured NAT64
spellings while an overlapping IPv6 catch-all grant is present.

Header conformance distinguishes a byte-ceiling violation (`431
header-too-large`), early EOF (`400 header-eof`), and the absolute slow-header
deadline (`408 header-timeout`). A controlled transport reset remains the
distinct `400 header-read-failed` result. A bounded async-stream test writes another
byte every millisecond and proves that activity cannot turn the absolute
deadline into an idle timeout; a separate real-listener test checks the exact
408 wire response and final accounting. Each case must close with one denial
and no active connection; lease close during a still-pending header is tested
separately. A parser boundary case accepts 64 fields and rejects field 65 with
the stable `too-many-headers` response and diagnostic code, without copying
attacker-controlled names or
values into the event. The byte ceiling accepts a complete terminator whose
last byte lands exactly at the configured limit and rejects the same terminator
shifted one byte beyond it.

A fixed parser matrix rejects obsolete field folding, NUL and other control
bytes, whitespace before a field name or colon, and non-ASCII CONNECT or Host
authority spellings. The mature request parser may identify UTF-8 in the
request target before the authority parser rejects it; the resulting stable
reason is `invalid-authority`, not generic malformed syntax.

The connection benchmark also sends a full 1 MiB header made from repeated
`\r\n\rX` near matches. It must remain an ordinary bounded 431 denial and makes
linear-scan CPU drift visible beside the existing all-`a` input.
The same incremental framer reads upstream-proxy responses under an independent
32 KiB ceiling; split-terminator and near-terminator cases cover both callers.

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

The listener-error backoff has a deterministic state-level boundary case: the
first failure waits five milliseconds, consecutive failures double only to a
one-second ceiling, and one successful accept or drain resets the sequence.
The management path treats a failed mandatory drain as
`ListenerUnavailable`; it never converts that failure into an empty-queue
certificate.

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
only as a hostname, sent through the production resolver to an explicit local
DNS server, resolved to loopback, rejected by the forbidden-address floor, and
required to produce zero connector calls. The fixture observes both A and AAAA
wire queries for every spelling.

The provider-control exception has its own real-listener proof. A controlled
hostname resolves to Azure WireServer's public-looking `168.63.129.16`; the
request must receive `resolved-address-denied`, the connector must observe zero
attempts, and certified close must return one accepted, one denied, and zero
active connections.

Two self-connection cases explicitly grant the listener's loopback network and
assigned port. A literal real-listener request must still receive
`proxy-endpoint-denied` with exact final accounting. A controlled hostname
answer for the same listener must receive that denial with zero connector
calls. A wildcard-listener case returns an ordinary public answer on the same
port and must also deny with zero connector calls. The matching unit matrix
covers an IPv4-mapped spelling, IPv4 and dual-stack wildcard binds, both private
and public candidate addresses, and the different-port boundary.

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

An already-expired close deadline has its own public boundary case. The call
must first transition the lease to revoking, then return `DeadlineExceeded`
with ownership. Replacement attachment remains refused, a newly arriving
socket is terminated and attributed to the old lease without admission, and a
later retry certifies exact final counters. The deadline bounds how long the
caller waits for certification; it never defers cancellation or reopens
admission.

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
layer. The final image runs as UID/GID 65534 and must reproduce every
deterministic case without Cargo, source, or a build cache.

Source-identity cases prove an IPv4 address and its mapped IPv6 transport
spelling collide in the registry. A real dual-stack listener routes an IPv4
client to that canonical lease, while the IPv6 CONNECT case uses an IPv6
listener, IPv6 source identity, checked IPv6 destination, and IPv6 upstream.
A public attachment matrix rejects IPv4 and IPv6 unspecified, multicast,
limited-broadcast, mapped-multicast, and mapped-broadcast source identities
before sequence allocation; the next valid attachment must still receive lease
ID 1.
A separate real-socket case attaches restrictive and permissive policies to
two loopback source identities. A client observed as the restrictive identity
sends the permissive address in `X-Run-ID`; it must still be denied before dial,
and the claimed lease must retain zero accepted and denied connections.

Post-establishment upload and download ceiling cases send bytes across a
zero-byte budget, prove the peer receives none, preserve attempted-byte
accounting, and require exactly one policy denial. Paired nonzero cases write
one byte beyond a seven-byte ceiling in a single call and require exactly the
seven-byte prefix on the other side, removing kernel read coalescing from
policy behavior. This is separate from socket reset tests so transport failure
cannot masquerade as policy enforcement. A deterministic reader also pins the
boundary ordering: a reset immediately after exactly three allowed bytes stays
a reset with three bytes accounted, while one successful fourth-byte read is
accounted and returned as the transfer-limit error before a later reset.

Two ordinary FIN cases prove tunnel directions remain independent. After a
guest finishes upload, an upstream may still send a delayed response; after an
upstream finishes download, the guest may still send a late upload. Both paths
must preserve exact byte counters and count one normally completed tunnel. A
separate asymmetric full-duplex case sends 1,048,699 patterned bytes upward and
3,145,771 different patterned bytes downward concurrently on one real tunnel.
Both peers must receive the complete payload, and certified close must return
those exact counters with one completed connection and no denial. A
certified-close case then holds the remaining direction open after the guest's
upload FIN has propagated. Close must return without upstream cooperation,
leave no active ownership, make the guest read terminal, and make a subsequently
released upstream writer observe a terminal socket error. A separate upstream
RST case proves delivered upload bytes remain accounted while the tunnel is
neither completed nor classified as a policy denial. The mirror case waits
until download accounting advances, resets the guest, and requires the upstream
writer to hit a terminal error while the read bytes remain in the final
counters. A refused local destination separately requires a bounded 502
`dial-failed` denial before any CONNECT-success response.

Counter boundary tests seed cumulative atomics immediately below `u64::MAX`,
then require accepted, completed, denied, upload, and download totals to
saturate without a debug-build panic. The active gauge must still return to
zero.

Diagnostic limiter tests use an injected monotonic instant rather than sleeps.
They prove a fixed-window excess and a full channel are both nonblocking and
appear in the next delivered event's saturating suppression count. A
receiver-disconnect case proves the first failed send disables later reporting
before it consumes another rate slot. A public
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
four repeated batches. A third lane holds a configurable number of partial
CONNECT headers before their deadline, proves every admission is active, then
requires certified close to make all guest sockets terminal and recover
descriptors and threads. Its 512-connection measurement also catches changes
to the bounded header buffer's per-handshake reserve. A fourth lane establishes
a configurable batch of
silent tunnels under one idle-expiring lease. It samples their simultaneous peak,
requires every guest and upstream socket to become terminal, and checks exact
idle-denial counters plus recovered descriptors and threads. A fifth lane
keeps one real lease and upstream alive, then alternates completed echo tunnels,
upload-ceiling denials, channel-timed upstream resets after CONNECT success,
and pre-DNS hostname denials. It waits for active ownership to return to zero
and samples descriptor and thread recovery after every batch and final
shutdown. Final accepted, completed, denied, upload, and download counters must
exactly distinguish all four paths; the reset is neither a completion nor a
policy denial. A sixth lane holds a configurable number of TLS-inspected
tunnels at 60,020 bytes of a legal, incomplete, multi-record `ClientHello`.
Aggregate upload accounting proves every parser buffer is live before the peak
sample. Successful lease close must then cancel all of them, make every guest
and upstream socket terminal, freeze exact zero-denial final counters, and
recover descriptors and threads. A seventh lane repeatedly drives both guest
and upstream nonblocking writers until each independently observes a full send
queue while neither application reads. Every certified close must terminate
both writers, freeze positive bidirectional accounting with no completion or
denial, and permit the same source identity to attach again. It samples
descriptor and thread recovery after each batch and final shutdown. An eighth
lane holds a configurable batch after the approved numeric CONNECT request has
reached an operator-controlled upstream proxy, but while that proxy has
returned only 900 bytes of an unterminated response header. Certified close
must cancel every response parser, make every guest and upstream socket
terminal, freeze exact zero-completion and zero-denial counters, and recover
descriptors and threads.

On Linux the collectors read `/proc`; on macOS they use `ps` and `lsof`; other
targets compile and report unsupported counters as absent. The runner starts
each lane in a fresh process so one lane's allocator high-water mark cannot be
misattributed to the next, while keeping the lanes serial to avoid host
contention. Run
`./scripts/measure-resources.sh [lease-runs-per-batch]
[lease-batches] [idle-connections] [TLS-connections]
[terminal-runs-per-batch] [terminal-batches] [partial-header-connections]
[partial-upstream-response-connections]`.
Management churn remains
adjustable with
`SANDBOX_EGRESS_CONTROL_CONCURRENCY` and
`SANDBOX_EGRESS_CONTROL_BATCHES`; the repeated pressure lane uses
`SANDBOX_EGRESS_BACKPRESSURE_RUNS` and
`SANDBOX_EGRESS_BACKPRESSURE_BATCHES`; the upstream-response lane uses
`SANDBOX_EGRESS_UPSTREAM_CONNECTIONS`.

The terminal-churn harness waits for each asserted protocol outcome and then
uses abortive close on its own hostile-path client socket. Its completion and
upload-limit fixtures send upstream EOF first and explicitly synchronize the
completed half-close. Together these prevent a long measurement from
exhausting macOS's ephemeral source ports on the deliberately repeated local
tuples. This is measurement plumbing, not proxy behavior; ordinary graceful
half-close and reset semantics remain covered separately in the tunnel
conformance suite.

The committed tunnel conformance lane currently checks graceful half-close in
both directions, upstream reset classification, zero and exact download
ceilings, independent per-tunnel budgets, idle tunnel shutdown, an uploader
whose upstream never reads, a downloader whose guest never reads, guest-reset
broken-pipe accounting, and refusal before CONNECT success. Terminal socket
assertions reject timeouts: a peer that merely remains blocked is not accepted
as evidence of revocation.

A simultaneous-backpressure case makes the guest and upstream write
continuously while neither reads. Both accounting directions must advance
before certified close, both hostile writers must then observe a terminal
socket error, and final active ownership must be zero.

The idle-policy mirror lets both writers continue without host intervention.
Bytes initially move while TCP buffers have capacity, so both activity
directions must be counted. Once backpressure stops successful proxy reads, the
shared idle clock must expire, both writers must receive a terminal socket
error rather than their five-second failure bound, and final accounting must
show one idle denial, no completion, and no active work.

The resolver seam is internal to tests, so production callers cannot replace
host-authenticated policy with a guest-selected backend. Controlled resolver
tests hold lookups pending, measure the exact concurrency ceiling, cancel both
active and queued work, and deliver an answer after close to prove no dial
consumer remains.

A pending resolver that exceeds its configured deadline must yield `504
dns-timeout`, release its active work, and make zero connector calls. A
resolver that returns an I/O error remains `502 dns-failed` and likewise never
reaches the connector. A successful lookup containing no addresses is the
distinct `502 dns-empty` result and also makes zero connector calls. These
complement the separate `503 dns-capacity` proof for work that cannot acquire a
resolver permit in time.

Resolver construction tests inspect Hickory's effective options and require
the configured response-count ceiling plus the same maximum TTL for positive
and negative entries, as well as TCP recovery after a failed UDP exchange.
Caching is disabled by default. Configuration can opt into at most 64 entries
and a 24-hour TTL but cannot widen those ceilings. Explicit server tests use a
nonstandard loopback port, require the hosts file to be disabled, and complete
a UDP-truncated response over a real TCP DNS connection. More than eight
distinct servers, port zero, and scoped IPv6 addresses fail validation. An
identity-reuse case gives two runs the same hostname and repeated loopback
answer: the first policy explicitly grants loopback and reaches the connector,
while the replacement policy does not and must deny without another connector
call.

An address-equivalence case grants both IPv4 loopback and its IPv4-mapped IPv6
form, then returns both for one hostname. Policy sees each original form, but
the connector must receive only one dial attempt. This prevents equivalent
resolver spellings from amplifying fallback dials without weakening exact
policy evaluation.

A fixed local UDP DNS server exercises the actual Hickory wire path without
using the public network. With zero cache capacity, two identical lookups must
produce two upstream queries. With an enabled cache and a one-second maximum
TTL, an immediate repeat is served from cache and a lookup after 1.2 seconds
must produce the second upstream query. A fixed NXDOMAIN response carrying a
60-second SOA negative TTL follows the same sequence, proving that the shared
ceiling constrains negative caching in behavior as well as configuration.

An invalid response claiming the maximum 65,535 answer records but containing
no record body must exhaust the resolver's bounded retries as `dns-failed`,
make no dial attempt, and leave no active lease work. This pins the actual
dependency decoder path without pretending that the returned-address ceiling
can prevent the decoder from seeing wire section counts first.

A real proxy/lease case holds Hickory's parallel A and AAAA requests at a local
recursive server, certifies lease close, then releases valid late SERVFAIL
responses. The server watches both new UDP packets and TCP accepts for 400 ms
and requires neither. This pins the dependency-level cancellation behavior:
closing our lookup future must close Hickory's request completion channel and
prevent a late failure from entering its retry path. The case also requires the
guest socket to terminate and final active ownership to be zero.

An oversized-answer case supplies one address beyond the default cardinality
ceiling and uses a recording connector. It requires the bounded
`dns-answer-too-large` denial, one denial counter increment, and zero dial
attempts; the implementation may not silently truncate the answer.

A duplicate-answer case supplies the same approved address in all 64 default
slots and requires exactly one dial attempt. The mixed allowed/metadata case
uses the same recording connector and requires zero attempts, proving the
entire set is validated before first-seen-order deduplication reaches dialing.
A real UDP CNAME case allows the original hostname, observes Hickory's A and
AAAA questions through a seven-link noncyclic chain, returns
`169.254.169.254` for the terminal name, and requires the ordinary
resolved-address denial with zero connector calls. This proves a near-limit
alias chain cannot inherit hostname trust at the IP boundary.
A two-name CNAME cycle receives valid alias replies through the production
resolver and requires exactly 16 A/AAAA questions: the resolver's eight-hop
bound for each family. It then returns `dns-failed`, never calls the connector,
and closes with one denial. The alias, cycle, and incomplete-reply cases live in
`src/proxy/tests/dns_wire.rs`, keeping wire construction out of the proxy's
main conformance body.
An incomplete-wire case answers each A/AAAA attempt with only the two-byte
transaction identifier. It requires exactly six questions, a bounded
`dns-failed` response, zero connector calls, and exact final denial accounting.
The focused case is repeated 25 times; broader malformed response shapes remain
an explicit hardening inventory rather than an implied claim.
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

The upstream-proxy lane uses a real local HTTP CONNECT peer. It requires that
the peer receive only the already-approved numeric target, preserves payload
bytes coalesced with the peer's 2xx response, and checks exact guest byte
accounting. A one-byte download ceiling separately requires a six-byte
coalesced payload to account all six bytes while forwarding exactly the first
byte and denying the tunnel. Other cases pin a distinct denial for a 407
refusal, reject a response header that reaches the 32 KiB ceiling, reject
configuring the shared listener as its own upstream, and certify lease
revocation while the peer withholds its response. The last case requires both
guest and upstream sockets to become terminal without releasing the peer first.
A four-guest capacity barrier with two dial permits then withholds every
upstream response, proves exactly two upstream negotiations exist for 200
milliseconds, and requires certified close to cancel both active negotiations
and both queued permit waits.

Upstream fallback has an end-to-end controlled-resolver proof. One absolute
hostname lookup returns two explicitly approved addresses; the local upstream
proxy refuses the first numeric CONNECT and accepts the second. The guest then
exchanges bytes through the tunnel, the upstream never receives the hostname,
no second DNS lookup occurs, and certified close reports one accepted and
completed connection with exact byte counters.

A phase barrier holds the first address attempt until a coordinator has first
cancelled the lease token. Releasing that attempt as a failure must return no
connection and leave the second address entirely untouched. Removing the
per-attempt cancellation observation makes the case deterministically record
both addresses.

The connection Criterion suite also measures one complete upstream-proxy
negotiation beside direct allowed CONNECT. The local peer consumes and verifies
the exact numeric CONNECT request before replying, so the measurement includes
the second TCP setup, request reconstruction, bounded response parse, and guest
success response without involving public DNS or traffic.
Another target sends the full 32 KiB upstream response ceiling as repeated
`\r\n\rX` near matches without a terminator. It requires the normal bounded 502
denial and exposes repeated-scan CPU growth in the upstream response reader.

Dial admission has its own process-wide phase budget. With five approved
connections and two permits, the connector must observe exactly two live
attempts; certified close cancels those attempts and all three queued waits.
A two-identity case then occupies the only permit with one lease while a
dual-stack request on another lease consumes its absolute handshake deadline.
That contender receives exactly one `dial-capacity` denial and never reaches
the connector. A public system-dial case sets the limit to one and holds two
real loopback tunnels open at once, proving each permit is released after
connection establishment rather than retained for tunnel lifetime. Extreme
host configuration is also clamped before semaphore construction.

A direct connection-task case supplies an already-expired listener-accept
timestamp without sending any header bytes. It must immediately return the
accounted `header-timeout` denial and close without response bytes, proving
scheduler delay cannot reset either absolute deadline when the spawned task
begins or create diagnostic work after it expires.

The shared deadline primitive is tested with work that would complete
immediately: when the supplied deadline is already expired, the work is never
polled. The production CONNECT-success writer is then exercised through a
one-byte-capacity in-memory stream. It cannot finish the 39-byte response and
must return at the handshake deadline with only a strict prefix observable.
This pins the response write as part of the handshake rather than an unbounded
gap between dialing and ClientHello or tunnel work.

Real-socket denial cases separately prove ordinary one-shot diagnostic
delivery and zero response bytes once the absolute deadline expires. Denial
delivery is intentionally best-effort: response construction and delivery are
skipped after expiry, while accounting and socket shutdown do not wait for a
guest to read the diagnostic body.

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

A roughly 60 KiB valid hello split across legal 16 KiB TLS records is also
delivered with no bytes coalesced after CONNECT and, separately, one transport
byte per async read. The empty-initial-buffer case pins a discovered boundary:
if Rustls consumes only the first complete record from a larger socket read,
the proxy must feed every retained byte before reading the socket again. EOF
cannot override a complete hello that is already in the bounded buffer. Both
shapes must recover the same SNI and preserve the exact wire bytes.

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

Two complete first records captured from independent clients add a small,
offline compatibility corpus: OpenSSL 3.6.3 and Apple SecureTransport. Each
must parse to the exact `fixture.example` SNI, report no ECH, and retain every
wire byte unchanged. Their invocation, length, and SHA-256 provenance live
beside the fixtures in `tests/fixtures/README.md`; changing them is a reviewed
compatibility update rather than an ambient dependency on locally installed
TLS software.

A constrained-forwarding case uses a valid roughly 64 KiB ClientHello split
across bounded TLS records and reduces both upstream socket buffers. The test
connector fills its send queue to an observed `WouldBlock` before returning the
socket and records that exact prefill; the peer accepts but does not read. The
absolute deadline must cancel the blocked upstream write, send less than the
full hello beyond the prefill, record one denial, and finish with no active
connection. This distinguishes a real forwarding barrier from a parser-only
timeout without relying on platform TCP buffer requests as proof of pressure.

The uninspected path separately fills a bounded in-memory upstream to force
backpressure before tunnelling. Its original absolute handshake deadline must
cancel the buffered upload write and retain accounting for bytes already read
from the guest. Closing the peer instead proves an immediate upstream write
failure has a distinct reason and retains the same attempted-byte accounting.

## Coverage review

`./scripts/measure-coverage.sh` runs the full deterministic suite under LLVM
source coverage. It requires the exact `cargo-llvm-cov` version printed by the
script plus the Rust `llvm-tools-preview` component. Keeping it outside
`check.sh` avoids making instrumentation overhead part of the ordinary edit
loop.

Coverage is a map for review rather than a pass/fail percentage. Uncovered
parser, resolver, accounting, cancellation, and shutdown spans deserve
inspection; platform errors and defensive fallbacks may remain impractical to
force deterministically. Tests added solely to move the total are not evidence
of a stronger boundary.

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

The ordinary allowed-CONNECT benchmark has a direct loopback TCP control using
the same upstream listener and reset-on-drop behavior. Comparing them does not
subtract a precise parser cost, but it shows when the host's TCP stack and
scheduler moved enough to invalidate a small proxy-path claim.

Each client sends a marker after setup. The controlled upstream consumes it
and resets that completed tunnel, preventing rapid repetitions from exhausting
the host's ephemeral ports with `TIME_WAIT` sockets. Dials are distributed
across several upstream destination ports as a second guard against per-tuple
limits. Sustained capacity still remains host-specific evidence.

`./scripts/measure-throughput.sh [MiB per tunnel] [concurrency]
[upload|download|both] [idle timeout ms]` opens every CONNECT tunnel before
releasing a shared start barrier. Controlled peers then move bounded chunks in
one direction and perform an explicit teardown exchange. The test checks
aggregate byte counters exactly after certified close, including one marker
byte per tunnel in the opposite direction. Zero leaves idle expiry disabled; a
positive fourth argument measures the opt-in activity-clock cost while
continuous traffic keeps every tunnel alive. This is a same-host regression
measure, not a network bandwidth claim.

The tunnel lane separately pins idle semantics. One case proves an established
silent tunnel closes both guest and upstream and contributes exactly one
`tunnel-idle-timeout` denial. Separate upload-and-echo and download-only cases
move bytes for longer than the configured interval, proving traffic from
either side postpones expiry, then observe closure only after traffic stops.
The certified-close case uses a much longer idle interval and proves revocation
preempts the waiter without misclassifying shutdown as a denial.

## Connection churn and Linux host evidence

Deterministic token-bucket unit cases use explicit `Instant` values: the full
burst is available immediately, sub-token refill accumulates without floating
point, and a long refill cannot exceed burst capacity. Real-listener cases then
hold one partial header open and require the next immediate attempt to be
rejected by the configured per-lease or process-wide bucket before a task is
spawned. Certified close returns one accepted, one denied, and zero active
connections. A separate close-and-reattach case proves the replacement lease
receives a full fresh burst rather than inheriting rate state from the old run.
Its process-wide counterpart proves that lease replacement does not reset the
global bucket and therefore cannot evade the fleet-level churn control.

`scripts/measure-linux-network-state.sh COMMAND ...` samples host-global
conntrack, TCP, TIME_WAIT, UDP, and allocated-file counters around a
deterministic command. It records baseline, high-water, and final values plus
the configured conntrack ceiling. Recovery can be informational or required
with an explicit slack, because a shared developer host may have unrelated
traffic while a dedicated release worker should not.

`scripts/test-linux-host-boundary.sh` is an opt-in privileged lane, kept out of
the ordinary unprivileged factory. It creates separate host and guest network
namespaces and a veth pair, starts the same library through the
`linux_host_proxy` example, and installs a deny-first nftables input chain. A guest
CONNECT must reach a controlled loopback echo service through the proxy while
a direct host-veth decoy remains unreachable. The lane then holds a tunnel,
fences the guest link, requires certified zero-active close and no host-side
proxy socket, removes the old veth, and inspects the still-live old namespace
by PID to prove its egress device is absent. Only then does it recreate the same
source IP for a fresh successful lease. Final reconciliation deletes both
named namespaces and proves neither survives.

This is a generic host lifecycle certificate, not sandbox or VMM emulation. It
does not claim IPv6, UDP/DNS, inherited-descriptor, or NAT-port coverage; those
remain named deployment tests in the hardening backlog.
