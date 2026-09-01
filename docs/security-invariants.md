# Security invariants

The threat model is an untrusted guest that can open arbitrary connections,
send malformed bytes slowly, race shutdown, and control requested hostnames and
DNS answers. The host supervisor and kernel network boundary are trusted.

## Identity and reuse

`SourceIp` is evidence only when the host ensures the guest cannot choose or
spoof it and cannot bypass the proxy. The supervisor must fence the old
namespace/NAT path before calling `Lease::close`.

TCP contains no run-generation field. Therefore a shared listener cannot, by
itself, distinguish a deliberately delayed SYN from an old run after the same
source address is reassigned. The implementation keeps the identity revoking
until one complete configured interval passes without an accepted old socket;
each observed arrival restarts that interval. It cannot authenticate a packet
that arrives after the interval, so the host-side fencing order is
load-bearing:

1. prevent the old guest from creating traffic;
2. close the lease successfully;
3. only then assign the address to a new guest and attach a new lease.

An integration that cannot guarantee that ordering must use unique per-run
source addresses or a stronger host-authenticated transport identity.

IPv6 listeners can report an IPv4 peer with the IPv4-mapped IPv6 transport
spelling. Attachment and accepted peers both canonicalize that spelling to
IPv4 before registry lookup. The two spellings therefore cannot hold separate
policies for one effective source address. Other IPv6 forms remain distinct.

## Admission and policy snapshot

The runtime reserves global and per-lease permits and obtains a task-tracker
token under the lease lifecycle lock. Close takes the same lock, changes the
state to revoking, and closes the tracker. Thus either admission belongs to the
old lease and close waits for it, or admission sees revoking and is refused.
Global and per-lease capacity refusals happen before task creation, do not
increase accepted or active counts, and each increment the owning lease's
denial counter.

Unadmitted sockets are closed and their optional denial accounting completes
under the same lifecycle lock used to commit final counters. A socket observed
after the final snapshot is still refused but cannot mutate that snapshot.

## DNS and dialing

Hostname policy is checked before DNS. Every resolved address is checked after
DNS. Only those checked `SocketAddr` values are passed to `TcpStream::connect`;
the dial path never receives the hostname. A process-wide semaphore bounds
lookups executing concurrently; waiting for a permit consumes the same DNS and
absolute handshake deadlines. A cancelled or late resolver future cannot reach
dialing because it lives inside the tracked connection future.

DNS address cardinality is process-configured and has a hard upper bound. The
system resolver collects at most one entry beyond that ceiling, solely to
detect overflow. An oversized answer is rejected as a whole before address
policy or dialing; response ordering cannot select a truncated subset for the
dialer. This bounds the proxy's collected address vector and dial attempts; the
resolver still necessarily parses the DNS message it receives.

Every address is policy-checked before the answer can reach the dialer. After
that full-set validation, duplicate approved socket addresses are collapsed in
first-seen order. Repeated records therefore consume one bounded answer slot
each but cannot amplify sequential connection attempts.

Approved addresses retain resolver order and are dialed one at a time. Before
each attempt, the remaining absolute handshake time is divided evenly across
the addresses not yet tried. A pending early address therefore cannot consume
the fallback's entire deadline, while one admitted connection still owns at
most one live upstream dial. Immediate failures advance without an artificial
delay. Lease cancellation drops the current attempt and prevents the next.

Policy construction begins with no allowed hostname, destination network, or
port. Every permitted port is explicit: adding one port cannot retain a hidden
HTTPS default, and adding a hostname cannot create a port grant. The thin
executable chooses port 443 itself; that wrapper convenience is not a library
policy default.

Dialing receives only approved `SocketAddr` values and shares the absolute
handshake deadline. Lease cancellation drops the in-progress connect future;
certified close waits for the owning tracked connection task to disappear.

CONNECT header acquisition has a process-wide byte ceiling and an absolute
deadline capped by the lease handshake deadline. Both deadlines begin when the
listener accepts the socket, so time awaiting the spawned connection task does
not extend either budget. Oversize input, early EOF, timeout, and other socket
read failure remain fail-closed and have distinct bounded reason codes. The
mature parser also uses a fixed 64-header slot array;
header 65 is rejected as `too-many-headers` rather than allocating more space
or being mislabeled as malformed syntax. Header terminator search scans only
new bytes plus the three-byte boundary overlap, so raising the trusted byte
ceiling does not give a guest quadratic parser work.

CONNECT authority parsing requires a nonzero decimal port. Square brackets are
accepted only around a value that parses as IPv6; bracketed DNS names, IPv4,
IPvFuture, and scoped-zone forms fail as `invalid-ipv6-literal` rather than
being stripped and reinterpreted as another host class.

The CONNECT request-target is the sole authority input. HTTP/1.1 requests must
contain exactly one syntactically valid Host field whose hostname agrees with
the target; if Host supplies a port, that port must also agree. A missing,
duplicate, malformed, or conflicting field is rejected before policy or DNS.
HTTP/1.0 may omit Host. Host and all other guest headers are never identity or
policy selectors. This follows the message requirements and authority-form
reconstruction rules in [RFC 9112 sections 3.2 and
3.3](https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2).

