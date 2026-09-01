# Prior art

Reviewed through 2026-09-01. Commit pins make future comparisons reproducible;
links remain upstream-owned and are not vendored.

| Project | Reviewed commit | What to learn | Gap this crate targets |
| --- | --- | --- | --- |
| [Stripe Smokescreen](https://github.com/stripe/smokescreen) | `d4da883a` | ACL and IP filtering, operational limits, diagnostics | Go daemon; no run lease |
| [lens-sandbox-core](https://github.com/lensapp/lens-sandbox-core) | `a0a95786` | broad Rust DNS/proxy/TLS/policy implementation | shared mutable policy and detached connection lifecycle |
| [nono](https://github.com/nolabs-ai/nono) | `8f15fc86` | supervisor-side proxy, credential boundary, audit | guest session token and accept-loop shutdown, not certified close |
| [motosan-sandbox](https://github.com/motosan-dev/motosan-sandbox) | `13eab245` | small per-run CONNECT proxy and hard routing | one proxy per run; spawned tunnels are not a shared lease |
| [ressrf](https://github.com/timescale/ressrf) | `52fc89cf` | generated forbidden ranges, DNS-pinned transports, adversarial parser cases | policy/transport components rather than lease ownership |
| [canister](https://github.com/dergraf/canister) | `27434158` | hostile L7 contracts, body limits, DLP | sandbox product, not reusable lifecycle primitive |
| [eavs](https://github.com/byteowlz/eavs) | `afa178a0` | transparent destination recovery and SNI/Host ACLs | no ephemeral run ownership |
| [microsandbox](https://github.com/superradcompany/microsandbox) | `5b1c63d9` | network-layer DNS timeout/rebinding controls | microVM network subsystem, not forward-proxy lease API |

Also relevant: Rama for composable mature proxy machinery and VEY/G3 for
production daemon limits, metrics, ACLs, and per-user policy.

## Admission and shutdown comparison

The reviewed implementations reinforce two separate rules that should not be
conflated. `lens-sandbox-core` shares one semaphore across its explicit and
transparent listeners, acquires a permit before spawning a handler, and drops
an accepted socket when the process-wide limit is full. `nono` has one accept
loop; its active-count check and increment have no suspension point between
them, so they are serialized by that task before each handler is spawned.

Neither shape supplies the lifecycle certificate required here. Lens's
connection handlers are detached process-lifetime tasks. Nono's `shutdown()`
signals its accept loop through a watch channel, but its handle does not own a
join result for the loop or the spawned handlers. Smokescreen performs stronger
process-wide draining with a connection tracker, but it does not hand one
source identity from an old run to a new policy on a shared live listener.

Sandbox Egress therefore keeps its distinct boundary: reserve global and
per-lease capacity before task creation, register the task under the immutable
lease, and make certified lease close wait for that owned task set. One
listener-owner command loop serializes identity installation. A 32-caller
contention case proves exactly one attachment wins and every other caller sees
`IdentityInUse`; adding a second registry synchronization scheme would not
strengthen that invariant.

## Protocol references

- [Rustls 0.23 server `Acceptor`](https://docs.rs/rustls/0.23.43/rustls/server/struct.Acceptor.html)
  supplies the incremental, syntactic ClientHello boundary and visible SNI.
- [RFC 9849](https://www.rfc-editor.org/rfc/rfc9849.html) defines TLS Encrypted
  ClientHello and the distinction between visible outer and encrypted inner
  names.
- The [IANA TLS extension registry](https://www.iana.org/assignments/tls-extensiontype-values)
  assigns `0xfe0d` to Encrypted ClientHello.
- The IANA [IPv4](https://www.iana.org/assignments/iana-ipv4-special-registry)
  and [IPv6](https://www.iana.org/assignments/iana-ipv6-special-registry)
  special-purpose registries are the source of the default address floor;
  transition prefixes receive extra treatment when they can encode IPv4.
- [RFC 6052](https://www.rfc-editor.org/rfc/rfc6052.html) defines the
  well-known and network-specific NAT64 prefix lengths and the six layouts
  used to recover the effective IPv4 destination.
- Rust's [`Ipv6Addr`](https://doc.rust-lang.org/stable/std/net/struct.Ipv6Addr.html)
  distinguishes mapped conversion from the broader mapped-or-compatible
  conversion used by the guard.

## Research rule

Copy concepts, tests, and threat-model lessons—not code—unless license and
provenance are reviewed. Record new sources and exact commits here before a
design meaningfully depends on them.
