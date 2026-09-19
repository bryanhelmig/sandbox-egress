# Contributing and the factory

Start with [AGENTS.md](AGENTS.md). Add a deterministic failing test, make the
smallest change, and keep the red result with the green evidence. Public APIs
need rustdoc and migration notes. New dependencies need a rationale and a
license/advisory check. Report suspected bypasses through [SECURITY.md](SECURITY.md).

## Claims and commands

| Claim | Command / evidence |
| --- | --- |
| API, formatting, Clippy, ordinary tests, doctests, docs and package | `./scripts/check.sh` |
| Hostile protocol/lifecycle behavior | `./scripts/test-conformance.sh` |
| Public embedding API | `./scripts/build-host-fixture.sh` |
| Pooled kernel preflight and silent-no-op controls | `python3 scripts/test-pooled-preflight.py` (also in CI and the host image build) |
| Bounded RSS, descriptors and threads | `python3 scripts/certify-resources.py --require-clean --output NEW_FILE` |
| Attach/close progress with competing traffic | `cargo test --locked --release --test management_load -- --ignored --nocapture` |
| Linux destroy/recreate and pooled reset | Build/run `Dockerfile.host-boundary` privileged, with `--network=none` |
| Rust 1.88 compatibility and Linux conformance | Build/run `Dockerfile` |
| Source complexity trend | `./scripts/measure-complexity.sh` |
| Performance observations | `./scripts/bench.sh`, `./scripts/measure-throughput.sh 128 8 both` |

The resource certificate has eight isolated lanes. Its defaults are four
batches, 250 churn iterations per batch, 64 occupied connections, 16 concurrent
management workers, eight backpressure iterations, and 180 seconds per lane.
RSS limits are 131072 KiB peak and 8192 KiB post-warmup growth; descriptors and
threads must recover. These are regression tripwires for the finite workload,
not production density or long-duration guarantees. Missing required metrics,
timeouts, source changes, or budget violations fail the certificate. Test its
negative controls with `python3 scripts/test-resource-certificate.py`.

The management workload covers attributed and unknown-source churn. Every
attach/close sample must observe competing completed traffic as well as stay
inside its explicit deadline, with no cross-identity accounting. Historical
runs sometimes missed traffic overlap despite meeting deadlines. This is an
unresolved workload/evidence gap, not an established starvation bug. Preserve
that failure until a demonstrated workload repair resolves it; do not accept a
lucky retry or silently weaken the overlap assertion.

The host image first runs the independent host-boundary lane and reports its
success, then proves socket destruction on a live loopback pair. Missing kernel
support exits 78 before the pooled scenarios; it does not pass the full certificate.
Docker Desktop can run the first lane, but its reviewed kernel cannot run the
pooled lane. See the host guide for kernel requirements.
The host fixture uses local controlled peers. Its pooled lane then proves
that each omitted reset operation prevents reuse, then proves the complete
sequence, unchanged slot identity, exact client-port reuse, full replacement
capacity, policy replacement, and bystander continuity. It does not certify
an arbitrary VMM, address family, bypass topology, or delayed-packet boundary.
See [host integration](docs/host-integration.md).

## Release certificate

From a committed, clean tree:

```sh
python3 scripts/certify-release.py --output /path/to/new/evidence
# Optional independent performance comparison:
python3 scripts/certify-release.py --baseline REVIEWED_COMMIT \
  --output /path/to/another/new/evidence
```

The driver creates clean detached snapshots under `~/code`, never publishes,
and preserves unexpected edits. It requires Cargo, the pinned toolchain,
cargo-deny, SCC 4.0.0, Python 3, and Docker. Required lanes cover correctness,
dependency policy, strict resources, management pressure, complexity tooling,
MSRV, Linux host profiles and management, plus benchmark workload assertions.
Hosted CI remains one bounded cached Linux job; heavier checks stay here.

Schema 2 records `release_eligible`, the required lane names, every lane's
status, source fingerprints, commands, log hashes, tools, and image identities.
The process exits nonzero if a required lane fails or is missing. A benchmark
run's exact-traffic/accounting assertions remain correctness checks.

Comparative **performance** is advisory and has its own status. No baseline
means `not_run`; incompatible workloads, missing observations, noisy timing,
or a measured regression remain visible failures of that comparison but do
not change release eligibility. Baseline preparation belongs to this lane and
cannot prevent independent correctness checks. Existing schema-1 reports keep
their original verdict; do not rewrite them as passed. Test evaluator controls
with `python3 scripts/test-release-certificate.py`.

Performance comparisons retain three alternating baseline/candidate pairs,
identical workload/dependency/toolchain definitions, direct-TCP setup controls,
default-quiet close, and one GiB in each direction across eight tunnels.
Budgets remain 15% setup, 5% close, and 20% throughput per direction. Reject
nonfinite/incomplete/noisy measurements; never enlarge a tolerance to pass.
These measurements guide engineering and do not assert a service SLO.

## Cutting a release

Update both package lockfiles, Cargo version, README install tag, and CHANGELOG.
Keep current documentation to the five integration/design guides under docs/;
retain durable decisions there and use Git history for removed chronology.
Verify `cargo package --list` and `cargo publish --locked --dry-run` before
publishing. A GitHub release and a crates.io upload are separate outcomes;
report which happened. Never move an existing tag or imply production
certification from a version designation.

Attach a source-bound evidence summary to the release, including any failed
required lane and all untested host boundaries. Version 0.1.0 is the official
release of the reviewed implementation with the macOS management-overlap gap
disclosed; it must not be described as fully certified. The release designation
does not change certificate requirements. Production readiness also needs independent
API/threat-model review and the integrating host's own tests.

## Remaining bounded work

- Resolve management-pressure overlap with captured client/listener state.
  Prior client reset-on-drop and pacing experiments were not repeatable fixes.
- Expand host tests for IPv6, UDP/DNS and inherited-descriptor bypasses, packet
  delay, and additional NAT/conntrack ownership topologies.
- Preserve the current dial-before-SNI decision unless a separate, tested API
  explicitly accepts optimistic CONNECT success and later tunnel failure.
- Explore abortive cancellation separately; it cannot erase existing orphans.
- Keep Hickory decoder allocation exposure distinct from the bounded collected
  answer vector. Prefer an upstream byte-aware bound to a custom DNS parser.

Optional coverage tooling is `./scripts/measure-coverage.sh`; inspect uncovered
behavior, not just the percentage. `scripts/measure-linux-network-state.sh`
measures host kernel recovery on an otherwise quiet Linux worker.
