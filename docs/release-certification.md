# Explicit release checks

`python3 scripts/certify-release.py --baseline COMMIT --output NEW_DIRECTORY`
runs the maintainer factory from a committed, clean source tree. It does not
publish code or a crate, change hosted CI, or replace an independent security
review. Python 3, Cargo with the pinned toolchain, cargo-deny, SCC 4.0.0, and a
working Docker daemon are required. Docker runs the host fixture in a disposable
privileged container with its external network disconnected.
Both container factories omit dev-profile debug symbols to keep test images
small; native release benchmarks retain the committed release profile.

The driver creates detached candidate/baseline worktrees under `~/code` (or
`--worktree-parent`) and removes clean worktrees afterward. Unexpected edits are
preserved for diagnosis. The output directory must be new; failed evidence is
never overwritten by a later attempt. Build-target/toolchain/Rust-flag overrides
are rejected. Keep the release worker quiet while measuring performance.

The JSON manifest starts with every required lane `not_run`, marks each running
lane, and records pass/fail plus commands, log hashes, source fingerprints,
toolchain, and image identities. Missing tools, required measurements, changed
sources, failed checks, or incompatible benchmark contracts produce a nonzero
exit and `passed: false`. A successful Cargo benchmark alone is insufficient.
Independent lanes continue after a failure so the report can guide the next
repair; dependent lanes remain `not_run` when their prerequisite failed.

Required lanes are ordinary checks, hostile conformance, a fresh dependency
policy check, evidence-evaluator controls, strict resource certification, native
management pressure, source complexity, the Rust 1.88 container factory, the
external host consumer, Linux management pressure, complete Criterion suites,
and a controlled performance comparison. The advisory database is freshly
fetched into the evidence directory and its commit is
recorded; network access for dependency/tool setup is separate from local test
traffic. No registry credential or upload command is used.

## Performance acceptance

The baseline must be an explicitly selected reviewed commit with identical
benchmark code, throughput workload, Cargo manifest and lockfile, toolchain,
and Cargo configuration. If the oracle or workload changes, establish a new
baseline; do not compare incompatible definitions. `--baseline HEAD` is useful for an
unchanged-source calibration, not proof of improvement over an older release.

Three baseline/candidate pairs alternate order. Each measures the direct TCP
control, allowed CONNECT, default-quiet attach/close, and one GiB upload/download
through eight established tunnels. JSON Criterion estimates and complete logs
are retained. Setup compares the CONNECT median minus its same-process direct
control; it is an end-to-end difference, not a parser-cost attribution.

Initial review budgets are 15% for setup overhead, 5% for default close, and
20% for each throughput direction. These are regression tripwires, not service
SLOs. Both versions' repeat spread must fit the corresponding budget; otherwise
the run is noisy and fails instead of inventing a wider allowance. Median
regression beyond a budget fails independently; a throughput win cannot offset
a close regression. Fixed controls verify rejection of slower work, missing
samples, nonfinite data, noisy observations, and missing release lanes.

Calibrate unchanged-source runs on the selected release worker before relying
on these budgets. If noise prevents a verdict, investigate the worker and
retain the failed report. Changing a workload or tolerance needs separate
review and evidence; never raise it merely to make the current candidate pass.

## External host consumer and executable freshness

`tests/consumer` is a separate, unpublished Cargo package depending only on the
public crate. The normal example compiles that same fixture source, avoiding a
second implementation. The Linux namespace test builds the external package
through `scripts/build-host-fixture.sh`; Cargo checks freshness on every local
invocation and reports the actual executable, including a custom
`CARGO_TARGET_DIR`.

A supplied `SANDBOX_EGRESS_HOST_FIXTURE` requires
`SANDBOX_EGRESS_HOST_FIXTURE_SHA256`; mismatch fails before namespace creation.
The host image builds and hashes its executable explicitly. The outer release
manifest identifies the image actually run, binding that prebuilt path to the
candidate build. A hash supplied by an arbitrary caller is an integrity check,
not independent proof of its source.

The consumer checks a deliberately failed close, retained ownership and denied
reattachment, then successful retry, replacement policy on the same listener,
and an unrelated continuous tunnel with exact accounting. The shell fixture
owns deny-first routing, fencing, and kernel-resource replacement. Neither
fixture claims Firecracker launch/restore, delayed-SYN authentication, full
NAT/conntrack generation cleanup, or all host bypass topologies.

## Recorded release evidence

Recorded on 2026-09-05. The evaluated source is
`4f84dd7f6a7956874bd021711748a04d05c36ce9`. These are recorded results for that
commit, not a certificate for later changes.

| Check | Recorded result |
| --- | --- |
| Ordinary factory | 224 tests, seven doctests, formatting, Clippy, docs and package verification passed |
| Hostile conformance and fresh dependency policy | Passed |
| Strict resources | All nine lanes passed; maximum sampled RSS 18,912 KiB and post-warmup growth 144 KiB |
| Complete benchmark suites | Passed; this does not imply comparative performance acceptance |
| Rust 1.88 factory, Linux host consumer and Linux management pressure | Passed in a separate same-source rerun after recovering Docker build space |
| Native macOS management pressure | Failed: three close samples had no competing terminal traffic despite close latency remaining within budget |
| Controlled performance acceptance | Failed: identical-source setup measurements were too variable; a preceding run also rejected download variability |

The full report remains `passed: false`. Local raw evidence is under ignored
`target/release-hardening/calibration-3/`; the Linux rerun is under
`target/release-hardening/linux-final/`. Their before/after source fingerprints
match exactly. The Linux supplement resolves the environmental container
failures; it does not override the native pressure or performance failures.
Portable conclusions and rejected experiments are also in the
[engineering log](engineering-log.md).

The next two bounded tasks are:

1. Capture client TCP and listener state across one missing-traffic interval on
   a fixed worker. Distinguish transport stalls from listener/runtime behavior
   before changing production admission or drain logic. Reset-on-drop and fixed
   pacing were tried and rejected as insufficiently repeatable; preserve the
   overlap requirement and failed evidence.
2. Calibrate unchanged-source performance on a quiet release worker. A longer
   fixed throughput workload is a candidate experiment, applied equally to both
   versions. Preserve raw alternating observations and independent budgets;
   neither a lucky rerun nor a wider tolerance settles the existing failures.

Neither failure establishes a production starvation bug or performance
regression. Both remain open evidence gates before a clean go-live verdict.
