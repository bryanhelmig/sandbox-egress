# Configuration and enforcement reference

Start with the minimal recipe in the [README](../README.md). The normative
trust boundary remains [the deployment contract](deployment-contract.md).

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

