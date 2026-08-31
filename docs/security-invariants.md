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

## Shutdown result

`Lease::close` consumes the lease. On deadline or coordination failure the
error returns ownership through `CloseError::into_lease`. A successful result
is emitted only after the task tracker is closed and empty. Its counters are
then immutable because no tracked task remains able to change them.

## Protocol claims

Current enforcement covers CONNECT authority and resolved destination IP. It
does not yet inspect ClientHello, enforce visible SNI equality, parse ECH, or
enforce application `Host` authority inside TLS. Documentation and diagnostics
must not imply otherwise.
