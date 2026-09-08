# Scope and simplicity

The useful core is `Proxy / Policy / Lease`: one shared runtime and listener,
an immutable policy for one run, and an owning handle that certifies cleanup
without allowing identity reuse on failure. That close contract distinguishes
this module from an ordinary proxy. The [architecture](architecture.md)
explains its implementation; [security invariants](security-invariants.md)
define the guarantees.

## Retained responsibilities

| Responsibility | Why it belongs |
| --- | --- |
| Host-observed source-IP identity and immutable policy | A guest cannot choose another run or mutate an admitted connection's authority |
| CONNECT parsing, DNS and conservative destination checks | One permitted authority leads to one checked numeric dial, with maintained protocol parsers |
| Connection, DNS, dial, byte and time bounds | Hostile traffic cannot create unrestricted lease-owned work |
| Accounting and bounded diagnostics | The supervisor can observe use without making enforcement wait for logging |
| Optional visible-SNI/ECH checks | A bounded initial authority check, with no TLS interception or hidden-application claim |
| Fallible close/shutdown, final usage and owning errors | Failure retains ownership; cleanup and observation remain distinct |
| One synchronous facade over one owned runtime | Integrators do not need a runtime per lease or an async supervisor rewrite |

Keep mature parsers and the lifecycle states. Removing dependencies by writing
local protocol grammar, or collapsing quiescence into best-effort Drop, would
move essential complexity rather than eliminate it. Supporting value and error
types do not create more lifecycle owners.

## Decisions after the first consumer review

**Upstream HTTP CONNECT chaining is removed.** The first custom sandbox service
does not need a corporate proxy route. Direct checked-address dialing removes
the extra negotiation, response parser, prefix-buffered stream and associated
test combinations. There is no generic connector plugin replacement.

**Byte ceilings are explicitly per tunnel.** The builders are
`max_tunnel_upload_bytes` and `max_tunnel_download_bytes`. Usage still sums the
whole lease; every new tunnel gets its own allowance. These settings are not
an aggregate run spending cap.

**Custom NAT64 remains host configuration.** A host routing an RFC 6052 prefix
must register it. Removing recognition while allowing those routes would
weaken destination checks. A future IPv4-only scope needs an explicit host
contract, not silent removal of equivalent-address defenses.

**Keep one crate and the thin executable.** There is no independent API or
dependency boundary requiring more packages. TLS inspection and diagnostics
remain runtime opt-ins; a Cargo feature for every setting would add a build
matrix without simplifying the consumer model.

## Deliberately outside the core

Proxy chaining, authentication plugins, credentials, TLS termination/MITM,
application-layer policy, transparent interception, UDP/DNS service, live
policy mutation, VMM/namespace management, fairness scheduling, and fleet
control remain outside this module. The host owns confinement and network
generation teardown as defined by the [deployment contract](deployment-contract.md).

New scope must identify a founding invariant or concrete consumer need, show
why the host cannot own it more cleanly, and include deterministic denial,
cancellation and resource evidence. It should remove a responsibility or
justify each new one. Configuration breadth is a cost even when disabled code
is inexpensive.

## Documentation and evidence

The README is the short integration recipe; configuration is the advanced
reference; testing is an invariant-to-check index. Factory pressure owns
workloads and budgets, and release certification owns release status. Dated
results, failed trials and prior scope decisions remain in the append-only
[engineering log](engineering-log.md). Earlier feature-retention judgments
there are historical, not exemptions from the current scope decision.
