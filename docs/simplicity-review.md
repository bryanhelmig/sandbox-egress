# Simplicity review

This review asks whether Sandbox Egress still needs its current shape after the
first broad hardening pass. The test is not whether a feature is interesting;
it is whether removing it would preserve the founding `Proxy / Policy / Lease`
contract for a hostile sandbox deployment.

## Conclusion

The crate has a small conceptual center and should keep it. A host firewall can
force traffic through one listener, and an ordinary CONNECT proxy can filter a
destination, but neither alone certifies that one run's in-flight DNS, dial,
and tunnel work is gone before its network identity is reused. `Lease::close`
is the reason for this crate rather than another daemon configuration.

The public surface has grown around that center, but the reviewed controls are
not independent product ideas. They close concrete gaps in one of four places:

1. selecting the run without trusting the guest;
2. selecting and validating one destination;
3. bounding hostile connection work;
4. certifying teardown and final accounting.

No current capability is expensive and marginal enough to remove safely. The
strongest simplification in this pass is structural: production proxy code,
routing proofs, and lifecycle proofs now have separate review scopes. Future
growth should face a much higher bar than the current surface did.

## The irreducible kernel

The useful abstraction remains three objects:

- `Proxy` owns process-wide resources: one listener, one runtime, one resolver,
  and global budgets.
- `Policy` freezes the authority and resource limits for one run.
- `Lease` exclusively owns that run's observed identity, work, counters, and
  fallible cleanup certificate.

Supporting public types make those three objects usable without creating more
owners. `PeerIdentity` and `Endpoint` are boundary values. `Usage` and
`FinalUsage` distinguish a live observation from a certified final snapshot.
Typed close and shutdown errors retain the owning handle. TLS and diagnostic
types configure an existing phase; they do not create another subsystem or
lifecycle. `FinalUsage` deliberately has no public constructor or `Default`
implementation: its type-level meaning is that certified close produced it.

The synchronous management API over one owned async runtime is also part of the
kernel. Requiring every sandbox supervisor to become async, or allocating one
runtime per run, would move complexity to every integrator without improving
the proxy boundary.

## Capability audit

| Capability | Why it remains in the core | Simplification decision |
| --- | --- | --- |
| Host-observed source-IP identity | Prevents a guest header or token from selecting another run's policy | Keep one identity mechanism; do not add an authentication plugin system |
| Immutable hostname, port, and network rules | Defines one stable decision for every connection owned by a lease | Keep the builder; do not add live policy mutation |
| Exact/wildcard hostname and network denials | Makes useful carve-outs possible and matches mature proxy precedent | Keep deny-overrides-grant; keep the grammar deliberately small |
| Mature CONNECT parsing and bounded headers | Avoids hand-written HTTP grammar and slow/unbounded request work | Keep `httparse` and `http`; remain CONNECT-only |
| Hickory DNS, absolute names, complete-answer checks, and DNS limits | Prevents search-suffix surprises, rebinding, forbidden-address fallback, and unbounded concurrent lookup work | Keep one resolver boundary; do not implement DNS transport locally |
| Destination floor, mapped/compatible IPv4, and NAT64 handling | Prevents alternate address spellings from recovering private or host-control endpoints | Keep explicit translation knowledge in trusted process config |
| Global/per-lease concurrency and attempt-rate limits | Bounds both long-lived work and rapid terminal churn before task creation | Keep fail-fast admission; defer fairness and reserved shares |
| Absolute handshake and optional idle deadlines | Bounds every pre-tunnel phase and abandoned tunnels without making idle time the security clock | Keep one handshake deadline and one optional shared tunnel clock |
| Upload/download ceilings, counters, and bounded diagnostics | Makes resource use attributable without blocking on an observer | Keep static reasons and a caller-owned bounded channel |
| Optional visible-SNI and explicit ECH policy | Implements the founding authority promise without claiming TLS or application interception | Keep runtime opt-in; do not widen it into MITM or L7 policy |
| Numeric upstream CONNECT chaining | Preserves local resolution and SSRF checks in corporate proxy deployments; multiple peers need this shape | Keep the narrow unauthenticated HTTP route; do not infer ambient proxy settings |
| Thin executable over the library | Enables local use and a later process boundary without a second implementation | Keep one package and one implementation |

## Awkward edges that are honest constraints

Some surface area looks unusual because the underlying guarantee is unusual:

