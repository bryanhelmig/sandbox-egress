# Performance measurement

Measurements are same-host regression evidence, not portable speed promises.
The [release guide](release-certification.md) owns acceptance budgets and the
current readiness verdict. [Factory pressure](factory-pressure.md) owns sampled
resource budgets. Dated observations and rejected optimizations belong in the
[engineering log](engineering-log.md).

## Workloads

| Question | Command | Interpretation |
| --- | --- | --- |
| Setup, hostname/SNI and hostile-header cost | `./scripts/bench.sh` | Criterion includes a direct loopback TCP control beside CONNECT; hostname/SNI cases wait for exact payload acknowledgement |
| Default lease lifecycle cost | `cargo bench --locked --bench lifecycle -- attach_close_empty_lease --noplot` | Compare actual default quiet time with the explicitly zero-quiet control; retain the guard |
| Concurrent setup capacity and latency | `./scripts/measure-load.sh 5000 64 16` | Connections, concurrency, destination count; aggregate rate plus p50/p95/p99 |
| Established-tunnel throughput | `./scripts/measure-throughput.sh 128 8 both` | MiB per tunnel, concurrency, direction; eight tunnels move one GiB per direction with exact accounting |
| Optional idle-clock overhead | `./scripts/measure-throughput.sh 128 8 both 1000` | Fourth argument enables the idle timeout in milliseconds |
| Fixed total-work concurrency sweeps | `./scripts/measure-load-sweep.sh` and `./scripts/measure-throughput-sweep.sh` | Use documented script inputs and retain every observation |

The ordinary setup timer ends at the complete 200 response. The direct control
uses the same local destination and teardown discipline; subtracting it does
not isolate a pure parser cost. Hostname/SNI comparisons use the same controlled
ClientHello and upstream acknowledgement. Throughput starts after all tunnels
are established, uses an explicit start barrier, and includes a terminal
exchange checked against final byte counters. Loopback throughput is not
Internet bandwidth or a production capacity guarantee.

Policy scans depend on trusted rule counts, DNS caching is off by default, and
one proxy owns two runtime workers. Benchmark the intended supervisor's active
lease count, rule sizes, DNS latency and traffic pattern before assigning SLOs.
CPU, allocation and long-duration measurements remain separate questions.

## Comparisons

Use a quiet worker, the locked dependencies and the same toolchain, release
profile, configuration, workload and success oracle. Alternate baseline and
candidate order, preserve raw Criterion estimates and logs, and keep setup,
close and each throughput direction independent. A win in one cannot excuse a
regression in another. If unchanged-source repetitions are noisy, report that
instead of widening budgets or selecting a favorable run.

The release driver fingerprints the complete benchmark contract. Removing the
chaining benchmarks changes that contract, so the alpha.1 contract cannot serve
as a release-comparison baseline for the reduced suite. Establish a reviewed
baseline for the new suite; measurements of unchanged surviving workloads are
useful exploratory evidence but not a complete release certificate.

Do not run benchmark or resource lanes beside compilation or each other.
Use a distinct `CARGO_TARGET_DIR` per worktree; a cached binary from another
source state cannot verify a candidate. Preserve failed evidence and record
source/toolchain identity with every result.

## Historical evidence

The detailed pre-simplification measurement ledger is preserved at
[alpha.1's source commit](https://github.com/bryanhelmig/sandbox-egress/blob/919dd0aa5ab4187ae0659de7aa02c1551cb0bdf3/docs/performance.md).
It includes prior ownership/allocation experiments, local setup/throughput,
resource occupancy and rejected optimizations. Some commands there exercise
the removed upstream-proxy feature and apply only to that historical source.
Keeping the dated ledger there and in Git avoids presenting old workloads or
changing local observations as current performance guarantees.

The [release evidence](release-certification.md#recorded-release-evidence)
records unresolved repeatability and management-overlap failures. Passing a
new individual workload does not erase those failures or certify the actual
sandbox integration.
