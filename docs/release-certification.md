# Explicit release checks

`python3 scripts/certify-release.py --baseline COMMIT --output NEW_DIRECTORY`
runs the maintainer factory from a committed, clean source tree. It does not
publish code or a crate, change hosted CI, or replace an independent security
review. Python 3, Cargo with the pinned toolchain, cargo-deny, SCC 4.0.0, and a
working Docker daemon are required. Docker runs the host fixture in a disposable
privileged container with its external network disconnected.

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

Required lanes are ordinary checks, hostile conformance, a fresh dependency
policy check, strict resource certification, native management pressure, source
complexity, the Rust 1.88 container factory, the external host consumer, Linux
management pressure, complete Criterion suites, and a controlled performance comparison. The advisory
database is freshly fetched into the evidence directory and its commit is
recorded; network access for dependency/tool setup is separate from local test
traffic. No registry credential or upload command is used.

## Performance acceptance

The baseline must be an explicitly selected reviewed commit with identical
benchmark code, throughput workload, Cargo manifest and lockfile, toolchain, and Cargo
configuration. If the oracle or workload changes, establish a new baseline;
do not compare incompatible definitions. `--baseline HEAD` is useful for an
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