Configuration and immutable policy construction reject durations too large for
the platform clock to represent as deadlines. The connection path also uses
checked deadline arithmetic, so elapsed startup time or a platform clock edge
cannot turn a trusted configuration mistake into a panicking runtime task.
The process header deadline must also be nonzero. Global connection and DNS
limits are clamped to Tokio's semaphore maximum, and a per-lease limit beyond
that maximum returns a typed policy error before attachment; extreme host
configuration cannot reach a panicking semaphore constructor.

Per-tunnel byte ceilings count bytes read from the guest or upstream. Bytes in
the read that crosses a ceiling are accounted but not forwarded. A ceiling
violation is a policy denial, distinct from an ordinary tunnel I/O failure, and
increments the lease denial counter exactly once.

Cumulative connection and byte counters use saturating atomic updates. They
remain monotonic at the integer boundary instead of wrapping, and byte
accounting cannot panic while forming the returned total. The active-connection
gauge is separate: it is bounded by admission permits and decrements when owned
work ends.

The forbidden-address floor applies IPv4 rules to both mapped and deprecated
compatible IPv6 forms. The well-known NAT64 `/96` is decoded and the embedded
IPv4 destination is checked. A host using an RFC 6052 network-specific NAT64
prefix must register that routed prefix in `ProxyConfig`; all six standard
prefix lengths are decoded, and a translated private or metadata destination
is denied before dialing. Unknown local-use NAT64, Teredo, 6to4, and non-global
special-purpose IPv6 prefixes are rejected because the effective endpoint is
not safely knowable at this layer. A host can deliberately override the floor
with an explicit CIDR grant.

NAT64 prefix knowledge belongs to the trusted proxy configuration, not the
guest or a run policy. Registering a network-specific prefix says how the host
network interprets matching IPv6 addresses; it does not allow a forbidden
embedded IPv4 destination. An arbitrary global IPv6 address is otherwise
treated as native because its syntax alone cannot identify an operator's
translation route.

The conservative floor rejects IANA's full `2001::/23` IETF protocol
assignments umbrella, as it does IPv4's `192.0.0.0/24` umbrella. This includes
more-specific special assignments that a deployment might intentionally use;
those require an explicit CIDR grant. Unassigned children cannot become an
allow-by-omission gap as the registry evolves.

After separately decoding IPv4-mapped, deprecated IPv4-compatible, and
well-known NAT64 forms, a native IPv6 destination must be inside IANA's
`2000::/3` global unicast block. Reserved address-space blocks are denied by
shape rather than by an incomplete enumeration. Four special-purpose ranges
inside `2000::/3` remain explicitly denied. As with every floor rule, a trusted
host can override this deliberately with an explicit CIDR grant.

## Shutdown result

`Lease::close` consumes the lease. On deadline or coordination failure the
error returns ownership through `CloseError::into_lease`. A successful result
is emitted only after the task tracker is closed and empty, one complete
queued-socket drain interval passes without another observed arrival, and the
state becomes internally quiesced under the lifecycle lock. Its counters are
then immutable: tracked work is gone and
later unadmitted sockets do not count. Quiescing alone does not make the
identity reusable; if success delivery is lost, the lease retains ownership
until a retry is observed successfully. That retry returns the already-frozen
snapshot without repeating cleanup or the identity-reuse quiet period.

## Protocol claims

Every policy enforces CONNECT authority and resolved destination IP. TLS
authority inspection is opt-in. When enabled, a maintained TLS parser must
accept a complete ClientHello within both the configured byte bound and the
lease's absolute handshake deadline. The proxy requires one canonical visible
SNI hostname equal to the canonical CONNECT hostname before forwarding any
ClientHello bytes upstream. Forwarding the approved ClientHello is part of the
same absolute deadline; upstream backpressure cannot hold this phase forever.
When TLS authority inspection is disabled, any tunnel bytes coalesced with the
CONNECT header are also forwarded within that deadline before the connection
enters ordinary bidirectional tunnelling.

Strict TLS authority mode rejects an ECH extension because the inner authority
is encrypted. `AllowOuterSni` is an explicit compatibility tradeoff: it checks
the outer SNI but cannot make a claim about the inner name. The proxy does not
terminate TLS and cannot enforce an application `Host` or `:authority` value
inside the encrypted tunnel. Documentation and diagnostics must not imply
otherwise.

ClientHello inspection happens after the CONNECT destination has resolved and
the checked socket has connected, because a conventional proxy client waits
for the 200 response before sending TLS. A denied ClientHello sends zero tunnel
bytes upstream, but the upstream TCP connection has already occurred.

## Diagnostics

Diagnostics are disabled by default. When configured, every lease-owned policy
or capacity denial increments accounting before attempting delivery. Events
contain only the proxy-assigned lease sequence, host-authenticated peer
identity, a crate-owned static reason code, and a process-wide suppression
count; guest-provided authority text is never copied into an event. The lease
sequence distinguishes an old queued event after its source IP is reassigned.
Lease attachment fails explicitly if the process-local sequence is exhausted;
it never wraps into an earlier run's diagnostic identity.

Delivery uses `SyncSender::try_send`, so a full or disconnected caller-owned
channel cannot block proxy work. A process-wide one-second window bounds
attempted delivery, and the configured rate has a hard ceiling. Rate- and
channel-suppressed events accumulate with saturation and are reported on the
next event the channel accepts. Diagnostic loss never weakens enforcement or
denial accounting.
