# Changelog

All notable changes will be documented here. The format follows Keep a
Changelog and versions follow Semantic Versioning.

## Unreleased

- Add a separate process-wide outbound-dial budget with deadline-bound waiting,
  a distinct `dial-capacity` denial, lease-owned cancellation, and release
  before tunnel lifetime.
- Bound a real two-name CNAME cycle to 16 A/AAAA questions and a no-dial
  `dns-failed` denial, and isolate DNS-wire conformance from the proxy test body.
- Pin an incomplete DNS wire reply to six bounded resolver questions, an
  immediate `dns-failed` denial, zero dial attempts, and exact lease cleanup.
- Prove an allowed CNAME that resolves to a metadata address is rejected after
  real wire alias following and never reaches the connector.
- Extend resource measurement through repeated completed tunnels,
  transfer-limit denials, post-success resets, and pre-DNS denials, with exact
  accounting and per-batch descriptor/thread recovery.
- Forward the exact permitted upload and download prefix before rejecting the
  first read beyond a tunnel byte ceiling, independent of read coalescing.
- Pin upstream refusal before CONNECT success and guest-reset broken-pipe
  accounting after success, including exact completion and denial semantics.
- Isolate resolver construction and bounded lookup in a small internal module
  without changing the public API or total complexity.
- Prove certified lease close cancels Hickory's real wire lookup so late DNS
  failures cannot trigger either UDP or TCP retries for the old lease.
- Add trusted process-wide explicit DNS server configuration, bounded to eight
  socket addresses, with hosts-file isolation and UDP-to-TCP recovery.
- Add an opt-in IANA registry-drift check with reviewed IPv4 and IPv6 CSV
  hashes, without adding a public-network dependency to tests or generation.
- Add a direct loopback TCP control beside the allowed CONNECT benchmark so
  host networking noise can be separated from proxy-path changes.
- Add an opt-in concurrent management soak that holds and closes 64 distinct
  leases together, sampling peak and recovered RSS, threads, and descriptors;
  serialize resource lanes so their process baselines cannot overlap.
- Pin fail-fast global-capacity behavior across two source identities: the
  refusal is attributed to the contender, which recovers on retry after close.
- Prove a permanently full diagnostic channel cannot block 64 concurrent
  policy denials or certified close, while preserving exact final counters.
- Add local UDP DNS conformance proving zero cache capacity requeries and the
  configured TTL ceiling expires both positive and negative answers.
- Pin positive and negative resolver-cache count and TTL ceilings, expose a
  narrowing host configuration, and recheck repeated answers after reuse.
- Distinguish an enforced DNS deadline as `504 dns-timeout` from resolver
  failures and DNS-capacity exhaustion, with zero-dial end-to-end proofs.
- Pin exact CONNECT header byte-limit behavior and reject folded fields,
  controls, whitespace ambiguities, and non-ASCII authority spellings.
- Make the unwind-time Lease Drop proof wait independently for queued stale
  release processing after replacement attachment.
- Prove visible-SNI mode gives valid no-SNI and non-TLS inputs distinct bounded
  denials while forwarding neither input upstream.
- Reject ClientHello SNI lists with multiple hostnames and prove that no
  ambiguous handshake bytes reach the upstream.
- Prove certified close terminates both hostile writers when upload and
  download are simultaneously backpressured.
- Add a reproducible 1 MiB near-terminator header benchmark; measurement
  supports retaining the existing linear scanner without added machinery.
- Give controlled and system resolvers the same absolute DNS name, and pin the
  crate's ASCII/ACE hostname, label-length, case, and trailing-dot boundaries.
- Clarify that Linux `SO_MARK` bypass schemes must remove both `CAP_NET_ADMIN`
  and `CAP_NET_RAW` from every untrusted process sharing the network namespace.
- Prove repeated failed lease closes preserve ownership and an exact nonzero
  usage snapshot until a later retry certifies it as final.
- Prove lease Drop remains non-panicking and releases ownership during unwind
  and after the proxy runtime has stopped.
- Make the Docker source-validation stage offline after dependency warmup and
  discard its compilation tree after collecting conformance executables.
- Prove that legacy numeric host spellings remain on the checked DNS path and
  cannot turn a forbidden answer into an unchecked dial.
- Prove explicit and best-effort proxy shutdown racing both lease close and
  lease drop, including pending-dial cancellation and final ownership release.
- Return a still-owning, permanently stopping proxy when proxy-wide shutdown
  misses its deadline, refuse new attachments, and allow certified retry even
  when a success reply races caller abandonment.
- Pin graceful FIN behavior in both directions and distinguish an upstream RST
  from both normal tunnel completion and a policy denial.
- Prove that 32 simultaneous host-side attachments produce exactly one owner
  for a source identity and keep every losing policy detached.
- Drain the listener's ready accept queue before close certification and before
  installing an identity mapping, preventing an old queued socket from
  inheriting a replacement lease under management-channel pressure.
- Serialize only the stripped conformance executables into the unprivileged
  Docker runner, reducing the verified image content size by 96.4% without
  dropping any of its 112 deterministic cases.
- Let a surviving lease consume the final counters already certified by a
  successful proxy-wide shutdown, including runtime-disconnect races.