- `close` consumes the lease but returns it inside an error. A conventional
  `&mut self` shutdown or best-effort `Drop` would be simpler syntactically but
  could release identity ownership without a certificate.
- The internal `Open -> Revoking -> Quiesced -> Closed` phases separate actual
  cleanup from the caller observing cleanup. Collapsing those states reopens
  the lost-reply and identity-reuse races already covered by tests.
- Hostname and port grants currently form a Cartesian product. That limitation
  is documented rather than hidden behind an API that appears to express
  per-destination contracts. Adding compound rules is future product scope,
  not a cleanup.
- Explicit network grants can override the conservative address floor. This is
  powerful, but required for sandboxes deliberately allowed to reach private
  services. Denials still win, and deployments must keep tenant/control ranges
  outside broad grants.
- A wildcard listener has no single guest-reachable advertised address.
  `Lease::endpoint` reports its assigned port with the wildcard IP, and the
  host maps that port to each guest's reachable gateway. Adding a second global
  address would be wrong for multi-network hosts; inferring topology is outside
  the listener's authority.
- The trusted management command queue is unbounded so `Drop` can always enqueue
  cleanup without blocking or silently losing it. Guest traffic cannot reach
  that queue; bounding host call concurrency remains an integrator duty.

These are places to review carefully, not reasons to replace explicit behavior
with implicit behavior.

## Dependency audit

There are eight direct runtime dependencies, each with one narrow job:

- Tokio and `tokio-util`: networking, timers, semaphores, cancellation, and
  task ownership;
- Hickory Resolver: maintained system/explicit DNS and UDP/TCP behavior;
- `httparse` and `http`: bounded HTTP parsing and authority syntax;
- Rustls: maintained incremental ClientHello parsing;
- `ipnet`: explicit CIDR policy values;
- `thiserror`: compact typed construction and startup errors.

Replacing the protocol dependencies with local parsers would reduce the
manifest while increasing security-sensitive source and maintenance. Making
every optional behavior a Cargo feature would add a build matrix and conditional
public API. The current single library build is the simpler pre-release shape.

## What stays outside

The following may be valuable in a sandbox product, but do not belong in this
core without a new independently justified boundary:

- process, filesystem, syscall, namespace, TAP, NAT, nftables, or VMM lifecycle;
- transparent TCP/UDP interception or guest DNS service;
- HTTP application policy, TLS termination, MITM, DLP, or credential injection;
- mutable policy hot reload, guest-selected identities, or authentication
  plugins;
- corporate proxy credentials, trust roots, hostname handoff, and ambient
  `HTTP_PROXY`/`NO_PROXY` behavior;
- a fairness scheduler, reserved shares, durable run journal, or fleet control
  plane;
- multiple crates or a framework abstraction without an independently useful
  consumer and dependency boundary.

These exclusions keep the library honest: it governs traffic that the host has
already forced through its listener, and it certifies only work it owns.

## Measured shape after the pass

The security-critical production proxy is 1,553 code lines (1,724 including
comments and blanks) after moving 2,847 lines of white-box machinery into child
test modules. The complete Rust factory had reached 14,903 lines across 31
files, including integration tests, benchmarks, resource soaks, and fixed
protocol fixtures. Removing a redundant already-quiesced race path brought the
tree to 14,900 lines. A final composite proof that the upload ceiling bounds
TLS inspection before forwarding adds 63 test-only lines, leaving the complete
tree at 14,963 lines and its aggregate SCC estimate unchanged at 866 structural
and 2,451 cognitive points; the production proxy remains 185/606. No test was
deleted to improve the headline.

The attach/close benchmark remains in its prior interval with no detected
change. The reorganization is absent from normal builds because it moves only
`cfg(test)` code.

## Bar for the next feature

A proposed core feature should answer all of these before implementation:

1. Which founding invariant or repeated peer-system requirement does it serve?
2. Can the host integration own it more cleanly?
3. Does it introduce a new authority source, lifecycle owner, or parser?
4. What deterministic denial, cancellation, and resource evidence will prove
   it?
5. Can an existing type or phase express it without another public abstraction?
6. Which current code or concept becomes simpler in exchange?

If those answers are weak, the default is to keep the feature outside the
crate. Simplicity here means few authorities and ownership boundaries, not the
fewest possible tests or the shortest manifest.
