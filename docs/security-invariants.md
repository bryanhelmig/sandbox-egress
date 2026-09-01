# Security invariants

The threat model is an untrusted guest that can open arbitrary connections,
send malformed bytes slowly, race shutdown, and control requested hostnames and
DNS answers. The host supervisor and kernel network boundary are trusted. Only
host management handles can enqueue runtime commands; guest sockets cannot
reach that control plane.

## Identity and reuse

`SourceIp` is evidence only when the host ensures the guest cannot choose or
spoof it and cannot bypass the proxy. The supervisor must fence the old
namespace/NAT path before calling `Lease::close`.

Attachment rejects unspecified, multicast, and IPv4 limited-broadcast source
addresses because none can identify the peer of an accepted TCP connection.
Rejection happens before allocating the process-local lease sequence.
IPv4-mapped IPv6 input is first canonicalized, so mapped multicast and
broadcast addresses cannot bypass that validation.

If the host's egress cage exempts trusted proxy sockets with Linux `SO_MARK`,
every untrusted process in that network namespace must lack both
`CAP_NET_ADMIN` and `CAP_NET_RAW`. Since Linux 5.17 either capability can set a
socket mark. Container defaults can retain `CAP_NET_RAW`, and changing the
process UID does not remove an effective capability. This crate does not
install or certify the host cage; the supervisor must verify that boundary.

Only sockets accepted by the shared listener enter a lease. Direct TCP or UDP,
an unrelated loopback or host IPC endpoint, and a socket inherited or passed
to the guest bypass attribution, accounting, and revocation entirely. The host
must deny those paths and prevent guest DNS from becoming an alternate egress
channel. The normative integration checklist is in
[`deployment-contract.md`](deployment-contract.md).

TCP contains no run-generation field. Therefore a shared listener cannot, by
itself, distinguish a deliberately delayed SYN from an old run after the same
source address is reassigned. The implementation keeps the identity revoking
until one complete configured interval passes without an accepted old socket;
each observed arrival restarts that interval. At the end of a candidate quiet
interval, the listener owner drains ready accepts in bounded batches and
rechecks the generation before certifying cleanup. Reaching a batch limit is
not treated as an empty queue. An accept failure returns
`ListenerUnavailable`, retains lease ownership, and cannot certify cleanup.
Attachment independently requires a successful empty ready-queue poll before
installing a replacement mapping, so management command pressure or listener
failure cannot carry an already-queued socket into a new policy.

The ordinary accept loop backs off from 5 milliseconds to at most one second
after consecutive listener errors and resets after a successful accept or
drain. The retry timer remains inside the management select loop, so resource
pressure cannot turn a ready listener error into a CPU spin or prevent close
and shutdown commands from being serviced.

The proxy still cannot authenticate a packet that arrives after the interval,
so the host-side fencing order is load-bearing:

1. prevent the old guest from creating traffic;
2. close the lease successfully;
3. only then assign the address to a new guest and attach a new lease.

An integration that cannot guarantee that ordering must use unique per-run
source addresses or a stronger host-authenticated transport identity.

Successful proxy-wide shutdown drains every tracker and marks each lease
closed before the runtime thread joins. A surviving lease handle may consume
that already-certified final snapshot locally. Send or reply disconnection is
rechecked against closed state, but a deadline timeout still retains ownership;
the shutdown race cannot silently turn uncommitted cleanup into success.

Once proxy-wide shutdown begins, the ordinary listener branch is disabled and
every new attachment returns `ProxyStopping`. A drain barrier may accept a
queued socket only to refuse it under revocation. A deadline failure returns
the owning proxy in this irreversible stopping state. The runtime sends success
through a zero-capacity commit channel and exits only if the waiting caller
receives it; an unobserved certificate remains retryable. Dropping the recovered
handle switches to bounded best-effort teardown and makes no success claim.

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
denial counter. Global admission is fail-fast: it does not promise ordering or
reserved shares between leases. A contending lease can retry after capacity is
released; a two-identity proof requires the retry to be admitted without any
cross-lease accounting.

