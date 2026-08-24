# sonicbrew · Unified Server

> **Unified FreeBSD high-performance distributed audio server (the sonicbrew app)** — treats the entire network topology as a single audio graph, providing a compatibility gateway, session, distributed transport, and control layer that lets existing Linux apps (PulseAudio/ALSA) and browsers (WebAudio) connect without modification.

---

## Shared Goal

We address the real gap in the FreeBSD audio ecosystem: the **lack of an integration layer** ([P01](../notes/p10-architecture-design.md) §10). The sonicbrew subproject owns distributed session consensus, network transport, protocol-compatible gateways, the control API, and observability, while core audio processing (graph engine, DSP, codecs, kernel I/O) is delegated to the **[`audio-toolkit`](../audio-toolkit/)** subproject.

> **Dependency relationship:** `sonicbrew` → `audio-toolkit` (**10 crates**, consumed from crates.io — building does not require the `../audio-toolkit/` sibling; for local co-development switch to path deps, see [CONTRIBUTING §1.2](./CONTRIBUTING.md)). `audio-bluetooth-bsd` is a Bluetooth A2DP **input backend** that enters through the same backend abstraction as any other audio device. The gateways (`gw-*`) and `net-rtp-aes67` implement audio-toolkit's `AudioNode` trait so they plug into the core graph as nodes.

---

## Modules

| Crate | Role (1 line) |
|----------|-----------|
| `session-store` | `openraft` multi-node consensus + `SessionStore` + WAL persistence (redb). Single-node `RaftEngine` + distributed `DistributedRaftEngine`. |
| `net-rtp-aes67` | RTP/AES67 zero-copy send/receive (`netmap(4)`). Professional audio transport between gateways/distributed nodes. |
| `gw-pulse` | PulseAudio protocol compatibility — Linux apps connect unmodified. |
| `gw-alsa` | ALSA(L) PCM plugin emulation — Linux ALSA app compatibility. |
| `gw-browser` | WebAudio-WebSocket gateway (WebRTC/HLS are stubs). |
| `control-api` | REST control plane — `GET/POST/DELETE /nodes`, `/links` + `GET /topology` + `NodeParams` typed params (20 kinds) + multi-port links. |
| `monitor` | Low-overhead metrics/tracing (latency, xrun counters) + Prometheus `/metrics`. |
| `audio-engine` | Runtime orchestration — `GraphEngine` + live rebuild + gateway bridging + **20 audio nodes** (EQ/compressor/reverb/delay/chorus/flanger/phaser/distortion/bitcrusher/tremolo, etc.). |
| `sonicbrew` (bin) | Server binary — unified entry point integrating the modules + audio-engine. `--server-engine` live-reload mode. |

> Each module implements or consumes audio-toolkit's `AudioNode`/`Gateway`/`SessionStore`/`NetworkAudioTransport`/`ControlApi`/`MetricsSink` traits.

---

## Status

- ✅ **Implementation complete** — 9 crates (8 lib + 1 bin) · **535 tests passing** · 23 AudioNodes (4 sources + 16 effects + 3 mixing/routing) · REST CRUD + typed params + multi-port links + `GET /topology` · live reload (`--server-engine`) · openraft multi-node consensus. Details in [PROGRESS.md](./docs/PROGRESS.md).

---

## Environments

> Aligned with the umbrella [GOVERNANCE](../GOVERNANCE.md) §3 environment system.

| Environment | Description | Purpose |
|------|------|------|
| **local** | Dev PC (Linux or FreeBSD host) | Unit tests, `cargo build` |
| **vm** | bhyve/VirtualBox — FreeBSD 14.2-RELEASE VM | Build/integration verification, xrun/alloc regressions |
| **staging** | jail isolation or separate host — production mirror | Pre-release verification (masked data) |
| **production** | FreeBSD host — `carp(4)` HA | Live service |

**Targets:** FreeBSD `14.2-RELEASE` (minimum patch p3) · Rust 2021 ed. (MSRV 1.85+).

