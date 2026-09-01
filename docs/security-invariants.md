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
source address is reassigned. The implementation drains while the identity is
revoking, but the host-side fencing order is load-bearing:

1. prevent the old guest from creating traffic;
2. close the lease successfully;
3. only then assign the address to a new guest and attach a new lease.

An integration that cannot guarantee that ordering must use unique per-run
source addresses or a stronger host-authenticated transport identity.

## Admission and policy snapshot

The runtime reserves global and per-lease permits and obtains a task-tracker
token under the lease lifecycle lock. Close takes the same lock, changes the
state to revoking, and closes the tracker. Thus either admission belongs to the
old lease and close waits for it, or admission sees revoking and is refused.
Global and per-lease capacity refusals happen before task creation, do not
increase accepted or active counts, and each increment the owning lease's
denial counter.

## DNS and dialing

Hostname policy is checked before DNS. Every resolved address is checked after
DNS. Only those checked `SocketAddr` values are passed to `TcpStream::connect`;
the dial path never receives the hostname. A process-wide semaphore bounds
lookups executing concurrently; waiting for a permit consumes the same DNS and
absolute handshake deadlines. A cancelled or late resolver future cannot reach
dialing because it lives inside the tracked connection future.

Dialing receives only approved `SocketAddr` values and shares the absolute
handshake deadline. Lease cancellation drops the in-progress connect future;
certified close waits for the owning tracked connection task to disappear.

CONNECT header acquisition has a process-wide byte ceiling and an absolute
deadline capped by the lease handshake deadline. Oversize input, early EOF,
timeout, and other socket read failure remain fail-closed and have distinct
bounded reason codes.

Per-tunnel byte ceilings count bytes read from the guest or upstream. Bytes in
the read that crosses a ceiling are accounted but not forwarded. A ceiling
violation is a policy denial, distinct from an ordinary tunnel I/O failure, and
increments the lease denial counter exactly once.

The forbidden-address floor applies IPv4 rules to both mapped and deprecated
compatible IPv6 forms. The well-known NAT64 `/96` is decoded and the embedded
IPv4 destination is checked. Local-use NAT64, Teredo, 6to4, and non-global
special-purpose IPv6 prefixes are rejected because the effective endpoint is
not safely knowable at this layer. A host can deliberately override the floor
with an explicit CIDR grant.

## Shutdown result

`Lease::close` consumes the lease. On deadline or coordination failure the
error returns ownership through `CloseError::into_lease`. A successful result
is emitted only after the task tracker is closed and empty. Its counters are
then immutable because no tracked task remains able to change them.

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
