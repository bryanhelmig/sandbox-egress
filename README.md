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
                        one shared Proxy
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
socket. Merely setting `HTTP_PROXY` or `HTTPS_PROXY` is not a security
boundary.

If that boundary exempts trusted proxy sockets with Linux `SO_MARK`, remove
both `CAP_NET_ADMIN` and `CAP_NET_RAW` from every untrusted workload and
sidecar in the network namespace. Since Linux 5.17, either capability can set
the mark; Docker retains `CAP_NET_RAW` by default. Running as a non-root UID
does not replace this capability check.

The complete [deployment contract](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/deployment-contract.md) divides the
guarantees owned by this crate from the direct TCP, UDP, DNS, inherited-socket,
and host-IPC confinement that the sandbox must own. That checklist is the
right starting point before calling a deployment safe.

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
        .deny_host("admin.static.example.com")?
        .allow_port(443)
        // Optional: block a destination CIDR even when its hostname is allowed.
        .deny_network("93.184.216.0/24".parse()?)
        // Optional: require visible TLS SNI to repeat the CONNECT hostname.
        .require_tls_sni()
        // Optional: release a tunnel that moves no bytes in either direction.
        .idle_timeout(Duration::from_secs(60))
        .max_connections(8)?
        // Optional: bound rapid terminal connection churn as well as live work.
        .connection_attempt_rate(32, 8)?
        .build()?;

    // Attach the source address the listener observes. The host network
    // boundary, not guest configuration, establishes this identity.
    let run_egress_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let lease = proxy.attach(PeerIdentity::SourceIp(run_egress_ip), policy)?;

    // Expose this HTTP CONNECT proxy to an HTTPS client. For clients using
    // standard proxy environment variables, this is normally HTTPS_PROXY.
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

`lease.endpoint()` reports the address actually bound by the shared listener.
With a concrete bind, that can be exposed directly as in the example. With a
wildcard bind, it contains `0.0.0.0` or `::`; the host must combine its assigned
port with the reachable proxy address for each guest network. The library does
not guess routing topology or add a second advertised-address authority.

`Policy::builder()` is deny-by-default across every rule dimension. Adding a
hostname does not add a port, and adding port 80 does not silently retain port
443. Explicit hostname denials override exact and wildcard grants; network
denials override both grants and the ordinary public-IP behavior. The example
permits 443 explicitly; the thin executable makes the same HTTPS-only choice
on behalf of its intentionally smaller command-line surface.

Hostname and port grants are independent. Allowing hosts `api.example.com` and
`database.example.com` plus ports 443 and 5432 permits all four combinations,
not just `api:443` and `database:5432`. The current API deliberately does not
express per-host port associations. If those cross-combinations would be an
overgrant, do not approximate that policy with this release.

Hostname rules authorize names, not arbitrary numeric authorities. An allowed
hostname may resolve to an otherwise ordinary public address, but a direct IP
literal requires an explicit `allow_network` grant. Both paths still honor
network denials, translated-address checks, and the proxy-endpoint guard.

Byte limits apply separately to each tunnel; `Usage` totals bytes across the
whole lease. A new connection gets a fresh byte allowance. Process connection,
DNS, and dial capacities reject zero or oversized values at startup.

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

A successful `Proxy::shutdown` certifies all attached leases before its runtime
thread joins. A still-held lease can call `close` afterward to consume that
committed certificate and retrieve its final counters without a live runtime.
If the proxy-wide deadline expires, `ShutdownError::into_proxy` returns the
still-stopping proxy for retry. It no longer admits socket work or accepts new
leases. Listener-drain barriers may accept queued sockets only to refuse them
under revocation. The runtime exits only after the caller observes a success
certificate; losing a reply race cannot turn a recoverable handle into a dead
one. Dropping that handle remains best-effort and does not certify cleanup.

## Current enforcement

The current vertical slice provides:

- HTTP/1 CONNECT request-target authority and destination-port allow rules,
  with strict HTTP/1.1 Host-field validation but no header-selected policy;
- canonical ASCII hostnames (including explicit ACE/punycode spellings) and
  wildcard suffixes matching one or more subdomain labels, with explicit
  deny-overrides-grant carve-outs; raw Unicode is not mapped implicitly;
- source-IP identity derived from the accepted socket;
- one DNS resolution followed by checks on every returned address;
- a resolver cache disabled by default because the dependency bounds entries,
  not bytes; the host may explicitly enable up to 64 responses with a 24-hour
  TTL ceiling;
