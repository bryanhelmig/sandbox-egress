# Prior art

Reviewed through 2026-09-01. Commit pins make future comparisons reproducible;
links remain upstream-owned and are not vendored.

| Project | Reviewed commit | What to learn | Gap this crate targets |
| --- | --- | --- | --- |
| [Stripe Smokescreen](https://github.com/stripe/smokescreen) | `d4da883a` | ACL and IP filtering, operational limits, diagnostics | Go daemon; no run lease |
| [lens-sandbox-core](https://github.com/lensapp/lens-sandbox-core) | `2bc4ecc5` | broad Rust DNS/proxy/TLS/policy implementation and Linux cage boundary | shared mutable policy and detached connection lifecycle |
| [nono](https://github.com/nolabs-ai/nono) | `7989b578` | supervisor-side proxy, credential boundary, audit | guest session token and accept-loop shutdown, not certified close |
| [motosan-sandbox](https://github.com/motosan-dev/motosan-sandbox) | `13eab245` | small per-run CONNECT proxy and hard routing | one proxy per run; spawned tunnels are not a shared lease |
| [ressrf](https://github.com/timescale/ressrf) | `52fc89cf` | generated forbidden ranges, DNS-pinned transports, adversarial parser cases | policy/transport components rather than lease ownership |
| [canister](https://github.com/dergraf/canister) | `27434158` | hostile L7 contracts, body limits, DLP | sandbox product, not reusable lifecycle primitive |
| [eavs](https://github.com/byteowlz/eavs) | `afa178a0` | transparent destination recovery and SNI/Host ACLs | no ephemeral run ownership |
| [microsandbox](https://github.com/superradcompany/microsandbox) | `5b1c63d9` | network-layer DNS timeout/rebinding controls | microVM network subsystem, not forward-proxy lease API |
| [NVIDIA OpenShell](https://github.com/NVIDIA/OpenShell) | `f7180c0f` | operator-owned corporate proxy chaining after local SSRF validation | product supervisor with broader TLS/auth/bypass configuration |

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
strengthen that invariant. Like the reviewed implementations, global
saturation is fail-fast rather than a fairness queue. A two-lease proof pins
correct refusal attribution and admission on retry after certified release;
reserved shares remain a separate, optional scheduling design.

Motosan's per-run proxy uses `copy_bidirectional`, so ordinary directional EOF
inherits Tokio's correct half-close behavior. Its async handle aborts the
listener task, while accepted connections are spawned without retained join
ownership. That is appropriate for a small proxy whose process or sandbox owns
the run, but it cannot certify that those connections ended. Sandbox Egress
therefore keeps the mature bidirectional-copy lesson and separately proves that
lease close terminates a tunnel whose upload direction already reached EOF,
without waiting for the still-open upstream direction to cooperate.

## Hostname-denial comparison

Smokescreen and nono both pin case-insensitive and trailing-root-dot hostname
denials. Nono also tests that a wildcard denial covers subdomains but not its
apex, plus a richer host-and-port deny syntax. Sandbox Egress adopts the first
two normalization proofs and wildcard boundary because they fit its canonical
hostname contract. It does not currently adopt compound host-and-port rules:
ports remain a separate explicit allow dimension, and one implementation alone
does not justify complicating that model.

The nono pin advanced from `8f15fc86` to `7989b578` during this review. The
intervening change only expanded environment variables in credential
local-socket paths; it did not alter the proxy or the comparison above.

## Provider control-plane comparison

The ressrf provider data highlighted a gap that an IANA-only address floor
cannot cover: Azure WireServer lives at the stable virtual public address
`168.63.129.16`. Microsoft documents that address as the host-node endpoint for
platform services and VM-agent traffic. Sandbox Egress therefore denies its
`/32` by default, while preserving the existing explicit-CIDR escape hatch for
trusted deployments that deliberately need it. AWS and GCP metadata addresses
reviewed in the same comparison are already inside the default link-local or
non-global IPv6 floor; importing provider domain lists or broad internal DNS
suffixes would duplicate the resolve-and-check guarantee and was not retained.

## Self-connection comparison

Smokescreen enumerates local interfaces at startup and rejects a destination
whose address is local and whose port is the proxy listener. This closes a
recursive-proxy shape that the ordinary private-address floor cannot cover
after a trusted policy explicitly grants a local network. Sandbox Egress adopts
the invariant at its narrower library boundary: it freezes the actual
post-bind listener address and rejects matching literal and DNS destinations
before policy grants or dialing. It does not add an interface-enumeration
dependency; wildcard and translated-address deployments must bind a concrete
guest-facing address or enforce other local aliases in the host cage.

## Upstream-proxy comparison

Smokescreen, Lens, nono, and OpenShell all support routing outbound CONNECT
tunnels through an operator-controlled proxy. OpenShell makes the important
security ordering explicit: resolve and validate locally, then send the
approved numeric address to the corporate proxy so it cannot perform a second
destination lookup. Its hostname CONNECT escape hatch deliberately transfers
that authority to the upstream proxy and is not adopted here.

Sandbox Egress implements the small common transport core: one process-wide
numeric HTTP proxy address, a bounded parsed CONNECT response, sequential
validated-address fallback, and lease-owned cancellation. It does not yet
import OpenShell's HTTPS proxy transport, CA bundles, credential files, or
resolution-aware bypass rules. Those features introduce separate secret and
trust-root contracts and remain explicit follow-up work rather than ambient
`HTTP_PROXY`, `HTTPS_PROXY`, or `NO_PROXY` behavior a guest could influence.

## Host-cage capability boundary

The current Lens cage review corrected a subtle deployment assumption around
mark-based routing. Linux [`socket(7)`](https://man7.org/linux/man-pages/man7/socket.7.html)
documents that `SO_MARK` requires `CAP_NET_ADMIN` or, since Linux 5.17,
`CAP_NET_RAW`. [Docker's runtime documentation](https://docs.docker.com/engine/containers/run/#runtime-privilege-and-linux-capabilities)
lists `NET_RAW` in its default retained capability set. A cage that exempts
marked proxy sockets must therefore remove both capabilities from every
untrusted workload or sidecar in the governed network namespace. A non-root
UID alone is not the relevant boundary.

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
  Both registries were last updated 2025-10-09 and were rechecked on
  2026-09-01. `scripts/check-iana-drift.sh` pins their authoritative CSV hashes
  as an opt-in review signal; it does not generate policy.
- Microsoft documents [`168.63.129.16`](https://learn.microsoft.com/en-us/azure/virtual-network/what-is-ip-address-168-63-129-16)
  as Azure's stable host-node virtual public endpoint for platform services;
  the default floor denies it explicitly because IANA global-address status is
  not sufficient to describe its sandbox trust boundary.
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