Unadmitted sockets are closed and their optional denial accounting completes
under the same lifecycle lock used to commit final counters. A socket observed
after the final snapshot is still refused but cannot mutate that snapshot.

## DNS and dialing

Hostname policy is checked before DNS. Every resolved address is checked after
DNS. Only those checked `SocketAddr` values are passed to `TcpStream::connect`;
the dial path never receives the hostname. System lookups append a terminal dot
and therefore cannot apply a local search suffix to the policy authority.
Controlled test resolvers receive that same absolute name, so conformance
cannot accidentally exercise weaker lookup semantics. A process-wide semaphore
bounds lookups executing concurrently; waiting for a permit consumes the same
DNS and absolute handshake deadlines. A cancelled or late resolver future
cannot reach dialing because it lives inside the tracked connection future.
Permit starvation, resolver failure, and deadline enforcement are distinct
bounded denials: `503 dns-capacity`, `502 dns-failed`, and `504 dns-timeout`.
None can start a dial.

Dropping a system lookup also closes Hickory's request completion channel. The
resolver's background transport removes that active request rather than
retrying it. Conformance observes this at the dependency's real UDP and TCP
boundary: after certified close, late failures for both parallel IP queries
cause no new outbound DNS packet or connection. Packets already sent before
revocation cannot be recalled, and a remote resolver may still deliver their
responses; neither event can make the proxy dial or retry after close.

Without explicit servers, resolver construction snapshots the host operating
system configuration at proxy startup. When the host supplies one or more DNS
server socket addresses, construction does not consult the operating system's
resolver configuration or hosts file. Explicit servers use their configured
port over UDP and retry truncated or failed UDP responses over TCP. This is
process configuration fixed before the listener starts; neither a guest nor an
attached lease can choose a resolver. At most eight distinct servers are
accepted, port zero is invalid, and scoped IPv6 server addresses fail startup
because their scope cannot be represented faithfully by the resolver backend.

The shared resolver cache is disabled by default because its dependency bounds
entries rather than bytes. The host may opt into at most 64 responses, each
with at most 24 hours of validity. Cached data is proxy-owned, not lease-owned
work. Every returned address, including a cache hit after identity reuse, is
rechecked under the current lease's immutable policy before it can reach the
connector.

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

A hostname grant authorizes the canonical requested name and then subjects
every answer to the destination floor. It does not authorize an equivalent
numeric request-target. A direct IPv4 or IPv6 literal requires an explicit
network grant, and an overlapping network denial still wins. This keeps a
domain-scoped policy from silently becoming a general public-IP policy.

Numeric-looking legacy host spellings are not trusted as IP literals. If a
host policy deliberately permits one as a hostname, the production resolver
still returns ordinary addresses and the complete result set crosses the same
destination floor before any connector call. A local-wire proof covers dotted
shorthand, leading-zero dotted form, hexadecimal integer, and decimal integer.

The listener's actual post-bind socket address, including its assigned port,
is frozen into process configuration before any connection is dispatched. A
matching literal or DNS result is rejected as `proxy-endpoint-denied` before an
explicit network grant can apply. IPv4-mapped spellings are canonicalized; a
wildcard listener also rejects same-family loopback, and a dual-stack IPv6
wildcard rejects both loopback families. This prevents a guest from nesting
CONNECT requests through the shared listener to multiply lease admissions.
The guard does not enumerate every host interface. A deployment that binds a
wildcard or exposes the listener through another local address must keep that
alias unreachable to proxy-originated dials with its host cage, or bind the
proxy to the concrete guest-facing address.

An immutable policy may explicitly deny destination CIDRs. Denial is checked
before both an explicit network grant and the ordinary public-address behavior,
and it applies identically to DNS results and direct IP literals. Overlapping
configuration therefore fails toward the narrower authority: a denied address
cannot reach the connector through a broader grant or a different authority
spelling. IPv4 denials also match mapped, compatible, well-known NAT64, and
host-configured RFC 6052 forms of the same effective destination.