- Divide the remaining absolute handshake budget fairly across sequential
  approved-address dial attempts so a pending first address cannot starve a
  reachable fallback or multiply live sockets.
- Require one valid HTTP/1.1 Host field that agrees with the CONNECT
  request-target, while keeping the request-target as the only authority used
  for policy, DNS, and dialing.
- Make destination ports fully deny-by-default: only calls to `allow_port`
  create grants, while the thin executable explicitly retains its HTTPS-only
  port 443 behavior.
- Decode operator-registered RFC 6052 NAT64 prefixes before destination policy
  checks, preventing translated private or metadata IPv4 addresses from
  appearing as ordinary global IPv6 DNS answers.
- Add fixed GREASE cipher-suite and extension conformance through Rustls, ECH
  detection, visible-SNI enforcement, exact forwarding, and accounting.
- Specify and test that `*.example.com` matches subdomains at any depth while
  excluding the suffix apex, consistent with the Smokescreen convention.
- Collapse IPv4-mapped IPv6 source identities into their IPv4 spelling at
  attachment and acceptance so one effective address cannot own two policies.
- Exercise the thin executable wrapper in both native and container factories,
  including its usage error and stdin-EOF lease shutdown paths.
- Reject bracketed CONNECT hosts unless their contents are a supported IPv6
  literal, instead of reinterpreting bracketed DNS or IPvFuture text.
- Restart the identity-reuse quiet period whenever another socket is rejected
  for a revoking lease, including best-effort dropped-lease cleanup.
- Start each connection's absolute handshake and header deadlines when the
  listener accepts its socket, including time spent awaiting the spawned task.
- Scan growing CONNECT headers incrementally instead of repeatedly rescanning
  the accumulated buffer.
- Add paired end-to-end benchmarks for hostname CONNECT with and without
  visible-SNI enforcement.
- Make the local factory compile the assembled crate package and declare its
  future docs.rs location in package metadata.
- Deduplicate approved DNS results in first-seen order so repeated records
  cannot amplify sequential dial attempts.
- Reject a zero header deadline, clamp process semaphore limits to Tokio's safe
  maximum, and return a typed error for an oversized per-lease limit.
- Give the fixed 64-header CONNECT parser ceiling its own bounded
  `too-many-headers` response and diagnostic reason.
- Let a close retry immediately certify an already-quiesced lease without
  repeating the identity-reuse quiet period or changing its final counters.
- Commit final counters under the lease lifecycle lock before close success,
  preventing late unadmitted sockets from mutating `FinalUsage`.
- Require native IPv6 destinations to be inside IANA's `2000::/3` global
  unicast block before applying the smaller special-purpose deny table.
- Reject the full IANA `2001::/23` protocol-assignments umbrella by default,
  closing unassigned IPv6 special-purpose gaps while preserving CIDR override.
- Add opt-in, rate-limited structured denial events through a caller-owned
  bounded channel, without a logging dependency or blocking callback. Events
  retain a non-wrapping lease sequence across source-identity reuse.
- Saturate cumulative usage counters at `u64::MAX` so final accounting cannot
  wrap or panic at the integer boundary.
- Bound accepted DNS answer cardinality and reject oversized sets before any
  address can reach the dialer.
- Extend the absolute handshake deadline through forwarding an approved
  ClientHello, including a constrained-socket cancellation proof.
- Apply the IPv4 forbidden-address floor to mapped, compatible, and
  well-known-NAT64 IPv6 forms, and deny unsafe transition prefixes by default.
- Add opt-in bounded TLS ClientHello inspection with visible-SNI equality,
  explicit ECH policy, and revocation/deadline conformance.
- Add an opt-in sustained local CONNECT harness with concurrency, throughput,
  and p50/p95/p99 setup latency.
- Add an opt-in concurrent tunnel throughput harness with exact directional
  accounting checks.
- Cache debug and release dependency builds separately from source changes in
  the Linux container factory, and include factory scripts in source packages.
- Add a pinned structural and cognitive complexity report with an initial
  evidence baseline and CI output.
- Add a pinned Rust 1.88 Linux container factory with conformance and resource
  smoke entry points.
- Add a controlled dial phase and prove both lease revocation and the absolute
  handshake deadline cancel in-progress connection attempts.
- Bound process-wide concurrent DNS work and prove queued lookup cancellation
  and late-answer safety with a controlled resolver seam.
- Add hostile tunnel conformance for download ceilings and certified shutdown
  with idle, nonreading, and flooding peers.
- Enforce upload ceilings on bytes coalesced with a CONNECT header before DNS
  or dialing, and keep each tunnel's byte ceiling independent while retaining
  lease-wide accounting.
- Add repeatable allowed and denied local connection-setup benchmarks.
- Add an opt-in cross-platform identity-churn resource measurement harness.
- Reject userinfo in CONNECT authority-form and support checked bracketed IPv6
  literals.
- Release closed identity registry entries without allowing delayed cleanup to
  remove a replacement lease.
- Keep a timed-out close's identity unavailable even when cleanup readiness
  races reply delivery.
- Adopt the Sandbox Egress name and package identity.
- Establish the repository, design invariants, contributor factory, initial
  `Proxy / Policy / Lease` API, CONNECT path, tests, and benchmarks.
