# Sandbox Egress

Run-scoped network access for untrusted sandboxes.

An embeddable Rust CONNECT proxy for sandbox supervisors. Give each run an
immutable network policy, bounded connection work, usage counters, and an
explicit shutdown boundary. One shared runtime serves many runs.

## Try the preview

```sh
cargo add sandbox-egress --git https://github.com/bryanhelmig/sandbox-egress --tag v0.1.0-alpha.1
```

This preview is for evaluation and controlled integration. The API may change
between previews. Read the host boundary below before connecting a sandbox.

## Three objects

- `Proxy` owns the listener, resolver, runtime, and process-wide budgets.
- `Policy` defines immutable destination rules and limits for one run.
- `Lease` owns the run's host-observed identity, admitted work, accounting,
  cancellation, and certified cleanup.

```rust,no_run
use std::net::IpAddr;
use std::time::{Duration, Instant};
use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proxy = Proxy::start(ProxyConfig::default())?;
    let policy = Policy::builder()
        .allow_host("api.example.com")?
        .allow_port(443)
        .max_connections(8)?
        .build()?;

    // Use the source address enforced by your host network boundary.
    let source_ip: IpAddr = "127.0.0.1".parse()?;
    let lease = proxy.attach(PeerIdentity::SourceIp(source_ip), policy)?;
    println!("HTTPS_PROXY={}", lease.endpoint());

    // Run the workload, then fence its network path before closing the lease.
    let usage = lease.close(Instant::now() + Duration::from_secs(2))?.usage();
    println!("final usage: {usage:?}");
    proxy.shutdown(Instant::now() + Duration::from_secs(2))?;
    Ok(())
}
```

The management API is synchronous. The proxy owns one Tokio runtime; callers
do not need an async rewrite or a runtime per run. A wildcard listener reports
`0.0.0.0` or `::` in its endpoint; the host chooses the guest-reachable address
and combines it with the assigned port.

## Close is a certificate

Successful `Lease::close` means admission is closed, tracked headers/DNS/dials/
tunnels are gone, sockets have stopped, and usage is final. It does not wait
for a cooperative remote peer. Failure returns the owning lease and keeps its
identity unavailable. `Drop` begins best-effort cancellation and never certifies
cleanup.

A supervisor should keep a failed owner in its quarantine/retry path:

```rust,no_run
use std::time::{Duration, Instant};
use sandbox_egress::{CloseError, FinalUsage, Lease};

// The caller has already fenced the guest. A second failure still returns
// ownership to that caller; it must keep the source address quarantined.
fn close_fenced_run(lease: Lease) -> Result<FinalUsage, CloseError> {
    match lease.close(Instant::now() + Duration::from_secs(2)) {
        Ok(final_usage) => Ok(final_usage),
        Err(error) => {
            eprintln!("close needs retry: {:?}", error.kind());
            error.into_lease().close(Instant::now() + Duration::from_secs(2))
        }
    }
}
```

`Proxy::shutdown` has the same ownership rule through
`ShutdownError::into_proxy`. Once shutdown begins, new attachments are refused.
A lease held after successful proxy shutdown can consume its final counters
without a live runtime.

## The host owns the jail

This crate controls traffic that reaches its listener. The host must prevent
direct TCP/UDP/DNS, inherited-socket, and host-IPC bypasses, and must establish
a source identity the guest cannot spoof. Setting `HTTPS_PROXY` alone does not
create a security boundary.

The lifecycle is: install and prove a deny-first network path; attach the
policy; run the guest; fence the old guest; certify close; remove its network/
conntrack state; only then reuse the source address. TCP carries no run
generation, so listener draining cannot authenticate a delayed old packet
after address reassignment.

Read the [deployment contract](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/deployment-contract.md)
and [host integration guide](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/host-integration.md)
before embedding. They cover namespace/NAT ownership, restore, bypass testing,
and capability boundaries. In particular, mark-based exemptions require dropping
both `CAP_NET_ADMIN` and `CAP_NET_RAW` from untrusted workloads.

## Deliberate policy semantics

- Every rule dimension starts denied. A hostname grant does not grant a port.
  Host and port grants form a Cartesian product, not endpoint pairs.
- Hostname denials override grants. Every DNS answer is checked before dialing
  an approved numeric address; explicit network grants can permit private
  services, while network denials still win. Direct IP literals require a
  network grant.
- Byte ceilings apply per tunnel; live/final usage aggregates the lease.
- TLS/SNI inspection is opt-in. It can verify visible SNI, with explicit ECH
  handling, but cannot enforce an application authority inside encrypted TLS.
- Invalid process ceilings fail startup; requested limits are never silently
  enlarged or reduced. Defaults remain bounded and diagnostics/cache opt-in.

The [configuration reference](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/configuration.md)
covers timeouts, rates, diagnostics, DNS, NAT64, TLS inspection, and numeric
upstream CONNECT chaining. Plain HTTP forwarding, MITM, credential injection,
transparent interception, and VMM management are outside the core.

## Development and release evidence

```text
./scripts/check.sh                       ordinary Cargo factory
./scripts/test-conformance.sh            hostile lifecycle/protocol cases
./scripts/bench.sh                       Criterion measurements
python3 scripts/certify-resources.py     bounded RSS/FD/thread evidence
./scripts/measure-complexity.sh          source/decision trend report
./scripts/build-host-fixture.sh          compile the external public-API consumer
```

Tests use local peers. Cargo/tool setup may fetch locked dependencies. Hosted
CI remains one cached Linux job; heavy resource, performance, MSRV, and privileged
host checks are explicit maintainer work. See [factory pressure](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/factory-pressure.md)
and [release certification](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/release-certification.md).

Start contributions with [AGENTS.md](https://github.com/bryanhelmig/sandbox-egress/blob/main/AGENTS.md).
The [architecture](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/architecture.md),
[security invariants](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/security-invariants.md),
[performance record](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/performance.md),
and [prior art](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/prior-art.md)
explain the design and its evidence.

## Status

`0.1.0-alpha.1` is a preview, with no stable API promise. Correctness, resource,
and Linux host-fixture checks have passed, but the full release certificate
remains failed: repeatable management-pressure overlap and performance
calibration are unresolved. See the [release evidence](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/release-certification.md#preview-launch-evidence).

Passing factory checks does not replace independent API/threat-model review or a
real integrating sandbox's security certification. The
[roadmap](https://github.com/bryanhelmig/sandbox-egress/blob/main/docs/roadmap.md)
separates public-source, preview-crate, and production-readiness gates.

Licensed under MIT.
