# Configuration reference

Start with the [README](../README.md) recipe. The [deployment contract](deployment-contract.md)
defines host obligations, and [security invariants](security-invariants.md)
define enforcement. This page explains which knobs an integrator must choose.
The complete builder methods and validation errors are documented in
[ProxyConfig](../src/config.rs) and [PolicyBuilder](../src/policy.rs).

## Process configuration and run policy

`ProxyConfig` is fixed at startup and owns shared resource ceilings, the
listener, resolver and diagnostics. `Policy` is immutable for one lease;
changing rules requires certified close and a new attachment.

| Control | Default | Scope |
| --- | --- | --- |
| Listener | `127.0.0.1:0` | Host chooses the guest-facing bind and reachable advertised address |
| Concurrent connections | 256 per proxy; 64 per lease | Fail-fast admission before task creation |
| Concurrent DNS / dials | 32 / 256 | Shared phase budgets; permits are released when the phase ends |
| Resolved addresses | 64 | Oversized answers are rejected as a whole; configurable up to 1,024 |
| CONNECT header / ClientHello bytes | 32 KiB / 64 KiB | Each configurable from 1 KiB to 1 MiB |
| Header / handshake timeout | 10 s / 10 s | Absolute deadlines starting at socket acceptance |
| DNS timeout | 3 s | Includes waiting for DNS capacity; capped by handshake deadline |
| Identity quiet period | 25 ms | Close/reuse guard after revocation; does not authenticate delayed packets |
| Proxy-wide denied networks | Empty | Immutable floor for every lease on this proxy; grants cannot override it |
| Attempt-rate buckets | Disabled | Optional per-proxy and per-lease rate/burst limits |
| Tunnel upload/download ceilings | Disabled | A fresh allowance for each tunnel, not an aggregate run quota |
| Tunnel idle timeout | Disabled | Either direction's traffic resets the clock |
| TLS inspection / diagnostics / DNS cache | Disabled | Explicit opt-ins described below |

Invalid capacities, byte ceilings and unrepresentable deadlines fail startup
or policy construction; they are never silently clamped. Wildcard listeners
report a wildcard endpoint; the host supplies the guest-reachable address.
They also reject all destinations on their listener port to prevent recursion
through another local interface. Bind a concrete address if that is too broad.

```rust,no_run
# use sandbox_egress::ProxyConfig;
let config = ProxyConfig::default()
    .with_max_connections(512)
    .with_connection_attempt_rate(2_000, 250)
    .with_max_concurrent_dns(64)
    .with_max_concurrent_dials(128);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Destination rules and byte ceilings

Every hostname/network/port grant starts empty. Hostnames are canonical ASCII
DNS names, including explicit punycode; Unicode is not converted implicitly.
`*.example.com` matches subdomains at any depth, but not `example.com` itself.
Host denials win over grants. Hostnames and ports form a Cartesian product;
allowing two hosts and two ports permits all four combinations.

Each hostname is resolved once and every answer passes destination policy
before any numeric address is dialed directly. Explicit CIDR grants may permit
private services; CIDR denials still win. Direct IP literals require a network
grant. The library does not chain through another proxy or inherit ambient
proxy environment settings for its outbound route.

Set host, tenant, public/NAT, and control-plane exclusions once at startup:

```rust,no_run
# use sandbox_egress::ProxyConfig;
let config = ProxyConfig::default()
    .with_denied_network("10.20.0.0/16".parse()?)
    .with_denied_network("2001:db8:1234::/48".parse()?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Use the actual networks for your host. No per-run grant overrides this floor.
It rejects matching literal destinations and DNS answers, including mapped,
compatible, and configured NAT64 forms of denied IPv4 addresses. A mixed DNS
answer containing a forbidden address is rejected before any dial. Denials
report `proxy-network-denied`. Explicit recursive DNS servers are trusted
process infrastructure and are not filtered by this guest-destination floor.
The scope is one `Proxy`; configure every instance when a process has several.
Host topology changes require updated configuration or firewall enforcement.

```rust,no_run
# use sandbox_egress::Policy;
let policy = Policy::builder()
    .allow_host("api.example.com")?
    .allow_port(443)
    .max_tunnel_upload_bytes(1_048_576)
    .max_tunnel_download_bytes(8_388_608)
    .build()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Two tunnels each receive the full configured allowance. `Usage` and `FinalUsage`
aggregate bytes across the lease; the limits above cannot enforce a total run
spending cap. After establishment, a tunnel forwards the permitted prefix and
closes when it observes excess input. Coalesced upload and TLS inspection have
earlier fail-closed boundaries described in the security invariants.

## DNS and NAT64

By default the resolver snapshots host configuration at startup. Supplying an
explicit server avoids both the hosts file and the system resolver config;
the configured address and port serve UDP with TCP recovery:

```rust,no_run
# use sandbox_egress::ProxyConfig;
let config = ProxyConfig::default()
    .with_dns_server("10.0.0.2:53".parse()?)
    .with_dns_server("10.0.0.3:53".parse()?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Up to eight distinct, concrete unicast servers are allowed. Scoped IPv6 DNS
servers are unsupported, and a server cannot be the shared listener itself.
These are trusted host choices; a guest cannot select a resolver.

Caching is off because the dependency counts responses, not bytes. A host can
opt into at most 64 responses with a maximum TTL of 24 hours. Cached answers
still cross each current policy's destination checks:

```rust,no_run
# use sandbox_egress::ProxyConfig;
# use std::time::Duration;
let config = ProxyConfig::default().with_dns_cache(32, Duration::from_secs(60));
# Ok::<(), Box<dyn std::error::Error>>(())
```

A host routing a network-specific RFC 6052 translation prefix must register it:

```rust,no_run
# use sandbox_egress::ProxyConfig;
let config = ProxyConfig::default().with_nat64_prefix(
    // Replace this documentation example with the actual routed prefix.
    "2001:db8:122:344::/96".parse()?,
);
# Ok::<(), Box<dyn std::error::Error>>(())
```

The well-known `64:ff9b::/96` prefix is recognized automatically. Arbitrary
global IPv6 syntax alone cannot reveal a host's translation route.

## TLS authority and diagnostics

`require_tls_sni()` requires a bounded initial ClientHello whose visible SNI
matches the CONNECT hostname and rejects ECH. IP-literal CONNECT cannot satisfy
that hostname requirement. `TlsAuthority::RequireVisibleSni` with
`EchPolicy::AllowOuterSni` explicitly permits an unknowable encrypted inner
name. Neither mode terminates TLS or checks encrypted application authority.
The destination TCP connection occurs before inspection; rejected ClientHello
bytes are not forwarded.

This ordering is deliberate: dial failures can return HTTP 502 before CONNECT
success. A normal TLS client waits for HTTP 200 before sending ClientHello.
Inspect-before-dial would require optimistic 200, after which dial failure can
only close the tunnel. This preview retains dial-before-inspection and does
not expose an additional ordering switch. SNI matching is not client identity
authentication or inspection of the encrypted application authority.

`with_diagnostic_channel(sender, max_events_per_second)` accepts a caller-owned
bounded synchronous channel. Events have static reasons and lease attribution;
delivery never blocks enforcement. Rate and channel suppression are counted,
and a disconnected receiver disables delivery. The consumer owns retention
and aggregation. Payloads and guest-provided authority text are not logged.