Resolver-followed aliases do not transfer trust from the allowed original
hostname to their target addresses. Real-wire conformance follows an allowed
CNAME through a seven-link noncyclic chain of separate A and AAAA questions to
a link-local metadata address, then requires `resolved-address-denied` with
zero connector calls. The CONNECT hostname remains the authority rule; every
terminal address still passes the destination floor independently.

A two-name CNAME cycle is also finite. The production resolver follows its
eight-hop bound independently for A and AAAA, producing exactly 16 local wire
questions before `dns-failed`. No address reaches the connector, and close
certifies the denial.

Malformed resolver input cannot fall through to dialing. A real UDP fixture
returns only the query transaction identifier, omitting even the DNS header.
The maintained resolver makes six bounded A/AAAA attempts, after which the
connection receives `dns-failed`; the connector remains untouched and close
certifies the denial.

Approved addresses retain resolver order and are dialed one at a time. Before
each attempt, the remaining absolute handshake time is divided evenly across
the addresses not yet tried. A pending early address therefore cannot consume
the fallback's entire deadline, while one admitted connection still owns at
most one live upstream dial. Immediate failures advance without an artificial
delay. Lease cancellation drops the current attempt and prevents the next.
The fallback loop observes the lease token before every address attempt, so a
refusal that becomes ready after revocation cannot advance to another dial in
the same future poll.

A process-wide semaphore separately bounds connections executing the outbound
dial phase. It is acquired only after the complete resolved-address set passes
policy and remains subject to the connection's absolute handshake deadline.
Capacity expiry is the distinct bounded denial `503 dial-capacity`. The permit
is released immediately after connection establishment, before CONNECT success
or tunnelling, so long-lived tunnels do not consume dial capacity. The permit,
any queued wait, and the current connect future live inside the lease's tracked
connection task; lease cancellation drops all three before close can certify.

Policy construction begins with no allowed hostname, destination network, or
port. Every permitted port is explicit: adding one port cannot retain a hidden
HTTPS default, and adding a hostname cannot create a port grant. The thin
executable chooses port 443 itself; that wrapper convenience is not a library
policy default. Freezing a policy rejects port zero because TCP CONNECT cannot
reach it; an impossible grant never appears to be installed successfully.

Hostname denials are immutable patterns with the same canonical exact and
left-most-wildcard grammar as grants. A matching denial takes priority over
every exact or wildcard grant and is evaluated before DNS capacity or lookup.
A denial without a matching grant cannot open access; it only carves a closed
subset out of the allowlist.

Dialing receives only approved `SocketAddr` values and shares the absolute
handshake deadline. Lease cancellation drops the in-progress connect future;
certified close waits for the owning tracked connection task to disappear.

An optional process-wide upstream proxy changes the transport route, not the
destination decision. It is a host-supplied numeric `SocketAddr`; the guest
cannot select it through a header or environment variable. Sandbox Egress
still resolves and validates the destination locally, then uses its checked IP
and port as the upstream CONNECT authority. The upstream proxy therefore gets
no hostname to resolve again. Its TCP setup, bounded 32 KiB response header,
and CONNECT negotiation live inside the same dial permit, per-address attempt
deadline, tracked connection task, and lease cancellation boundary. A
non-success or malformed response is `upstream-proxy-failed`. Any bytes read
beyond a successful response header are preserved as the first tunnel bytes;
they receive ordinary download accounting and policy ceilings. The configured
upstream proxy may not be the shared Sandbox Egress listener itself.

This route currently supports unauthenticated cleartext HTTP CONNECT only.
Authentication, TLS to the upstream proxy, and host-controlled bypass rules
require explicit designs for secret ownership, trust roots, and preserving the
validated-address guarantee; they are not silently inferred from process or
guest proxy environment variables.

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
policy selectors. Obsolete field folding, control bytes, whitespace-ambiguous
field syntax, and non-ASCII authority text are rejected rather than
normalized. This follows the message requirements and authority-form
reconstruction rules in [RFC 9112 sections 3.2 and
3.3](https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2).
An attached lease named in a guest `X-Run-ID` header receives no traffic unless
the socket's host-observed source address actually maps to that lease.

