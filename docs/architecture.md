# Architecture

The crate is split by responsibility, not protocol fashion:

```text
synchronous caller
  Proxy ───────── management channel ───────┐
  Lease ─ usage atomics / close request ────┤
                                            v
                                  one owned Tokio runtime
                                  listener + resolver
                                            |
                       source IP -> immutable LeaseState
                                            |
       admit -> track -> headers -> DNS -> dial -> optional ClientHello -> tunnel
                                            |
                                  counters + cancellation
```

## Core objects

`Proxy` owns the runtime thread. `Proxy::attach` is a synchronous command that
atomically installs an immutable `Policy` for an unused `PeerIdentity`.
`Proxy::start` does not detach that ownership on failure: if the spawned
runtime reports a resolver, bind, or post-bind validation error, the caller
gets the error only after the runtime thread has exited and been joined.
Policy construction canonicalizes, sorts, and deduplicates its owned rules.
Frozen ports are a contiguous sorted vector with binary-search lookup; the
runtime never retains builder-only tree nodes or mutable rule indexes.

`LeaseState` is shared internally but not exposed. Its lifecycle lock orders
admission against revocation. A Tokio cancellation token ends async phase work;
a `TaskTracker` turns task destruction into the close barrier. Counters are
atomics so `Lease::usage` does not cross the runtime boundary.

The internal lifecycle is `Open -> Revoking -> Quiesced -> Closed`. Quiescing
commits final counters under the lifecycle lock after tracked work ends and one
complete queued-socket drain interval passes without an observed arrival. Each
revoking-phase arrival restarts that interval. Quiescing does not release
identity ownership;
only the synchronous caller's observation of close success advances to
`Closed` and sends the registry release. A retry can certify an already
quiesced snapshot immediately; it does not rerun the quiet-period barrier.

`Lease` is intentionally not `Clone`. `close(self, deadline)` either produces
`FinalUsage` or a `CloseError` containing the still-owning lease.

The management command channel is an unbounded trusted-control-plane queue;
guest sockets cannot enqueue into it. Retaining that shape is deliberate:
`Lease::drop` must initiate cleanup without blocking, and silently discarding a
cleanup command from a full bounded queue could strand identity ownership.
Host integrations should still bound their own concurrent management calls. An
opt-in resource lane releases 64 simultaneous attaches and closes in repeated
batches and verifies process threads and descriptors recover.

Listener accept errors use a bounded retry delay without sleeping the runtime
thread, so management commands remain responsive during descriptor pressure.
A failure while performing the identity handoff drain is different from an
empty queue: close or replacement attachment fails explicitly and the old
identity remains owned.

A successful proxy-wide shutdown drains every lease tracker and marks each
state closed before joining the runtime thread. A surviving lease handle reads
that immutable closed snapshot locally; runtime loss alone cannot hide an
already-committed certificate.

The proxy-wide stopping state is irreversible. The listener owner disables its
ordinary accept branch, rejects every attachment command, and services shutdown
retries. A drain barrier may still accept queued sockets solely to refuse them
under revoking lease state. A failed call returns `ShutdownError` with the
owning `Proxy`. Success uses a synchronous rendezvous: the runtime exits only
after its caller receives the certificate, so an unobserved reply leaves a live
stopping handle rather than a disconnected one. `Proxy::drop` uses a distinct
best-effort request and may abandon certification after its deadline.

Optional diagnostics use a caller-owned bounded synchronous channel. Proxy
tasks call only `try_send`; they never wait for a logger or spawn a logging
thread. One process-wide fixed-window limiter records rate- and
channel-suppressed events on the next delivered event.

Resolver construction and lookup live in one small internal module. It owns
the distinction between host-system and explicitly pinned recursive servers,
cache and transport options, absolute-name lookup, and bounded answer
collection. The proxy lifecycle owns deadlines, cancellation, address policy,
and dialing. This boundary is internal: it does not create a guest-selectable
backend or another public core object.

## Data path

The listener uses the socket peer address as host-supplied identity. Lease
attachment and socket acceptance canonicalize an IPv4-mapped IPv6 peer to the
equivalent IPv4 identity before registry lookup. Admission is reserved before
a task is spawned. One internal incremental framer enforces the byte ceiling
and terminator boundary for both guest CONNECT requests and upstream-proxy
responses; a compile-time chunk size lets the two callers retain distinct
limits and exact read-buffer footprints without duplicating source.
`httparse` parses each bounded header block. HTTP/1.1 requires
one valid Host field consistent with the CONNECT request-target, but only the
request-target supplies authority to policy, DNS, and dialing. The policy then
checks hostname denials, grants, and port before DNS. Hickory performs one
async lookup under a deadline. Its process-wide positive and negative response
cache is disabled by default and has host-narrowable count and TTL ceilings
when enabled. Every result, including a cache hit, is filtered under the
current lease policy,
including RFC 6052 decoding under host-configured network-specific NAT64
prefixes. Explicit destination denials take priority over grants and default
public-address handling. The actual listener endpoint is also rejected before
any explicit grant, preventing recursive CONNECT chains through the proxy
itself. Tokio then dials a selected checked IP directly. When the host
configures an upstream HTTP proxy, the same connector instead dials that
numeric proxy address and sends CONNECT for the selected checked `SocketAddr`;
the destination hostname never crosses that boundary for another lookup. A
bounded mature-parser response phase remains inside the dial and absolute
handshake budgets, and any bytes coalesced after its successful header are
retained for the tunnel.
Approved addresses are tried sequentially. Each receives
a fair share of the remaining absolute handshake budget so a pending first
address cannot consume all fallback time or create parallel socket
amplification. An opt-in TLS authority phase incrementally parses a bounded
ClientHello, compares visible SNI with CONNECT authority, and applies the
lease's explicit ECH policy before forwarding those bytes. The CONNECT success
write and any initial tunnel bytes remain inside the same absolute handshake
deadline. The ordinary path does not instantiate the parser. A bounded
bidirectional copy loop accounts bytes. An optional policy idle timeout gives
the two metered readers one shared activity clock. Either upload or download
resets it; no timer task is spawned, and the default path allocates no activity
channel. Idle expiry drops the copy future and its owned sockets together, so
it does not wait for either remote endpoint.

## Why one publishable package

The library, thin executable, and Linux conformance example remain one
publishable package. The example is test infrastructure, not another proxy
implementation or public API. Splitting the implementation into crates now
would manufacture versioning and dependency boundaries before they are known.
Introduce another package only when a component has an independently useful
API or dependency graph.