---

## Layout

```
sonicbrew/
├── crates/
│   ├── session-store/          # session consensus + WAL (single-node + openraft multi-node)
│   ├── net-rtp-aes67/          # RTP/AES67 transport + jitter buffer
│   ├── gw-pulse/               # PulseAudio compatibility
│   ├── gw-alsa/                # ALSA(L) emulation
│   ├── gw-browser/             # WebSocket gateway
│   ├── control-api/            # REST control API
│   ├── monitor/                # observability (Prometheus)
│   ├── audio-engine/           # Runtime orchestration + 20 audio nodes
│   └── sonicbrew/              # Unified binary
├── README.md                   # This file
├── CONTRIBUTING.md             # Contribution guide
├── CODEOWNERS                  # Ownership rules
└── docs/                       # Project docs (all tracked)
    ├── ARCHITECTURE.md         # Architecture (as-implemented)
    ├── REST-API.md             # Control API reference
    ├── AUDIO-NODES.md          # Audio node catalog (23 nodes)
    ├── RUNBOOK.md              # Build/run/operations guide
    ├── PROGRESS.md             # Development progress + future work
    ├── TEST-LAYERS.md          # Test coverage matrix
    ├── PERFORMANCE.md          # Performance metrics/targets/measurement criteria
    ├── KNOWLEDGE.md            # Per-module domain knowledge base
    ├── GOVERNANCE.md           # Governance/policies
    └── adr/                    # Architecture decision records (0002–0006)
```

> **Documentation policy:** All project documents are maintained as regular (tracked) docs under `docs/`. The former "planning docs local-only" policy and ROADMAP/BUILD-PLAN (retired once all phases completed) were cleaned up on 2026-08-08.

---

## Related Documentation

| Document | Location | Description |
|------|------|------|
| [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) | docs/ | Architecture (as-implemented) |
| [docs/REST-API.md](./docs/REST-API.md) | docs/ | Control API reference (10 endpoints) |
| [docs/AUDIO-NODES.md](./docs/AUDIO-NODES.md) | docs/ | Audio node catalog (23 nodes / 20 kinds) |
| [docs/RUNBOOK.md](./docs/RUNBOOK.md) | docs/ | Build/run/operations guide |
| [docs/INTERNALS.md](./docs/INTERNALS.md) | docs/ | Internal mechanics deep-dive (live-reload, persistence, gateways) |
| [docs/CONCEPTS.md](./docs/CONCEPTS.md) | docs/ | Domain concepts (DSP math, RTP/AES67, PulseAudio/ALSA protocols, Raft) |
| [docs/FREEBSD.md](./docs/FREEBSD.md) | docs/ | FreeBSD integration (netmap zero-copy, VALE switching, OSS, kqueue) |
| [docs/PROGRESS.md](./docs/PROGRESS.md) | docs/ | Progress + future work |
| [docs/adr/](./docs/adr/) | docs/ | Architecture decision records |
| [CONTRIBUTING.md](./CONTRIBUTING.md) | sonicbrew/ | Development environment, test rules |
| [CODEOWNERS](./CODEOWNERS) | sonicbrew/ | Per-module ownership |
| [umbrella README](../README.md) | ../ | Full sonicbrew introduction |
| [audio-toolkit](../audio-toolkit/) | ../ | Core audio processing subproject |

---

## Quick Start

> See [CONTRIBUTING.md](./CONTRIBUTING.md) for detailed steps.

```bash
# Prerequisites: Rust 1.85+ (MSRV), FreeBSD 14.2 environment (or bhyve VM)
# audio-toolkit deps resolve automatically from crates.io — no ../audio-toolkit/ sibling needed.
# (To use path deps for local co-development, see CONTRIBUTING §1.2)

rustup target add x86_64-unknown-freebsd

# Build with audio-toolkit dependencies
cargo build
cargo check --target x86_64-unknown-freebsd

# Test / lint
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```
