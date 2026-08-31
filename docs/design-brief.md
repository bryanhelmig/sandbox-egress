# Original design brief

This file preserves the founding input. Later designs may refine mechanisms,
but must explicitly account for these requirements.

This is repository-owned source material, not a summary that depends on chat
history. Do not delete or silently weaken it. If implementation experience
forces a different guarantee, add a design record, update the public claim,
and retain the original requirement here with a link to that decision.

The motivating integration boundary is the existing WarmVM network lifecycle
at `work/sandbox-runtime/warmvm/warmvmd/src/network.rs:31`. The crate should
drop into a Rust sandbox supervisor without changing that tick lifecycle. The
broader deployment model includes local sandboxes, microVM services,
Lambda-like systems, and E2B-like systems.

## One proxy, one lease per run

```rust,ignore
let proxy = Proxy::start(config)?;
let lease = proxy.attach(
    PeerIdentity::SourceIp(run_egress_ip),
    policy,
)?;
let guest_proxy_url = lease.endpoint();
let usage = lease.usage();
let final_usage = lease.close(deadline)?;
```

`Proxy` owns the shared listener, resolver, runtime, and global budgets.
`Policy` contains immutable rules for this run. `Lease` owns the run's
connections, accounting, and shutdown. The management API is synchronous and
does not require a sandbox daemon to become async.

## Closing the lease is the strongest guarantee

Successful close means no more traffic, pending dial, or DNS work holding the
lease, and it returns final counters. Close must:

- refuse new connections immediately;
- cancel headers, DNS, ClientHello, connecting, and tunnelling phases;
- stop both socket directions without waiting for the remote endpoint;
- prevent late DNS answers from starting a connection after revocation;
- retain ownership on failure so an identity cannot accidentally be reused.

Drop may initiate cancellation, but cannot certify cleanup.

In particular, cancellation covers work while reading request headers,
resolving DNS, parsing ClientHello, connecting, uploading, downloading, or
tunnelling. A successful close does not wait for a cooperative remote peer to
close its half of a socket.

## Identity is supplied by the host

The intended identity is the source address enforced by the namespace/NAT
boundary. There is no guest-supplied run header or policy selector. Address
reuse must not let queued work from an old run inherit a new policy.

## Opinionated, bounded handling

- Mature HTTP and TLS parsers.
- Explicit DNS deadlines and bounded resolver concurrency.
- Resolve, reject forbidden addresses, then dial the approved address directly.
- Per-run and global connection limits reserved before spawning work.
- Absolute handshake deadlines, not only idle timeouts.
- Bounded buffers, backpressure, upload/download accounting.
- Structured denial reasons and rate-limited diagnostics.
- A precise authority promise: CONNECT authority plus visible SNI unless the
  proxy actually enforces application authority. ECH behavior is explicit; SNI
  inspection alone is not represented as preventing domain fronting.

## Embeddable, with one implementation

The async core sits behind a synchronous handle and one proxy-owned runtime.
The same library also powers a thin executable, permitting later process
isolation without a second implementation.

## Hostile conformance suite

The repository should prove revocation in every phase, DNS rebinding and
forbidden-address rejection, slow/malformed handshakes, ECH behavior, shutdown
during uploads/downloads, identity reuse isolation, and stable
memory/thread/file-descriptor use under abuse.

Concurrency, CPU, memory, thread, and descriptor measurements are part of the
deliverable, not optional polish. The factory should make regressions visible
with ordinary, documented commands.

## Project ambition

This should become a small, public, dependable crate that drops into microVM,
Lambda-like, E2B-like, and local sandbox services. Simplicity is a feature.
The repository should feel like a software factory: familiar commands, fast
feedback, reproducible tests, visible performance, durable context, and enough
guidance that many concurrent contributors climb the same hills.
