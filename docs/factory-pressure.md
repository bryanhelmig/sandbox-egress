# Factory pressure and the next contribution

The factory should reward preserved ownership, visible resource costs, and a
small consumer interface. A change need not add a feature or improve a score.
A rejected optimization or a disproved concern is useful evidence.

## Checks matched to the claim

| Claim | Evidence | Acceptance |
| --- | --- | --- |
| Core semantics still hold | `./scripts/check.sh` and security-sensitive `./scripts/test-conformance.sh` | Existing denial, cancellation, accounting, and reuse cases stay green |
| A host can replace one run without restarting its shared proxy | `scripts/test-linux-host-boundary.sh` in disposable Linux | Same process/listener, different destination grant, fenced old namespace, old grant denied, unrelated single tunnel continues with exact accounting |
| Management progresses while unrelated clients churn | `cargo test --locked --release --test management_load -- --ignored --nocapture` | Both known and unknown source cases complete; every sample overlaps observed terminal traffic; each attach/close meets the explicit workload budget; quiet lease counters remain zero |
| Default lifecycle cost is visible | `cargo bench --locked --bench lifecycle -- attach_close_empty_lease --noplot` | Record both default-quiet and historical zero-quiet control; do not reduce the guard to win a benchmark |
| Resource regression is bounded | `python3 scripts/certify-resources.py` | All nine Rust lanes pass, measurements exist, sampled RSS and post-warmup growth stay within stated budgets |
| Complexity comparisons are comparable | `./scripts/measure-complexity.sh` | SCC 4.0.0 required; explain changed responsibilities instead of optimizing the aggregate score |

Keep hosted CI as one ordinary Linux job. These heavier lanes run locally or on
an explicitly selected release worker. Do not turn every push into a benchmark
or privileged container job.

[Release certification](release-certification.md) combines the required lanes
from isolated, committed source snapshots and records failed or missing evidence.
It requires a reviewed, comparable performance baseline; an unchanged-source
calibration is useful evidence about the worker, not an optimization result.

## Resource certificate

The Python standard-library driver runs each existing Rust resource lane in a
fresh process. It first runs eight deterministic negative-control tests for the
evidence evaluator. Each Rust subprocess group has a timeout; a stuck workload
is killed and cannot receive a success certificate. The JSON report records
commands, workload settings, commit, dirty state, source-tree fingerprint,
toolchain, platform, logs, measurements, and pass/fail. It fingerprints the
source before and after all lanes and rejects a mixed-source run. A failed or
unsupported measurement produces `passed: false` and a nonzero command exit.

```sh
python3 scripts/certify-resources.py
```

Defaults: four batches, 250 operations per batch in serial churn lanes, 64
concurrent connections in occupancy lanes, 16 concurrent management callers,
eight backpressured tunnels per batch, 128 MiB sampled RSS ceiling, and 8 MiB
post-warmup growth allowance. Each lane has a 180-second wall-clock bound,
including a cold build. Build release tests first on a slow worker or choose an
explicit longer bound. The output defaults to `target/resource-certificate.json`
with a unique sibling log directory for each invocation.

The first completed batch is allocator warm-up. Every later batch must remain
within the growth allowance relative to that first batch; final recovery cannot
hide an earlier growth spike. Non-batched occupancy lanes require peak,
recovered, and final samples. File-descriptor and thread recovery assertions
remain in the Rust tests. Certification also requires the Rust baseline
collectors to succeed; a missing baseline cannot silently skip recovery checks. At least three batches are required.

The budgets are conservative regression tripwires for this complete fixture
process, including its test peers. They are not limits on the production proxy
alone, continuous peak monitoring, proof of zero allocations, or a long-duration
leak guarantee. Changing the workload or budget is a reviewable change to the
measurement. Never raise a budget only to make a failing candidate green.

For a fixed release worker, calibrate with repeated unchanged runs, then record
an explicit invocation such as:

```sh
python3 scripts/certify-resources.py --require-clean \
  --runs-per-batch 1000 --batches 6 \
  --connections 128 --max-rss-kib 131072 --max-growth-kib 8192 \
  --output target/release-resource-certificate.json
```

Python 3 is required only for this opt-in lane, not to embed the library. Linux
and macOS collectors are supported; missing `ps`/`lsof` observations on macOS
fail certification rather than being interpreted as zero.

## Management workload and its limits

