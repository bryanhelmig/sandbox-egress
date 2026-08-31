# Sandbox Egress

Run-scoped network access for untrusted sandboxes.

Sandbox Egress is an embeddable Rust proxy that lets a host give each sandbox
run its own outbound network policy, connection budget, usage counters, and
explicit shutdown boundary. It is inspired by Stripe Smokescreen and the
host-side proxies used by microVM, code-execution, and agent sandboxes.

It is a library first. A small executable wraps the same implementation for
local use and for deployments that later want a separate, resource-capped
proxy process.

## Where it fits

If you are building a “safe jail” for untrusted code, this handles one specific
part: controlled access from the jail to the network.

```text
                         trusted host
                              |
                    one shared Proxy process
                              |
          source IP ----------+---------- immutable Policy
                              |
                            Lease
                 connections / usage / shutdown
                              |
                         allowed network
```

Sandbox Egress is not the whole jail. The host must still isolate processes,
files, memory, and syscalls, and must use a namespace, firewall, NAT boundary,
or equivalent mechanism so the guest cannot bypass the proxy with a direct
socket. Merely setting `HTTP_PROXY` is not a security boundary.

## The three-object model

- `Proxy` owns the shared listener, resolver, runtime, and global connection
  budget.
- `Policy` is an immutable set of outbound rules for exactly one run.
- `Lease` owns that run’s identity, admitted work, accounting, cancellation,
  and certified shutdown.

```rust,no_run
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proxy = Proxy::start(ProxyConfig::default())?;
    let policy = Policy::builder()
        .allow_host("api.example.com")?
        .allow_host("*.static.example.com")?
        .allow_port(443)
        .max_connections(8)?
        .build()?;

    // The host network boundary, not the guest, establishes this identity.
    let run_egress_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let lease = proxy.attach(PeerIdentity::SourceIp(run_egress_ip), policy)?;

    // Expose this URL inside the guest as its HTTP/HTTPS proxy.
    let guest_proxy_url = lease.endpoint();
    let live_usage = lease.usage();
    println!("proxy={guest_proxy_url} usage={live_usage:?}");

    // First prevent the guest from opening new sockets. Then certify cleanup.
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(2))?
        .usage();
    println!("final usage={final_usage:?}");

    proxy.shutdown(Instant::now() + Duration::from_secs(2))?;
    Ok(())
}
```

The management API is synchronous. The proxy owns one Tokio runtime; embedding
it does not require converting an existing supervisor to async or creating a
runtime per sandbox run.

## Why a lease?

Starting a connection and ending a run are concurrent events. A connection may
be reading headers, waiting on DNS, dialing an address, or moving bytes when
the host decides the run is over. Dropping an ordinary proxy handle does not
prove that this work is gone.

`Lease::close` consumes the lease and is fallible. A successful close means:

- new connections for the identity are refused;
- tracked header, DNS, dial, and tunnel work has ended;
- both socket directions have been stopped without waiting for the remote;
- no late DNS result can start a connection;
- the returned usage counters are final.

If the deadline expires, `CloseError::into_lease` returns the still-owning
lease. The identity remains unavailable, so a supervisor cannot accidentally
assign a new run to work left behind by the old one. `Drop` starts best-effort
cancellation, but never certifies cleanup.

## Current enforcement

The current vertical slice provides:

- HTTP/1 CONNECT authority and destination-port allow rules;
- exact hostnames and left-most wildcard subdomains;
- source-IP identity derived from the accepted socket;
- one DNS resolution followed by checks on every returned address;
- direct dialing of a checked `SocketAddr`, with no second lookup;
- an independently bounded process-wide DNS concurrency budget;
- default rejection of loopback, private, link-local, multicast,
  documentation, and cloud-metadata destinations unless a CIDR is explicitly
  granted;
- global and per-lease connection admission reserved before work is spawned;
- bounded request headers, backpressure, absolute handshake and DNS deadlines;
- upload/download accounting and optional transfer ceilings;
- explicit lease and proxy shutdown deadlines.

The policy promise today is CONNECT authority plus resolved destination IP.
Sandbox Egress does not yet inspect TLS `ClientHello`, compare visible SNI, or
define an ECH enforcement mode. It therefore does not claim to prevent domain
fronting. Plain HTTP forwarding, transparent interception, configurable
resolver backends, and rate-limited structured diagnostics are also not yet
implemented. These gaps are tracked rather than hidden.

## Safe integration order

For a source-IP identity, the host should use this lifecycle:

1. Give the run a unique or currently unused source address.
2. Attach its immutable policy and route its only egress path through the
   proxy.
3. Run the untrusted workload.
4. Fence the old namespace or NAT path so it cannot create more traffic.
5. Close the lease successfully.
6. Only then reuse that source address for another run.

TCP does not carry a userspace run generation. The shared listener cannot tell
a deliberately delayed packet from an old owner after the host reassigns the
same address. Host-side fencing and ordering are therefore part of the security
contract.

## Development

The ordinary development loop uses familiar Cargo commands behind small
scripts:

```text
./scripts/check.sh              format, compile, lint, test, docs, package
./scripts/test-conformance.sh   hostile lifecycle and concurrency cases
./scripts/bench.sh              Criterion performance baseline
./scripts/measure-resources.sh  opt-in RSS, thread, and descriptor soak
./scripts/measure-complexity.sh source size and complexity trend report
cargo run --bin sandbox-egress -- example.com
```

To reproduce the MSRV factory in a clean Linux environment:

```text
docker build -t sandbox-egress:dev .
docker run --rm sandbox-egress:dev
```

The image is pinned to Rust 1.88, runs the normal factory plus a small Linux
resource smoke while building, and runs the hostile conformance lane by
default. Tests remain local and do not call public network services.

Start with [AGENTS.md](AGENTS.md). The deeper project record is split into:

- [founding context](docs/founding-context.md) — product ambition and audience;
- [design brief](docs/design-brief.md) — the original lifecycle requirements;
- [security invariants](docs/security-invariants.md) — claims and trust boundary;
- [architecture](docs/architecture.md) — internal ownership and data flow;
- [testing strategy](docs/testing.md) — conformance and resource evidence;
- [performance evidence](docs/performance.md) — reproducible measurements;
- [complexity evidence](docs/complexity.md) — source and decision-shape trends;
- [engineering log](docs/engineering-log.md) — experiments and negative results;
- [hardening backlog](docs/hardening-backlog.md) — attack and measurement matrix;
- [prior art](docs/prior-art.md) — reviewed projects and pinned revisions;
- [roadmap](docs/roadmap.md) — known gaps and release gates.

## Status

Sandbox Egress is early, pre-release software. The core lifecycle works and is
tested, but the hostile conformance matrix and protocol enforcement are not yet
complete. There is no compatibility promise before the first published
release.

Licensed under MIT.