Configuration and immutable policy construction reject durations too large for
the platform clock to represent as deadlines. The connection path also uses
checked deadline arithmetic, so elapsed startup time or a platform clock edge
cannot turn a trusted configuration mistake into a panicking runtime task.
Every deadline-wrapped operation checks whether its absolute deadline has
already elapsed before polling work, then uses Tokio's maintained timer while
that work is pending. This applies to headers, DNS capacity and lookup, dial
capacity and attempts, the CONNECT success response, initial upload forwarding,
ClientHello inspection, and management close. In particular, a ready operation
cannot begin after an already-expired deadline merely because the timeout
wrapper polls its inner future first.
The process header deadline must also be nonzero. Global connection, DNS, and
dial limits are clamped to Tokio's semaphore maximum, and a per-lease limit
beyond that maximum returns a typed policy error before attachment; extreme
host configuration cannot reach a panicking semaphore constructor.

Per-tunnel byte ceilings count bytes read from the guest or upstream. After
CONNECT establishment, while allowance remains, the metered reader caps its
next read to that remainder, so the proxy forwards exactly the permitted
prefix regardless of kernel read coalescing. The first nonempty read after
exhaustion is accounted but not forwarded. A ceiling violation is a policy
denial, distinct from an ordinary tunnel I/O failure, and increments the lease
denial counter exactly once. At the exact ceiling, a transport error observed
before another successful nonempty read remains a transport error; the proxy
does not invent an excess byte or denial. If it successfully observes an
excess byte first, that byte is counted and the policy denial wins before a
later transport failure. An over-limit upload already coalesced with the
CONNECT header remains an earlier fail-closed case: it is denied in full before
DNS or dialing.

An immutable policy may also set a nonzero tunnel idle timeout. Its clock
starts only after CONNECT success and any configured ClientHello inspection;
handshake work remains governed by the absolute handshake deadline. Every
nonempty read from either tunnel direction resets one shared clock. When the
clock expires, the proxy records one static `tunnel-idle-timeout` denial and
drops the bidirectional copy, stopping both socket directions without waiting
for a peer. Lease cancellation races ahead of that timer and remains the
certifying shutdown boundary. Idle expiry is disabled by default, so callers
must choose a duration appropriate for the applications their run may use.

An ordinary EOF is directional. The proxy propagates it as a write-half
shutdown and continues the reverse copy until that direction also ends. Only
two graceful direction endings count as a completed tunnel. A connection reset
ends the owned task and preserves bytes already read, but does not become a
completion or a policy denial. This applies symmetrically: if the guest resets
while the proxy is forwarding an upstream response, already-read download
bytes remain accounted even when the following write reaches a broken pipe.
An upstream connection refusal happens before CONNECT success and is instead a
bounded `dial-failed` policy denial; the proxy never sends a misleading 200.
Simultaneous asymmetric upload and download preserve both independent byte
totals and count one completion only after both directions end gracefully.

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

The floor is not limited to IANA special-purpose space. Azure WireServer uses
the stable virtual public address `168.63.129.16` for host-platform services,
including VM-agent control traffic and DNS. Because that address looks globally
routable to an ordinary IP classifier, it is denied explicitly before dialing.
Trusted host infrastructure that intentionally needs the endpoint can grant
its `/32`; an untrusted run does not receive it by omission.

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
ClientHello bytes upstream. A ClientHello with more than one hostname is
invalid under [RFC 6066 section 3](https://www.rfc-editor.org/rfc/rfc6066.html#section-3)
and is denied rather than selecting one interpretation. Forwarding the
approved ClientHello is part of the same absolute deadline; upstream
backpressure cannot hold this phase forever. All bytes already read from the
guest are offered to the incremental parser before another socket read, so a
peer EOF cannot discard a complete fragmented hello retained in the bounded
buffer.
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
denial accounting. A public real-socket concurrency case holds a zero-capacity
channel full throughout a 64-connection denial storm, then requires certified
close and exact final counters. The first disconnected send atomically disables
later reporting before rate-state locking or event construction.
