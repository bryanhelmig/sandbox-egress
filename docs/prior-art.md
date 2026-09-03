# Prior art

Reviewed through 2026-09-02. Commit pins make future comparisons reproducible;
links remain upstream-owned and are not vendored.

| Project | Reviewed commit | What to learn | Gap this crate targets |
| --- | --- | --- | --- |
| [Stripe Smokescreen](https://github.com/stripe/smokescreen) | `d4da883a` | ACL and IP filtering, operational limits, diagnostics | Go daemon; no run lease |
| [lens-sandbox-core](https://github.com/lensapp/lens-sandbox-core) | `9f04f2e` | broad Rust DNS/proxy/TLS/policy implementation and Linux cage boundary | shared mutable policy and detached connection lifecycle |
| [nono](https://github.com/nolabs-ai/nono) | `d3c6f6b0` | supervisor-side proxy, credential boundary, bounded-consumer operational lessons | guest session token and accept-loop shutdown, not certified close |
| [motosan-sandbox](https://github.com/motosan-dev/motosan-sandbox) | `13eab245` | small per-run CONNECT proxy and hard routing | one proxy per run; spawned tunnels are not a shared lease |
| [ressrf](https://github.com/timescale/ressrf) | `52fc89cf` | generated forbidden ranges, DNS-pinned transports, adversarial parser cases | policy/transport components rather than lease ownership |
| [Raincoat](https://github.com/zachgenius/raincoat) | `811c8330` | honest host-cage boundary and hostile plain-HTTP framing cases | sandbox product with per-process proxy ownership |
| [RunSeal](https://github.com/runseal-labs/runseal) | `001b0dd6` | black-box proxy-bypass conformance across the process and network cage | sandbox product; listener lifecycle is not its reusable boundary |
| [canister](https://github.com/dergraf/canister) | `27434158` | hostile L7 contracts, body limits, DLP | sandbox product, not reusable lifecycle primitive |
| [eavs](https://github.com/byteowlz/eavs) | `afa178a0` | transparent destination recovery and SNI/Host ACLs | no ephemeral run ownership |
| [microsandbox](https://github.com/superradcompany/microsandbox) | `df4e1ead` | network-layer DNS timeout/rebinding controls | microVM network subsystem, not forward-proxy lease API |
| [NVIDIA OpenShell](https://github.com/NVIDIA/OpenShell) | `4ef84234` | operator-owned corporate proxy chaining after local SSRF validation | product supervisor with broader TLS/auth/bypass configuration |
| [torkbot/sandbox](https://github.com/torkbot/sandbox) | `3dc0dd5c` | transparent per-flow grants bound to original destination, DNS evidence, and TLS metadata | one network service per VM; teardown is VM-owned rather than a reusable lease certificate |
| [G3](https://github.com/bytedance/g3) | `79e99f76` | production user rate/concurrency limits, buffer controls, protocol breadth | daemon/user model rather than ephemeral certified leases |
| [Rama](https://github.com/plabayo/rama) | `cde3aa85` | composable timeout, concurrency, and token-bucket policies | general framework rather than an opinionated sandbox boundary |
| [Firecracker](https://github.com/firecracker-microvm/firecracker) | `4c998054` | host-owned TAP/filtering boundary, virtio-net token buckets, snapshot network limitations | VMM primitive; the integrator owns egress policy and host cleanup |
| [n8n sandbox service](https://github.com/n8n-io/n8n-sandbox-service) | `e7a7e728` | per-slot netns/TAP/veth/NAT lifecycle for snapshot-restored VMs | product-local slot networking without a shared proxy lease certificate |
| [CubeSandbox](https://github.com/TencentCloud/CubeSandbox) | `30e002cb` | dedicated TAP ownership, host allocator, L4/L7 split, pooled setup | sandbox platform rather than an embeddable CONNECT lease |
| [OpenSandbox](https://github.com/opensandbox-group/OpenSandbox) | `1eb8fffa` | deny-first subjects, generation-aware policy, atomic nft updates, restart cleanup | mutable transparent sidecar/control plane with different authority scope |
| [mvm](https://github.com/tinylabscom/mvm) | `4ebd13d5` | mechanically enforced vsock-only single egress path and fresh restore endpoint | full signed-plan sandbox with no workload NIC, not source-IP CONNECT |
| [Microsoft MXC](https://github.com/microsoft/mxc) | `878936a4` | backend capability validation, mapped-address firewall lowering, host-side microVM socket policy | cross-backend sandbox product; policy lifetime follows its runner/backend |
| [PandaStack](https://github.com/pandastack-io/pandastack-ai) | `1147f535` | shared snapshot IP translation, authoritative slot ownership, destroy-first reuse, orphan reconciliation | sandbox platform whose network pool remains supervisor-owned |
| [Hickory DNS](https://github.com/hickory-dns/hickory-dns) | `8c7b8780` (main), `0.26.1` (dependency) | maintained async resolver, system configuration, cache and transport behavior | decoded record vectors reserve from wire section counts before caller result limits |

Also relevant: VEY for production daemon limits, metrics, ACLs, and per-user
policy.

[SNAS](https://arxiv.org/pdf/2606.17533) is also relevant operational
literature rather than a reusable crate. It reports bandwidth fairness,
connection-rate limiting, conntrack pressure, source-port exhaustion, and
telemetry-driven tuning in a production multi-layer sandbox egress design.

## Admission and shutdown comparison

The closest implementations differ most at the ownership boundary, not at the
shape of an allowlist:

| Project | Run or policy selector | Connection owner | Strongest reviewed cleanup boundary |
| --- | --- | --- | --- |
| Smokescreen | request-derived role against a process policy | process connection tracker | process-wide server shutdown and drain |
| lens-sandbox-core | shared mutable proxy policy | detached process-lifetime handlers | proxy/process lifetime |
| nono | sandbox session configuration and guest token flows | one proxy accept loop plus spawned handlers | accept-loop shutdown signal |
| motosan-sandbox | one small proxy instance per sandbox run | spawned handlers under that instance | listener-task abort |
| torkbot/sandbox | one host-created network service per microVM | the VM's transparent network worker | `Drop` signals and joins that worker |
| Sandbox Egress | host-observed source IP plus one immutable `Policy` | one `Lease` tracker, cancellation token, and permits | fallible per-run close with final counters and retained ownership on failure |

This is an architectural comparison, not a quality ranking. Process-wide drain
is the right boundary for a daemon that exits between policy generations, and
a per-run listener can rely on destruction of the surrounding sandbox. A
shared long-lived listener that reuses source identities needs the narrower
lease certificate because the process remains alive while one run ends.

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
correct refusal attribution and admission on retry after certified release.
The same permit spans partial headers, DNS, dialing, and the live tunnel rather
than treating request parsing and tunnel occupancy as unrelated budgets. A
resource lane holds many partial headers concurrently and proves certified
release of every socket and permit. Reserved shares remain a separate,
optional scheduling design.

Several sandbox-local proxies accept a guest token, header, or per-process
listener as their run selector. Sandbox Egress deliberately does not generalize
that mechanism. A real-socket proof installs a restrictive policy for the
actual loopback peer and a permissive policy for another attached source
address, then sends the other address in `X-Run-ID`. The request is denied by
the observed peer's policy, the destination is never dialed, and the claimed
lease records no connection. This preserves the host-authenticated identity
boundary even when a familiar guest header is present.

## Host-network generation comparison

Firecracker deliberately forwards guest packets to a host TAP without applying
destination policy. Its production guidance assigns filtering to the host, and
its virtio-net token buckets address VM resource fairness rather than CONNECT
authority. Its snapshot documentation also warns that packet loss is expected
and network connection state is not guaranteed to survive restoration. Those
facts keep TAP ownership, shaping, and snapshot recovery in the supervisor
contract instead of pretending the proxy can infer them.

The n8n Firecracker runner demonstrates a concrete slot-shaped bundle: network
namespace, TAP, veth, routes, NAT, and guest addressing are created together.
Its deterministic names are local to one runner process and can collide across
multiple runners. CubeSandbox instead centralizes allocation and pools prepared
TAP resources for latency. The common lesson is ownership, not one naming
scheme: Sandbox Egress documents a generation-bearing host record and restart
reconciliation, while leaving pooling and allocator implementation to the
sandbox service.

OpenSandbox's current fleet design is particularly relevant to shared egress.
A discovered subject begins deny-first, policy pushes carry a generation, nft
sets are swapped atomically, and process restart wipes stale rules before live
subjects are rediscovered. Its original proposal also documented graceful
degradation when netfilter setup failed. That historical contrast is useful:
the Sandbox Egress host contract chooses the current fail-closed direction and
requires readiness probes before guest launch. It does not adopt a mutable
per-subject policy API; a phase change remains certified close followed by a
fresh immutable attach.

Mvm removes the raw-network problem entirely for production workloads: its
guest has no NIC and all networking uses one host-owned authenticated vsock
path, with static CI gates against another connection owner. That is a stronger
alternative identity and transport architecture. It does not replace the
founding source-IP boundary here, but it reinforces the rule that the sandbox
must mechanically provide exactly one egress path rather than rely on proxy
environment variables.

These comparisons produced a separate
[host network integration contract](host-integration.md), an opt-in privileged
Linux namespace certificate, and Linux conntrack/socket measurement.
Firecracker is one mapped consumer of that generic boundary. The work did not
add TAP, nftables, snapshot, or VM orchestration to the public crate API.

PandaStack adds a useful concrete pooled-restore case. Sandboxes from one
snapshot intentionally retain the same guest-visible IP, MAC, and gateway
inside separate namespaces. Egress SNAT rewrites that shared address to the
slot's unique namespace-side veth address before traffic reaches the root
namespace; otherwise return traffic and conntrack can collide. Its allocator
also records one authoritative owner per slot, atomically transfers prebuilt
slots rather than briefly freeing them, destroys kernel objects before marking
a slot reusable, and reconciles orphan ownership and namespaces before the
prewarmer starts. The project comments tie that shape to earlier leak,
double-free, and stale-namespace incidents. These are supervisor lessons, not
evidence to add pooling or durable storage to Sandbox Egress itself.

Microsoft MXC's backend matrix reinforces the same compositional rule from a
different direction. Its [Seatbelt path](https://github.com/microsoft/mxc/blob/878936a4aa3356b64b0949d5f213b85449a2e414/docs/seatbelt/seatbelt-backend.md)
rejects hostname filtering when the OS primitive cannot express it, rather than
accepting a policy and silently weakening it. Its
[Bubblewrap firewall path](https://github.com/microsoft/mxc/blob/878936a4aa3356b64b0949d5f213b85449a2e414/docs/bwrap-support/bubblewrap-backend.md)
normalizes IPv4-mapped literals and CIDRs into the packet family Linux will
actually emit, and rejects ambiguous shorter IPv6 blocks that straddle the
mapped range. Its experimental [Nanvix backend](https://github.com/microsoft/mxc/blob/878936a4aa3356b64b0949d5f213b85449a2e414/docs/nanvix-microvm/nanvix.md)
instead resolves hostnames into a host-side socket policy before the run and
handles resolution failure according to whether dropping an entry can widen
access.

Those are useful backend and policy-compilation lessons, but they do not expose
a smaller shared-listener lifecycle. Sandbox Egress already rejects
unrepresentable identity/configuration shapes, checks mapped and compatible
addresses at the effective IPv4 boundary, and validates every live lookup as a
set before dialing. MXC keeps sandbox/backend lifetime as the cleanup owner;
the per-source `Lease` certificate remains the distinct reusable boundary here.

## Listener failure comparison

Smokescreen delegates serving to Go's
[`net/http.Server`](https://github.com/golang/go/blob/go1.26.6/src/net/http/server.go#L3428-L3459),
whose temporary accept-error path starts at a 5 millisecond delay, doubles to a
one-second ceiling, and resets after a successful accept. At the reviewed Lens
pin, [both proxy listeners](https://github.com/lensapp/lens-sandbox-core/blob/2bc4ecc5d92a3dac985d28fbdfe0c1c0e1db4ffc/crates/lens-sandbox-core/src/proxy.rs#L510-L550)
warn and immediately continue after an accept error. Current nono commit
`d3c6f6b0` [does the same](https://github.com/nolabs-ai/nono/blob/d3c6f6b009fa97fe3985dbf5bfb1b1a8ea6b3d27/crates/nono-proxy/src/server.rs#L1375-L1402)
in its proxy loop. [Motosan](https://github.com/motosan-dev/motosan-sandbox/blob/13eab245e25100638db091381f24fe51d23d9e78/crates/motosan-sandbox-proxy/src/lib.rs#L53-L70)
instead warns and ends its small per-run accept task.

Immediate retry is risky when a ready listener repeatedly reports process or
system descriptor exhaustion: it can monopolize an executor worker. Ending the
loop is also the wrong library contract for a shared proxy that must preserve
management ownership. Sandbox Egress therefore keeps an asynchronous bounded
backoff for ordinary accepts. Its mandatory identity drain fails `close` or a
replacement `attach` with `ListenerUnavailable`, rather than either spinning
or treating an uninspected queue as empty. The caller retains the lease and can
retry after host resources recover.

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
ports remain a separate explicit allow dimension whose combinations with hosts
are explicitly documented and tested. Callers needing host-specific ports must
not approximate that narrower policy with this release.

Ressrf rejects ambiguous legacy IPv4 text before resolution. Sandbox Egress
does not need to infer an effective address from that text: a trusted host must
first allow it as a hostname, the production resolver receives an absolute
DNS name, and every returned address is then checked as an address. The
conformance case now proves that distinction on the real resolver wire path
for shorthand, leading-zero, hexadecimal, and decimal spellings; all resolve
to loopback and reach zero connector calls.

An earlier nono review advanced from `8f15fc86` to `7989b578`. That intervening
change only expanded environment variables in credential local-socket paths;
it did not alter the proxy or the comparison above. The later table pin
`d3c6f6b0` is the revision used for the current accept-loop comparison.

## Resolver decoder boundary

Sandbox Egress uses Hickory 0.26.1 instead of implementing DNS parsing and
transport. That release includes fixes for two 2026 security reports, including
bounded name-compression work. The resolver gives this crate maintained system
configuration, UDP-to-TCP fallback, caching, cancellation, and lookup APIs.
The dependency choice is still a boundary to inspect rather than an assurance
that every resource dimension is caller-configurable.

At both release 0.26.1 and reviewed main commit `8c7b8780`, Hickory's
[`Message::read_records`](https://github.com/hickory-dns/hickory-dns/blob/v0.26.1/crates/proto/src/op/message.rs#L422-L432)
reserves each record vector from the untrusted 16-bit wire count before parsing
records. Sandbox Egress's returned-address ceiling runs after that decode. The
resolver has no supported response-byte or decode-allocation option; its EDNS
payload setting describes queries and cannot bound TCP replies.

A fixed local UDP reply with 65,535 advertised answers and no records remains
fail-closed with zero dialing. Five fresh debug test processes completed in
30--50 milliseconds and reported 12,337,152--12,386,304 bytes maximum RSS; an
adjacent ordinary malformed reply reported 12,288,000 bytes. Those numbers do
not establish dangerous RSS amplification, but they also do not certify a
byte-aware decoder bound. The existing lookup semaphore limits simultaneous
exposure to the host-configured maximum, 32 by default.

A crate-local transport wrapper would need to implement both UDP transaction
handling and length-prefixed TCP fallback before passing messages to Hickory.
Vendoring the decoder would create a security-patch fork. Neither is retained
for this measured residual. The backlog instead calls for an upstream decoder
capacity bound or supported transport response ceiling, with the fixed-wire
conformance case kept as a regression sentinel.

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

An earlier follow-up against nono `46867b2f` and ressrf `52fc89cf` found no
additional default address class to import. Nono's proxy inventory names AWS
IPv4 and IPv6 metadata plus Google and Azure metadata hostnames. Ressrf's cloud
tier additionally names the ECS task endpoint `169.254.170.2` and Azure
WireServer. The whole IPv4 link-local range already covers both AWS IPv4
addresses, the non-global IPv6 floor covers `fd00:ec2::254`, and the explicit
WireServer rule covers the only globally classified address in that set.
Sandbox Egress does not add hostname-deny literals: a hostname must first be
allowed by the immutable run policy, and every answer is then checked against
the address floor before the approved numeric address is dialed.

## Self-connection comparison

Smokescreen enumerates local interfaces at startup and rejects a destination
whose address is local and whose port is the proxy listener. Lens's later
multi-listener work likewise rejects wildcard extra addresses and names which
listener lanes may receive each address; its review treats listening reach as
an authority property rather than a bind convenience. These close a
recursive-proxy shape that the ordinary private-address floor cannot cover
after a trusted policy explicitly grants a local network.

Sandbox Egress avoids importing a mutable interface-enumeration dependency. A
concrete bind rejects its exact post-bind endpoint. A wildcard bind instead
rejects every destination on the assigned listener port, because the library
cannot distinguish a remote address from another local interface using its
frozen configuration alone. This is deliberately conservative: callers that
need an unrelated destination on that port must bind one concrete guest-facing
address. Literal and DNS paths share the same check before policy grants or
dialing.

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

OpenShell separately proves that refusal of one validated numeric CONNECT
target falls through to the next locally approved address. Sandbox Egress pins
the same behavior through its shared listener and lease boundary: exactly one
absolute hostname lookup returns two approved addresses, the upstream proxy
sees only those two numeric authorities in order, and a refusal followed by a
successful tunnel remains one accepted and completed guest connection. This
does not adopt OpenShell's hostname-target escape hatch.

The comparison also exposes a lifecycle phase absent from direct-only resource
tests: an operator proxy can accept TCP and then hold an incomplete CONNECT
response indefinitely. Sandbox Egress therefore keeps a concurrent resource
lane with 128 such negotiations. Lease close must cancel all response parsing,
terminate both guest and upstream sockets, and return exact final ownership and
counters without waiting for either peer.

Smokescreen's instrumented connection records the byte count returned by each
Go `Read` or `Write` even when that operation also returns an error. Sandbox
Egress additionally enforces per-tunnel byte ceilings, so it pins the adjacent
ordering explicitly: an exact-boundary transport error is not relabeled as a
policy denial, while a successfully observed excess byte is counted and denied
before any later transport error.

## Protocol-scope comparison

Torkbot's Sandbox demonstrates the stronger authority promise that becomes
possible when a product owns the transparent network path. It derives policy
inputs from the original IP destination, guest-scoped accepted DNS answers,
and visible TLS metadata; its HTTP path explicitly does not treat the guest's
`Host` header as trusted authority. That is a useful reference for a future L7
mode, but it is not evidence that a CONNECT tunnel enforces application
authority.

Sandbox Egress therefore retains its narrower, explicit promise: CONNECT
authority plus visible SNI when configured, with ECH handled according to the
immutable policy. It does not import transparent TCP/UDP interception, HTTPS
MITM, credential injection, or guest DNS attribution into the core library.
Those facilities require a wider host integration and trust-root boundary.
The ownership distinction also remains material: Torkbot's network worker is
created and destroyed with one VM, whereas this crate must certify one source
identity's cleanup while its shared listener and other leases continue.

Raincoat and canister both accept plain HTTP and consequently own request-body
framing, ambiguous `Content-Length`/`Transfer-Encoding` rejection, interim
responses, and response-delimitation behavior. Those are useful hostile cases,
but importing them would materially widen this crate's authority promise and
parser state. Sandbox Egress remains CONNECT-only: bytes after the one bounded
CONNECT header belong to one already-selected tunnel and cannot select another
destination.

The comparison did expose a smaller transport-proof gap. The existing
throughput harness moved data in one direction per run, while hostile shutdown
tests applied simultaneous pressure without proving complete delivery. A real
socket case now transfers approximately 1 MiB upward and 3 MiB downward at the
same time, checks every byte at each peer, and requires exact final accounting
and one graceful completion. This tests the common tunnel core without adding
plain-HTTP policy or parser machinery.

## Host-cage capability boundary

The current Lens cage review corrected a subtle deployment assumption around
mark-based routing. Linux [`socket(7)`](https://man7.org/linux/man-pages/man7/socket.7.html)
documents that `SO_MARK` requires `CAP_NET_ADMIN` or, since Linux 5.17,
`CAP_NET_RAW`. [Docker's runtime documentation](https://docs.docker.com/engine/containers/run/#runtime-privilege-and-linux-capabilities)
lists `NET_RAW` in its default retained capability set. A cage that exempts
marked proxy sockets must therefore remove both capabilities from every
untrusted workload or sidecar in the governed network namespace. A non-root
UID alone is not the relevant boundary.

The current RunSeal comparison adds a useful black-box deployment distinction:
its proxy-mode conformance covers direct TCP and UDP, unrelated loopback,
host IPC, environment overrides, and inherited-socket bypasses. Those are
properties of RunSeal's process-and-network cage, not behaviors a listener-only
crate can implement or certify. Sandbox Egress now records the same cases as a
host integration contract. Its generic Linux host-boundary certificate covers
the proxy-only TCP path, fenced close, identity reuse, and orphan cleanup;
broader bypass cases remain integration work rather than claims that
`Lease::close` can revoke traffic it never accepted.

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
