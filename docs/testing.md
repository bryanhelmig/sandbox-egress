# Testing strategy

The suite is organized by claimed invariant rather than by source module.

- Unit tests: hostname and authority canonicalization, forbidden ranges,
  builder validation, counters, and state transitions.
- Integration tests: real listener, real CONNECT client, pinned local upstream,
  allow/deny behavior, limits, accounting, and shutdown.
- Concurrency tests: attach collisions, admission-versus-close races, many
  simultaneous tunnels, and identity reuse after certified close.
- Hostile conformance tests: deterministic phase barriers for headers, DNS,
  dial, ClientHello, and tunnel. Each phase must prove close returns only after
  its work is gone.
- Resource soak: repeated abuse with sampled RSS, thread count, and descriptor
  count. Platform-specific collectors report unsupported rather than silently
  passing.
- Fuzzing: parsers and policy normalization, added once their internal seams
  stabilize. Seed regressions remain ordinary tests.

No test may depend on the public internet. DNS and upstream behavior must be
locally controlled so failures are reproducible.

Performance gates begin as recorded baselines, not brittle absolute numbers.
Benchmarks cover attach/close, policy matching, admission contention, and
accounting overhead. Macrobenchmarks later report connections/sec, throughput,
p50/p95/p99 setup latency, peak RSS, threads, and file descriptors.

