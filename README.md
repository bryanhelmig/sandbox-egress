# Sandbox Egress

Network policy and reliable per-run cleanup for Rust sandbox supervisors.

One shared `Proxy` serves many runs. Each run gets an immutable `Policy` and an
owning `Lease`. The host supplies a source IP the guest cannot spoof, and forces
all guest egress through the proxy.

```sh
cargo add sandbox-egress --git https://github.com/bryanhelmig/sandbox-egress --tag v0.1.2
```

Version 0.1.2 hardens a DNS cancellation test under load. Runtime behavior and
the public API are unchanged from 0.1.1.
[Upgrading from alpha.1](CHANGELOG.md) includes renamed byte-limit methods and
removal of upstream proxy chaining.
The API may change in subsequent 0.x releases.

```rust,no_run
use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proxy = Proxy::start(
        ProxyConfig::default()
            // Supply your actual host/control/tenant networks here.
            .with_denied_network("10.20.0.0/16".parse()?),
    )?;
    let policy = Policy::builder()
        .allow_host("api.example.com")?
        .allow_port(443)
        .max_connections(8)?
        .build()?;
    let lease = proxy.attach(
        PeerIdentity::SourceIp("127.0.0.1".parse()?), // host-observed run IP
        policy,
    )?;
    println!("HTTPS_PROXY={}", lease.endpoint());

    // Launch the guest. When finished, stop it and fence its network first.
    let usage = lease.close(Instant::now() + Duration::from_secs(2))?.usage();
    println!("final usage: {usage:?}");
    // The host must now clear and verify kernel network state before IP reuse.
    proxy.shutdown(Instant::now() + Duration::from_secs(2))?;
    Ok(())
}
```

The management API is synchronous. The proxy owns one Tokio runtime; callers
need no async rewrite or runtime per lease. A wildcard listener reports its
wildcard address; the host supplies the address reachable from the guest.

## The reuse rule

**Fence the old guest → close its lease → clean the host's network state →
verify it is empty → attach the next run.**

You can destroy and recreate a network slot, or reset a pooled slot in place.
Both must leave the same clean result. Destroying a VM or its guest namespace
alone does not necessarily remove connections owned by the shared host proxy.

`Lease::close` certifies that library-owned tasks and socket handles are gone
and counters are final. The kernel can still retain TCP or conntrack/NAT state.
The host owns that second cleanup boundary. See the [integration recipe](docs/host-integration.md)
for both patterns, failure handling, and the executable Linux pooled test.

Close retains the identity for a **25 ms default quiet interval** so already
queued old sockets cannot enter the next run's policy. Each observed old
arrival restarts that interval, so total close latency can be longer. This
wait neither cleans kernel state nor authenticates delayed packets.

A failed close returns the owning lease through `CloseError::into_lease()`;
keep the slot quarantined and retry. Successful close followed by failed host
cleanup also keeps the slot quarantined, under the supervisor's ownership.
`Drop` is best-effort cancellation and never certifies cleanup.

## Policy and host responsibilities

- Hostnames, networks, and ports start denied. Host and port grants form a
  Cartesian product. IP literals need explicit network grants.
- Every DNS answer is checked before dialing an approved numeric address.
  Proxy-wide network denials and per-policy denials override policy grants.
  A denied address rejects the whole answer. Use `with_dns_address_family`
  to select IPv4-only or IPv6-only resolution before those checks.
- Byte ceilings apply to each tunnel; usage totals aggregate the lease.
- Optional SNI inspection checks the visible `ClientHello` against CONNECT.
  It happens after the upstream TCP dial, and forwards no rejected hello.
  It does not inspect encrypted application authority. This ordering preserves
  HTTP 502 errors for failed dials; there is no promise of zero pre-SNI dials.
- `HTTPS_PROXY` is configuration, not confinement. The host must prevent direct
  TCP/UDP/DNS, inherited-socket, and host-IPC bypasses, and source-IP spoofing.

Read the [deployment contract](docs/deployment-contract.md) and
[configuration reference](docs/configuration.md) before embedding.
The [architecture](docs/architecture.md) explains ownership;
[security invariants](docs/security-invariants.md) define enforcement.

## Verification and release scope

```sh
./scripts/check.sh
./scripts/test-conformance.sh
python3 scripts/certify-resources.py
# Disposable privileged Linux; the pooled lane needs CONFIG_INET_DIAG_DESTROY:
docker build -f Dockerfile.host-boundary -t sandbox-egress-host .
docker run --rm --network=none --privileged sandbox-egress-host
```

The independent host-boundary lane runs first and reports success. If socket
destruction is unsupported, the pooled preflight then exits 78; the first
lane's coverage is retained, including on Docker Desktop.

The full release certificate requires correctness, resource bounds, and
supported host-boundary checks. Performance calibration is reported separately.
Version 0.1.2 retains one known certificate gap: macOS management-pressure
samples can miss competing-traffic overlap even
when deadlines pass. The full certificate remains incomplete; its assertions
and failed verdict are unchanged. See the [release evidence](https://github.com/bryanhelmig/sandbox-egress/releases/tag/v0.1.2).

A passing Linux fixture certifies its tested topology, not a complete
Firecracker deployment. See [CONTRIBUTING.md](CONTRIBUTING.md) for the factory,
release commands, evidence limits, and remaining gaps. Hosted CI is one bounded
Linux job; heavier checks run explicitly for releases.

Licensed under MIT.
