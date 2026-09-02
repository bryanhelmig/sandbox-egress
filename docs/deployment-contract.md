# Deployment contract

Sandbox Egress controls only connections that reach its listener. It is one
part of a sandbox boundary, not the mechanism that forces a guest to use that
boundary. A production integration must establish the following conditions
before treating a lease policy as an egress guarantee.

The division of responsibility is exact:

- Sandbox Egress attributes each accepted TCP connection from its observed
  source IP. The host makes that source IP unspoofable and unique to the
  attached run.
- Sandbox Egress applies the immutable lease policy to accepted HTTP `CONNECT`
  requests. The host routes the guest to that endpoint and denies direct TCP,
  UDP, raw-socket, and alternate-proxy egress.
- Sandbox Egress resolves hostnames, rejects forbidden answers, and dials the
  approved numeric address without another lookup. The host ensures guest DNS,
  host resolvers, and local relays cannot become alternate data paths.
- Sandbox Egress tracks accepted work from parsing through DNS, TLS inspection,
  dialing, and tunnelling. The host starts the guest without inherited or
  passed network sockets that bypass the listener.
- Sandbox Egress revokes tracked work and certifies final counters with
  `Lease::close`. The host fences the old guest first and reuses its source
  address only after close succeeds.
- Sandbox Egress can exempt its upstream sockets from the host egress cage. The
  host prevents the guest and its sidecars from creating equivalent exempt
  sockets.

Proxy environment variables are application configuration, not confinement.
They can help ordinary software discover `Lease::endpoint()`, but an untrusted
program may ignore or replace them. The host boundary must still prevent every
route except the intended proxy endpoint and any deliberately isolated local
services.

An already-connected descriptor is especially important: it creates no new
connection for the proxy to accept, attribute, count, or revoke. Close
unneeded descriptors before guest launch, use close-on-exec where applicable,
and constrain every intentional descriptor-passing channel. The same principle
applies to unrelated loopback listeners and host IPC endpoints.

For a Firecracker integration, the usual shape is a guest-specific TAP or
namespace path whose firewall permits TCP only to the shared proxy listener.
The exact kernel mechanism is deployment-specific. Whatever mechanism is
chosen must also cover IPv4 and IPv6, reject forwarding around the listener,
and keep the proxy's own upstream path unavailable to the guest.
The normative generation, readiness, snapshot, reconciliation, and kernel
capacity sequence is in the [Firecracker host integration](firecracker-integration.md).

If `SO_MARK` distinguishes proxy-originated sockets on Linux, remove both
`CAP_NET_ADMIN` and `CAP_NET_RAW` from every untrusted process and sidecar in
the governed network namespace. Since Linux 5.17, either capability can set a
socket mark. A non-root UID alone is insufficient.

The required lifecycle order is:

1. reserve a fresh host-network generation, then install and verify the guest
   network boundary in a deny-first state;
2. attach the source identity and immutable policy;
3. launch the guest without unintended network descriptors;
4. prevent the guest from creating more traffic;
5. close the lease successfully;
6. remove run-owned conntrack/NAT and interface state, then reuse the guest
   identity only after certified close and teardown verification.

Failure at step 5 retains lease ownership. Do not reuse the identity or treat
partial cleanup as success.

## Conformance target

The crate's deterministic suite proves the behavior inside the listener. A
deployment should add black-box tests around the finished sandbox that attempt
all of the following and require failure:

- direct TCP and UDP to an external service;
- IPv4 and IPv6 paths that do not terminate at the proxy;
- guest-chosen proxy environment overrides;
- unrelated loopback and host IPC endpoints;
- inherited or deliberately passed connected sockets;
- direct access to the proxy's upstream route or recursive resolver;
- source-address reuse before a failed or incomplete close is recovered.

Those tests belong at the integration boundary because the library cannot
observe a bypass that never reaches its listener. The repository ships a first
privileged Linux namespace certificate in
`scripts/test-linux-host-boundary.sh`. It proves proxy-only TCP routing,
fenced close, source-IP reuse, and named-resource cleanup. TAP/KVM, IPv6, UDP,
DNS, inherited descriptors, and NAT-port recovery remain deployment-level
follow-up work rather than implied coverage.