The workload uses eight loopback clients by default and 32 attach/close cycles,
with the default identity quiet period. One case attributes those clients to a
deny-all lease and exercises parser denials. A second case leaves them unknown
and exercises immediate refusal. A separate identity repeatedly attaches and
closes without receiving traffic. Each attach and close has a one-second
workload acceptance budget. A supervising caller bounds the complete experiment
and stops churn before handling failure, including a stuck synchronous attach.

The knobs are `SANDBOX_EGRESS_MANAGEMENT_WORKERS`,
`SANDBOX_EGRESS_MANAGEMENT_CYCLES`, and `SANDBOX_EGRESS_MANAGEMENT_MAX_MS`.
Every reported sample must overlap completed competitor connections; socket
timeouts do not count as evidence of useful churn. The fixture bounds workers
to 128 and cycles to 1,000. Results print the configuration, observed exchanges,
maximum attach latency, and maximum close latency.

Clients use reset-on-drop after observing a terminal peer outcome, matching the
connection benchmarks. This avoids accumulating local TCP teardown state that
can interrupt offered churn. Per-cycle counters distinguish connection attempts,
successful connects, terminal outcomes, and connect/read errors. The overlap
assertion remains mandatory; neither a socket timeout nor a quiet interval is
counted as competing work. Ordinary TCP teardown is covered by the lifecycle
and resource lanes rather than being conflated with this bounded churn fixture.

This is a finite progress workload, not a new API guarantee of fairness or
bounded attach latency under arbitrary saturation. The shared accept drain
still requires an empty observation. Sustained full-batch drain starvation,
delayed old SYNs, and NAT/conntrack generation teardown remain explicit work.
Do not bypass a drain or release identity early to improve these measurements.

## Bounded hit list for the next agent

1. **Measure before tuning.** Pair the direct TCP control with allowed CONNECT,
   default lifecycle, and established-tunnel throughput. Use the same workload
   and toolchain, alternate baseline/candidate order, and retain raw results.
   Reject an optimization if the result disappears when order reverses. No
   production change is required when there is no repeatable benefit.
2. **Measure allocation cost.** Focus on bytes allocated per CONNECT and per
   inspected ClientHello. Include slow/near-limit inputs and resource recovery.
   Keep parsing mature and buffers bounded; do not replace parsers or add
   unsafe code for a benchmark. A useful win removes an allocation or copy
   without creating another owner or another configuration knob.
3. **Calibrate heavier resource pressure.** Repeat the certificate at larger
   fixed workloads on one dedicated Linux worker. Add a long-lived traffic lane
   only if it exposes a gap in the existing backpressure/churn lanes. Separate
   fixture-peer memory from proxy memory before claiming a production budget.
4. **Extend the host proof carefully.** Add NAT/conntrack generation teardown
   and delayed-packet cases to the existing same-proxy harness. Preserve the
   unrelated tunnel and different replacement policy. Deliver a failing
   reproduction before proposing changes to admission, quiescence, drain
   barriers, or identity release; those need focused senior review.
5. **Quantify the known DNS decoder exposure.** Use fixed malformed responses
   and configured lookup concurrency, measure peak memory and recovery, and
   prefer a supported upstream bound. Do not fork DNS framing merely to reduce
   a dependency count. The collected-address ceiling is not a decoder byte cap.

Every retained change should strengthen a named invariant, remove a concept,
or show a repeatable measured benefit. Preserve negative results in the
engineering log. Update only the canonical current contract and link to it;
do not duplicate the same explanation in every document.

## Feature decisions reserved for the owner

No feature is removed in this pass. These are candidates to justify before
expansion, not defects or automatic deletion instructions:

- **Corporate upstream CONNECT chaining.** It adds another TCP setup and
  response-parser phase, numeric-target fallback semantics, and cross-feature
  cancellation/accounting tests. Establish that a real consumer needs it before
  adding TLS, authentication, credential handling, or bypass configuration.
- **Operator-specific NAT64 prefixes.** Six RFC 6052 layouts plus overlap and
  denial-equivalence behavior expand the policy matrix. This is necessary when
  the host actually routes those translations; it may be speculative for an
  IPv4-only first consumer. Removing recognition while allowing such routes
  would weaken SSRF protection, so any simplification needs an explicit
  deployment-scope decision. Keep mapped-address and ordinary destination guards.

The outer-SNI/ECH compatibility option also deserves deliberate consumer choice,
but its implementation is small; it is not a strong code-size deletion target.
Keep the core lease state machine, owning runtime, and mature protocol parsers.
They carry the component's defining guarantees.