- optional host-pinned concrete-unicast recursive DNS servers with UDP plus
  truncated-response TCP recovery, independent of host resolver and hosts-file
  changes;
- bounded DNS answer cardinality, with oversized sets rejected before dialing;
- direct dialing of a checked `SocketAddr`, or host-configured HTTP CONNECT
  chaining using that numeric address, with no second lookup;
- rejection of the proxy's own concrete listener endpoint before any explicit
  network grant; wildcard-bound proxies conservatively reject the listener
  port at every address, preventing another local interface from becoming a
  nested CONNECT path;
- listener configuration limited to wildcard or unicast addresses, with an
  explicit zone required for scoped IPv6;
- sequential address failover with a fair share of the remaining absolute
  handshake budget per attempt, keeping one live dial per connection;
- an independently bounded process-wide DNS concurrency budget;
- a separate process-wide outbound-dial budget, acquired only after every
  resolved address is approved and released before tunnelling begins;
- default rejection of loopback, private, link-local, multicast,
  documentation, cloud-metadata, reviewed provider control-plane endpoints,
  and unsafe IPv6 transition destinations unless a CIDR is explicitly granted;
- per-policy destination CIDR denials that take priority over explicit grants
  and the ordinary public-address behavior, including mapped, compatible, and
  configured NAT64 forms of a denied IPv4 destination;
- RFC 6052 decoding for the well-known NAT64 prefix and any operator-registered
  network-specific NAT64 prefixes, so translated private and metadata IPv4
  destinations receive the same checks;
- fail-fast global and per-lease connection admission reserved before work is
  spawned, plus optional token buckets for rapid connection-attempt churn,
  with refusals attributed to the contending lease;
- bounded request headers, rejection of CONNECT `Content-Length` and
  `Transfer-Encoding` framing, backpressure, and absolute accept-to-handshake
  and DNS deadlines; waiting for DNS or dial capacity and writing the CONNECT
  success response consume those deadlines;
- opt-in, bounded TLS `ClientHello` parsing that requires visible SNI to equal
  the CONNECT hostname;
- explicit ECH handling: strict inspection rejects ECH by default, while an
  `AllowOuterSni` mode is available for integrations that knowingly accept an
  unverifiable encrypted inner name;
- upload/download accounting and optional transfer ceilings;
- an optional per-run tunnel idle timeout, reset by bytes moving in either
  direction and disabled by default;
- opt-in structured denial events with process-wide rate limiting and
  nonblocking bounded-channel delivery;
- deadline-bounded CONNECT success and best-effort, nonblocking denial
  responses, so an unread diagnostic cannot retain a run's connection;
- explicit lease and proxy shutdown deadlines.

The CONNECT request-target is the authority input. HTTP/1.1 requires exactly
one valid Host field that agrees with that target, but Host and every other
guest header are validation-only and can never select identity or policy. The
default policy promise remains CONNECT authority plus resolved destination IP.
Calling `PolicyBuilder::require_tls_sni` opts a lease into the stricter
promise: the first tunnel bytes must be a valid, bounded `ClientHello`, its
visible SNI must equal the CONNECT hostname, and ECH must be absent. IP-literal
CONNECT requests cannot satisfy this mode. `ProxyConfig` bounds buffered
`ClientHello` bytes, and the lease's absolute handshake deadline covers the
entire inspection.

For clients that use ECH, callers can explicitly select
`TlsAuthority::RequireVisibleSni { ech: EchPolicy::AllowOuterSni }`. That mode
checks only the visible outer SNI. It cannot know the encrypted inner name.
Neither mode terminates TLS or checks the application authority inside the
encrypted tunnel, so Sandbox Egress does not claim to eliminate every form of
domain fronting. Plain HTTP forwarding, transparent interception, arbitrary
resolver backends, and configurable destination-range tables remain outside
the current core. They are tracked as research or integration candidates, not
promised crate features.

Global connection, resolver, and outbound-dial work are bounded independently.
The defaults are 256 admitted connections, 32 concurrent DNS lookups, and
256 concurrent dials; a host can narrow or widen each ceiling before startup:

```rust,no_run
# use sandbox_egress::ProxyConfig;
let config = ProxyConfig::default()
    .with_max_connections(512)
    .with_connection_attempt_rate(2_000, 250)
    .with_max_concurrent_dns(64)
    .with_max_concurrent_dials(128);
# Ok::<(), Box<dyn std::error::Error>>(())
```

