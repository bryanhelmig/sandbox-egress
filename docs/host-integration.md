# Host network integration

Sandbox Egress owns the proxy lease. A sandbox supervisor must separately own
the operating-system resources that make the lease meaningful. Treat those
resources as one run-generation record, even if the production implementation
gives that record a different name.

This is an integration contract, not a fourth core crate object. The public
library remains `Proxy / Policy / Lease`; it does not create sandboxes, network
namespaces, virtual interfaces, firewall rules, or NAT state.

## One host record per run generation

The supervisor's durable record should bind at least:

- an unambiguous run ID and monotonically increasing generation;
- the source IP passed to `Proxy::attach`;
- the namespace or guest network, virtual interfaces, routes, and firewall rule
  handles;
- the NAT/conntrack zone or other state needed to find and remove old flows;
- the sandbox process or VM, cgroup, and host traffic-shaping configuration;
- the in-process `Lease` owner or enough state to mark the identity unavailable
  after a supervisor restart.

Resource names should include the generation or another collision-resistant
token. A fixed slot number alone is insufficient when a crashed supervisor can
leave its old namespace or firewall objects behind. Write the ownership record
before enabling traffic, and remove it only after every cleanup check succeeds.

The attached source IP is specifically the peer address the shared proxy
listener observes. It need not equal the address configured inside a guest.
Snapshot pools may safely reuse one baked guest-visible address inside isolated
namespaces only when routing or SNAT translates it to a unique, host-owned
source before the shared listener and conntrack boundary. Attach that observed
translated address; never derive identity from guest configuration metadata.

## Fail-closed startup

Use this order:

1. reserve a fresh generation and source IP;
2. create the guest network path in a deny-first state;
3. install routing, DNS confinement, proxy-only firewall rules, NAT/conntrack
   isolation, and VM-level bandwidth limits;
4. actively prove that the proxy endpoint is reachable and controlled direct
   TCP, UDP, DNS, host-service, and alternate-proxy probes are not;
5. attach the immutable `Policy` to the host-observed source IP;
6. only then launch or resume untrusted guest code.

A missing binary, unavailable nftables hook, failed rule transaction, ambiguous
interface, readiness timeout, or unavailable proxy is a launch failure. Logging
and continuing would silently turn a policy request into unrestricted egress.
Pooled sandboxes and service-mesh sidecars need their own profile: both can
pre-create or rewrite the same network path before a per-run policy exists.

## Certified shutdown and reuse

Use the reverse ownership order:

1. stop the guest vCPUs or otherwise prevent new guest packets;
2. sever or deny the old guest network path;
3. call `Lease::close` and retain the returned lease on failure;
4. verify that no host-side proxy socket, pending host dial, or run-owned
   conntrack/NAT state remains;
5. delete the run's firewall, route, interface, namespace, and shaping state;
6. mark the source IP reusable only after every preceding step succeeds.

The host fence is load-bearing. TCP has no sandbox generation field. A delayed
packet from a disconnected old namespace is distinguishable only because that
namespace no longer has a path, not because the shared listener can infer its
origin. A local socket table in a fenced guest may continue to display stale
TCP state because the guest cannot receive the final FIN or RST; certification
is the absence of a host-owned path and proxy work, not a cooperative guest
state transition.

## Restored and pooled sandboxes

Never serialize or restore a `Lease`. A restored or reassigned sandbox receives
a fresh host generation, fresh network path, fresh immutable policy, and fresh
proxy lease before it resumes. Guest connection state must not become authority
to reuse an old host identity.

Firecracker is one important consumer of this rule: its snapshot documentation
warns that network and vsock packet loss is expected after loading a snapshot
in another process and does not guarantee connection survival. Other VM,
container, and process sandboxes should follow the same fresh-lease rule unless
their host boundary can prove a stronger generation-preserving contract.

For a snapshot taken from a running VM:

- pause the VM, create the snapshot, and decide whether the original run will
  continue or be destroyed;
- if it is destroyed, fence and close its lease exactly as for ordinary
  shutdown;
- never package the source-IP allocation, proxy lease, host conntrack state, or
  NAT mappings as snapshot state;
- before clone resume, install and prove the clone's deny-first path and attach
  its own lease;
- require connections present in guest memory at snapshot time to reconnect;
  do not route them into a replacement run's authority.

The opt-in namespace lane below preserves a live old tunnel while fencing its
veth, certifies zero host-side proxy work, proves the still-live old namespace
has no egress device, and only then recreates the same source IP for a fresh
lease. It models the host ownership transition without coupling the crate to a
particular sandbox or VMM. A concrete sandbox integration can wrap this same
contract with its own launch, restore, and teardown checks.

