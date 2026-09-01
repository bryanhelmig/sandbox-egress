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

## Data path

The listener uses the socket peer address as host-supplied identity. Lease
attachment and socket acceptance canonicalize an IPv4-mapped IPv6 peer to the
equivalent IPv4 identity before registry lookup. Admission is reserved before
a task is spawned. `httparse` parses a bounded header block. HTTP/1.1 requires
one valid Host field consistent with the CONNECT request-target, but only the
request-target supplies authority to policy, DNS, and dialing. The policy then
checks that authority and port. Hickory performs one async lookup under a
deadline. Its process-wide positive and negative response cache has
host-narrowable count and TTL ceilings. Every result, including a cache hit,
is filtered under the current lease policy, including RFC 6052 decoding under
host-configured network-specific NAT64 prefixes, and Tokio dials a selected
checked IP directly. Approved addresses are tried sequentially; each receives
a fair share of the remaining absolute handshake budget so a pending first
address cannot consume all fallback time or create parallel socket
amplification. An opt-in TLS authority phase incrementally parses a bounded
ClientHello, compares visible SNI with CONNECT authority, and applies the
lease's explicit ECH policy before forwarding those bytes. The ordinary path
does not instantiate the parser. A bounded bidirectional copy loop accounts
bytes.

## Why one package

The library and thin executable begin in one package. Splitting crates now
would manufacture versioning and dependency boundaries before they are known.
Introduce a workspace only when a component has an independently useful API or
dependency graph.
