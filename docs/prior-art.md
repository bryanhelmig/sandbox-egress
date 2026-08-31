# Prior art

Reviewed on 2026-08-31. Commit pins make future comparisons reproducible; links
remain upstream-owned and are not vendored.

| Project | Reviewed commit | What to learn | Gap this crate targets |
| --- | --- | --- | --- |
| [Stripe Smokescreen](https://github.com/stripe/smokescreen) | `d4da883a` | ACL and IP filtering, operational limits, diagnostics | Go daemon; no run lease |
| [lens-sandbox-core](https://github.com/lensapp/lens-sandbox-core) | `a0a95786` | broad Rust DNS/proxy/TLS/policy implementation | shared mutable policy and detached connection lifecycle |
| [nono](https://github.com/nolabs-ai/nono) | `8f15fc86` | supervisor-side proxy, credential boundary, audit | guest session token and accept-loop shutdown, not certified close |
| [motosan-sandbox](https://github.com/motosan-dev/motosan-sandbox) | `13eab245` | small per-run CONNECT proxy and hard routing | one proxy per run; spawned tunnels are not a shared lease |
| [ressrf](https://github.com/timescale/ressrf) | `52fc89cf` | generated forbidden ranges, DNS-pinned transports, fuzz vectors | policy/transport components rather than lease ownership |
| [canister](https://github.com/dergraf/canister) | `27434158` | hostile L7 contracts, body limits, DLP | sandbox product, not reusable lifecycle primitive |
| [eavs](https://github.com/byteowlz/eavs) | `afa178a0` | transparent destination recovery and SNI/Host ACLs | no ephemeral run ownership |
| [microsandbox](https://github.com/superradcompany/microsandbox) | `5b1c63d9` | network-layer DNS timeout/rebinding controls | microVM network subsystem, not forward-proxy lease API |

Also relevant: Rama for composable mature proxy machinery and VEY/G3 for
production daemon limits, metrics, ACLs, and per-user policy.

## Research rule

Copy concepts, tests, and threat-model lessons—not code—unless license and
provenance are reviewed. Record new sources and exact commits here before a
design meaningfully depends on them.