For a prebuilt network pool, "available" kernel state still needs an owner.
Keep slot ownership in one authoritative ledger, park each prebuilt slot under
a unique sentinel owner, and transfer it atomically to a run rather than using
a free-then-claim window. On release, destroy the namespace/interfaces first
and mark the slot free last. After restart, reconcile stale owners and orphaned
kernel objects before refilling the pool. The ledger's persistence lifetime
must match the kernel objects it describes; durable ownership that outlives a
host reboot can resurrect claims for resources that no longer exist.

## Two rate-control planes

The controls complement one another:

- `ProxyConfig::with_connection_attempt_rate` and
  `PolicyBuilder::connection_attempt_rate` bound source-attributed inbound TCP
  churn before header parsing or task creation. Concurrent connection limits
  still bound live proxy work.
- Linux traffic control, VMM device limits such as Firecracker virtio-net token
  buckets, or an equivalent host mechanism bound packets and bandwidth before
  one sandbox can become a noisy neighbor. The proxy intentionally does not
  emulate a packet shaper.

Connection-attempt limits help reduce upstream socket and conntrack churn, but
they do not make conntrack or ephemeral ports infinite. Capacity planning must
measure the host kernel, choose per-run/fleet ceilings below its safe operating
range, and verify recovery after a run ends.

## Kernel evidence

On Linux, wrap a deterministic load or soak command with:

```sh
scripts/measure-linux-network-state.sh \
  env SANDBOX_EGRESS_LOAD_CONNECTIONS=100000 cargo test --release --test load -- --ignored
```

The wrapper records baseline, peak, and final conntrack entries, TCP sockets,
TIME_WAIT sockets, UDP sockets, allocated files, and the host conntrack limit.
Set `SANDBOX_EGRESS_REQUIRE_KERNEL_RECOVERY=1` to fail when conntrack does not
return to the configured baseline plus
`SANDBOX_EGRESS_KERNEL_RECOVERY_SLACK` before the recovery deadline. These are
host-global signals, so run release evidence on an otherwise quiet worker.

## Opt-in Linux boundary certificate

The ordinary crate factory is unprivileged and cross-platform. The separate
lane requires a disposable privileged Linux environment:

```sh
docker build -f Dockerfile.host-boundary \
  -t sandbox-egress-host-boundary:local .
docker run --rm --privileged sandbox-egress-host-boundary:local
```

It creates isolated host and guest network namespaces, installs a deny-first
nftables input chain, proves the allowed CONNECT path and a blocked direct TCP
decoy, holds and fences a live tunnel, requires final zero-active accounting,
reuses the source address only in a fresh namespace, and removes named orphan
resources. It uses no public network and no randomized input generation.

This certificate is intentionally narrower than a complete deployment matrix.
It does not exercise IPv6, prove UDP/DNS and inherited-descriptor denial, or
measure NAT port recovery. Those checks belong in the generic host-boundary
backlog and in each concrete sandbox integration rather than being implied by
one namespace test.

## Examples behind the boundary

- [Firecracker design](https://github.com/firecracker-microvm/firecracker/blob/main/docs/design.md)
  assigns host networking and traffic filtering to the integrator and exposes
  virtio-net rate limiting for resource fairness.
- [Firecracker production host setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md)
  explicitly requires host firewalling of untrusted guest egress.
- [Firecracker snapshot support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md)
  documents packet loss and the absence of a connection-survival guarantee.
- [n8n's Firecracker runner](https://github.com/n8n-io/n8n-sandbox-service/blob/main/internal/runner/runtime/firecracker.ee/README.md)
  is a concrete per-slot namespace/TAP/veth/NAT design.
- [CubeSandbox's network design](https://github.com/TencentCloud/CubeSandbox/blob/master/docs/blog/posts/2026-06-23-cubesandbox-network-deep-dive.md)
  demonstrates host-owned TAP allocation, L4/L7 separation, and pooled network
  resource setup.
- [PandaStack's NATID implementation](https://github.com/pandastack-io/pandastack-ai/blob/1147f535f303296de45d0b51fb58644dfcf79e14/agent/internal/netns/netns.go)
  shows shared snapshot identity translated to a unique host-visible source;
  its network and slot-store packages document pooled ownership and restart
  reconciliation.
- [SNAS](https://arxiv.org/pdf/2606.17533) reports production experience with
  bandwidth fairness, connection-rate controls, conntrack, and port exhaustion
  across a defense-in-depth sandbox egress system.
