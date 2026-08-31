# egress-lease

`egress-lease` is a Rust library for giving one sandbox run a bounded,
revocable path to the network.

The public model has three objects:

```rust,no_run
use std::{net::{IpAddr, Ipv4Addr}, time::{Duration, Instant}};
use egress_lease::{PeerIdentity, Policy, Proxy, ProxyConfig};

let proxy = Proxy::start(ProxyConfig::default())?;
let policy = Policy::builder()
    .allow_host("example.com")?
    .max_connections(8)?
    .build()?;
let lease = proxy.attach(
    PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    policy,
)?;

let guest_proxy_url = lease.endpoint().to_string();
let live_usage = lease.usage();
let final_usage = lease.close(Instant::now() + Duration::from_secs(2))?;
# let _ = (guest_proxy_url, live_usage, final_usage);
# Ok::<(), Box<dyn std::error::Error>>(())
```

- `Proxy` owns one listener, resolver, runtime, and global connection budget.
- `Policy` is an immutable set of rules for one run.
- `Lease` owns that run's connections, accounting, cancellation, and shutdown.

Successful `Lease::close` is intended to be a certificate: the identity refuses
new connections, tracked header/DNS/dial/tunnel work has ended, sockets have
been dropped, and the returned counters are final. Dropping a lease initiates
cancellation but does not certify completion.

## Current scope

The first slice implements an HTTP/1 CONNECT proxy with:

- source-IP identity supplied by the host-side network boundary;
- exact and left-wildcard hostname allow rules;
- explicit allowed destination ports;
- single-resolution, checked-address, direct-IP dialing;
- private, loopback, link-local, multicast, documentation, and metadata
  destination rejection unless an explicit CIDR grant overrides it;
- global and per-lease connection limits reserved before work is spawned;
- absolute handshake, header, and DNS deadlines;
- bounded headers and upload/download accounting;
- synchronous management handles backed by one owned Tokio runtime;
- explicit, fallible lease and proxy shutdown.

Not yet implemented: visible-SNI matching, ECH policy, plain HTTP forwarding,
transparent interception, rate-limited diagnostics, and configurable resolver
backends. They are tracked in [the roadmap](docs/roadmap.md), and the crate does
not claim those protections today.

## Security boundary

The guest must be kernel-confined so its only network path is the proxy. A
guest-controlled header or token is not a run identity. For
`PeerIdentity::SourceIp`, the supervisor must stop the old namespace/NAT path
before closing and reusing an address; TCP cannot carry a userspace generation
number. See [security invariants](docs/security-invariants.md).

## Development

The ordinary factory is intentionally unsurprising:

```text
./scripts/check.sh              format, compile, lint, tests, docs
./scripts/test-conformance.sh   hostile lifecycle/concurrency suite
./scripts/bench.sh              Criterion microbenchmarks
cargo run --bin egress-lease    small embedding harness
```

Start with [AGENTS.md](AGENTS.md), then read the
[founding context](docs/founding-context.md),
[design brief](docs/design-brief.md), and
[architecture](docs/architecture.md).
Recorded benchmark methodology and results live in
[performance evidence](docs/performance.md).

## Status

Early, pre-release software. The API is being shaped around explicit security
invariants; no compatibility promise is made before `0.1.0` is published.

Licensed under MIT.