By default the proxy snapshots the host's resolver configuration when it
starts. A sandbox supervisor can instead pin one or more recursive servers;
explicit mode never reads the hosts file or host resolver configuration and
uses each configured port for both UDP and TCP:

```rust,no_run
# use sandbox_egress::ProxyConfig;
let config = ProxyConfig::default()
    .with_dns_server("10.0.0.2:53".parse()?)
    .with_dns_server("10.0.0.3:53".parse()?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

This is trusted process configuration, not a per-lease or guest-selected
resolver. Up to eight distinct servers are accepted. Unspecified, multicast,
broadcast, and scoped IPv6 server addresses are rejected, including forbidden
IPv4 classes written as IPv4-mapped IPv6. A recursive server
must be a concrete unicast endpoint, and the underlying resolver cannot
preserve an IPv6 scope identifier. It also cannot point back at the shared
Sandbox Egress listener.

Corporate networks can route every approved destination through one
operator-controlled HTTP CONNECT proxy. Supply its numeric socket address in
process configuration; Sandbox Egress still resolves and checks the guest's
destination locally, then sends only the approved IP and port upstream:

```rust,no_run
# use sandbox_egress::ProxyConfig;
let config = ProxyConfig::default()
    .with_upstream_proxy("10.0.0.10:3128".parse()?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

The upstream endpoint must also be concrete unicast. IPv4-mapped endpoints use
the same class boundary as native IPv4. A scoped IPv6 endpoint is accepted only
when its socket address includes the required zone identifier.

This first slice is intentionally narrow: plain HTTP to the upstream proxy,
no authentication, no bypass list, and no hostname-selected CONNECT mode. The
upstream response header is bounded to 32 KiB and parsed with `httparse`; a
non-2xx response becomes the stable `upstream-proxy-failed` denial. TCP setup,
CONNECT negotiation, and any queued wait all consume the existing dial and
absolute handshake budgets. The guest cannot select or override this route.

Resolver caching is also a host decision. It is off by default because one DNS
response can contain many records even though Hickory counts it as one cache
entry. A host that accepts that memory tradeoff can enable a small shared cache:

```rust,no_run
# use sandbox_egress::ProxyConfig;
# use std::time::Duration;
let config = ProxyConfig::default().with_dns_cache(32, Duration::from_secs(60));
# Ok::<(), Box<dyn std::error::Error>>(())
```

If the proxy host uses DNS64/NAT64 with a network-specific prefix, register the
actual routed prefix in `ProxyConfig` before starting the proxy:

```rust,no_run
# use sandbox_egress::ProxyConfig;
let config = ProxyConfig::default().with_nat64_prefix(
    // Replace this RFC 6052 documentation example with the host's route.
    "2001:db8:122:344::/96".parse()?,
);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Without that host-supplied fact, an arbitrary global IPv6 address cannot be
distinguished from a translated IPv4 address by syntax alone. The well-known
`64:ff9b::/96` prefix is recognized automatically.

## Safe integration order

For a source-IP identity, the host should use this lifecycle:

1. Give the run a fresh host-network generation and source address.
2. Install a deny-first host network path and actively prove that only the
   proxy is reachable.
3. Attach its immutable policy, then run or resume the untrusted workload.
4. Fence the old namespace or NAT path so it cannot create more traffic.
5. Close the lease successfully and remove run-owned conntrack/NAT state.
6. Only then reuse that source address for another run.

TCP does not carry a userspace run generation. The shared listener cannot tell
a deliberately delayed packet from an old owner after the host reassigns the
same address. Host-side fencing and ordering are therefore part of the security
contract. Close performs a final nonblocking accept-queue drain before it
certifies the lease, and attach repeats that barrier before installing a new
mapping. Those barriers destroy sockets already visible to the listener; they
do not make host-side fencing optional or authenticate a packet delayed beyond
the configured quiet interval.

For mark-based nftables routing, verify the effective and bounding capability
sets of every process sharing the guest network namespace. In particular,
drop both `CAP_NET_ADMIN` and `CAP_NET_RAW`; a default container capability set
may still include the latter.

The [host network integration contract](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/host-integration.md) makes the
generation record, fail-closed readiness, restore/resume, orphan
reconciliation, bandwidth fairness, and kernel-capacity evidence explicit.
Firecracker, containers, namespaces, and process sandboxes are consumers of
that boundary rather than concepts in the public crate API.

## Development

The ordinary development loop uses familiar Cargo commands behind small
scripts:

```text
./scripts/check.sh              format, compile, lint, test, docs, package
./scripts/test-conformance.sh   hostile lifecycle and concurrency cases
./scripts/bench.sh              Criterion performance baseline
./scripts/measure-resources.sh  lease, control, idle, TLS, pressure, terminal soak
./scripts/measure-complexity.sh source size and complexity trend report
./scripts/measure-coverage.sh   optional uncovered-code review report
./scripts/measure-load.sh       concurrent CONNECT capacity and tail latency
./scripts/measure-load-sweep.sh repeated concurrency scaling sweep
./scripts/measure-throughput.sh concurrent upload/download tunnel throughput
./scripts/measure-throughput-sweep.sh fixed-work data-plane scaling sweep
./scripts/measure-linux-network-state.sh CMD  Linux conntrack/socket evidence
./scripts/check-iana-drift.sh   opt-in authoritative registry drift signal
cargo run --locked --bin sandbox-egress -- example.com
```

Hosted CI intentionally runs only `./scripts/check.sh` in one cached Linux job.
Pushes to feature branches are checked through their pull request rather than
also consuming a duplicate push run. Cross-platform, MSRV, dependency-policy,
resource, complexity, and container certification remain explicit maintainer
checks instead of spending hosted minutes on every change.

The test programs never call the public network. Ordinary first-time Cargo or
CI setup may fetch the locked dependencies and tools; package verification and
the dependency-policy check run offline once those inputs are present. The
IANA drift command is a separate, explicit maintainer research step: it
downloads the two authoritative CSVs and fails when either differs from the
last reviewed SHA-256 pin. A changed pin requires a policy review; the script
never rewrites the deny table.

Coverage is a review aid, not a release threshold. The optional command pins
its instrumenter version and is most useful for finding unexamined lifecycle
and denial paths; a higher aggregate percentage alone does not justify adding
tests or production branches.

To reproduce the MSRV factory in a clean Linux environment:

```text
docker build -t sandbox-egress:dev .
docker run --rm sandbox-egress:dev
```

The build stage is pinned to Rust 1.88, warms locked dependencies, then runs the
normal factory and a small Linux resource smoke with Cargo offline. After the
checked executables are collected, its compilation tree is discarded before
the layer is committed. The final image contains only the stripped conformance
executables and runs every deterministic case as an unprivileged user. It
does not ship Cargo, the compiler, source tree, or build cache. Tests remain
local and do not call public network services.

The host-boundary certificate is separate because it requires root networking
capabilities. Run it only in a disposable privileged Linux container:

```text
docker build -f Dockerfile.host-boundary -t sandbox-egress-host-boundary:local .
docker run --rm --privileged sandbox-egress-host-boundary:local
```

Start with [AGENTS.md](https://github.com/bryanhelmig/sandbox-egress/blob/main/AGENTS.md). The deeper project record is split into:

- [founding context](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/founding-context.md) — product ambition and audience;
- [design brief](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/design-brief.md) — the original lifecycle requirements;
- [security invariants](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/security-invariants.md) — claims and trust boundary;
- [host network integration](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/host-integration.md) — lifecycle, restore,
  readiness, shaping, and kernel evidence;
- [architecture](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/architecture.md) — internal ownership and data flow;
- [testing strategy](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/testing.md) — conformance and resource evidence;
- [performance evidence](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/performance.md) — reproducible measurements;
- [complexity evidence](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/complexity.md) — source and decision-shape trends;
- [simplicity review](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/simplicity-review.md) — what belongs in the core;
- [engineering log](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/engineering-log.md) — experiments and negative results;
- [hardening backlog](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/hardening-backlog.md) — attack and measurement matrix;
- [prior art](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/prior-art.md) — reviewed projects and pinned revisions;
- [roadmap](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/roadmap.md) — known gaps and release gates.

## Status

Sandbox Egress is early, pre-release software. The core lifecycle,
deterministic hostile suite, packaged source, and macOS/Rust 1.88 Linux
factories work. Before a public `0.1`, it still needs independent API and
threat-model review plus evidence from a real external sandbox integration;
broader client and host-cage matrices remain ongoing. There is no compatibility
promise before the first published release.

Licensed under MIT.

The opt-in [factory pressure guide](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/factory-pressure.md) covers resource certificates, management progress under churn, and default-setting lifecycle measurements.
