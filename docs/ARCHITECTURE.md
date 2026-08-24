# sonicbrew Architecture

> **Document status:** sonicbrew workspace architecture as of implementation completion. For design history, see [ADR](./adr/) and the umbrella design documents (`../docs/p01–p11`).

---

## 1. Overview

sonicbrew is a **FreeBSD-based high-performance distributed audio server**. It treats the entire network topology as a single audio graph, providing a compatibility gateway where Linux apps (PulseAudio/ALSA) and browsers (WebAudio) connect without modification, plus session consensus, distributed transport, a control API, and an observability layer. Core audio processing (graph engine, DSP, codecs, kernel I/O) is delegated to **audio-toolkit** (the `-bsd` crate family on crates.io).

## 2. Crate Layout (9 total: 8 lib + 1 bin)

| Crate | Role |
|----------|------|
| `session-store` | `SessionStore` trait + `RaftEngine` (single-node, redb WAL) + `DistributedRaftEngine` (openraft 0.9 multi-node: leader election/log replication/snapshots) — the single source of truth for topology |
| `net-rtp-aes67` | RTP RFC 3550 codec + L16/L24 framing + UDP transport + jitter buffer (wrap-aware reordering) + recv/send worker loops. netmap is `cfg(freebsd)` gated |
| `gw-pulse` | PulseAudio native protocol parser + `PulseGateway`. libpulse FFI daemon handshake is future work (FreeBSD) |
| `gw-alsa` | ALSA format domain + hw_params negotiation + `AlsaGateway`. libasound .so plugin is future work |
| `gw-browser` | WebSocket gateway (tokio-tungstenite) + `RingSource`/`RingSink` + 6-byte wire format + Opus sub-path (`opus` feature) |
| `control-api` | REST (axum): node/link CRUD + `GET /topology` + preset export/import + `NodeParams` typed params (20 kinds) — [REST-API.md](./REST-API.md) |
| `monitor` | `MetricsRecorder` (latency p50/p99 + xruns) + Prometheus `/metrics` raw-HTTP |
| `audio-engine` | runtime | `GraphEngine` (process+flush+swap) + `build_graph` + `GatewayBridge` (survives rebuilds) + `spawn_rebuild_task` (TopologyEvent-driven) + **20 audio nodes** — [AUDIO-NODES.md](./AUDIO-NODES.md) |
| `sonicbrew` (bin) | — | Integrated entry point. Default mode + `--server-engine` (live reload) — [RUNBOOK.md](./RUNBOOK.md) |

## 3. Layer Structure

```
┌─────────────────────────────────────────────────────────────┐
│ APIs & Gateways (sonicbrew)                                 │
│   gw-pulse · gw-alsa · gw-browser · control-api · monitor   │
├─────────────────────────────────────────────────────────────┤
│ Network Protocols                                           │
│   net-rtp-aes67(sonicbrew) · audio-opus/audio-codec(toolkit)│
├─────────────────────────────────────────────────────────────┤
│ Session & Persistence (sonicbrew)                           │
│   session-store (Raft consensus + WAL)                      │
├═════════════════════════════════════════════════════════════╡
│    ▲▼ AudioNode trait (audio-core-bsd) — insertion contract │
├─────────────────────────────────────────────────────────────┤
│ Core Graph & DSP (audio-toolkit)                            │
│   audio-graph-bsd · audio-dsp-bsd · audio-resample-bsd      │
├─────────────────────────────────────────────────────────────┤
│ Kernel Interface (audio-toolkit)                            │
│   audio-io-bsd (OSS /dev/dsp + cpal ALSA)                   │
└─────────────────────────────────────────────────────────────┘
```

**Key boundary:** sonicbrew's gateways (`gw-*`) and `net-rtp-aes67` implement audio-toolkit's `AudioNode` trait, plugging into the `audio-graph-bsd` Graph as Source/Sink nodes. This trait is the sole runtime coupling contract between sonicbrew and audio-toolkit; all dependencies are consumed from crates.io.

## 4. Runtime Data Flow (server-engine mode)

```
Browser ──WS──► gw-browser (serve_with_io)
                    │ push_inbound / pop_outbound (GatewayBridge, rtrb ring)
                    ▼
   ┌──────────────── RT thread (GraphEngine) ────────────────┐
   │  RingSource ─► [node chain: EQ → Reverb → Mixer → ...]  │
   │       process_cycle (RT, alloc-free)                    │
   │       flush_sinks  (between cycles → outbound ring)     │
   │       rebuild swap  (between cycles, atomic graph swap) │
   └──────────────────────────┬──────────────────────────────┘
                              ▼ RingSink ──► gw-browser ──WS──► Browser

   REST(control-api) ─► session-store(redb WAL) ─► TopologyEvent broadcast
                              ▼
        spawn_rebuild_task: snapshot + factory(KindRegistry/ParamsRegistry/
        FileBufferRegistry → render_node) → build_graph → RebuildSlot
```

### Live reload

1. `POST /nodes`/`/links` → session store mutation + WAL persistence + `TopologyEvent` broadcast
2. The rebuild thread receives the event → `build_graph` from the current snapshot (the factory renders nodes from kind+params)
3. The RT thread swaps in the new graph between cycles — gateway workers survive transparently on new rtrb handles from `GatewayBridge`

### RT safety model

sonicbrew's RT loop is a polling `std::thread` (not SCHED_FIFO). `process_cycle` forbids alloc/lock/panic (TESTING-STANDARDS §3.2); `flush_sinks` and the rebuild swap run **between** cycles. Every audio node allocates only in its constructor, and `process` does only bounded arithmetic.

## 5. Persistence (2 axes)

| Store | Contents | When |
|--------|----------|------|
| `sonicbrew-dev.redb` (session-store WAL) | topology (nodes/edges) | on every mutation |
| `sonicbrew-dev.preset.json` (preset sidecar) | full graph state + **kind/params** + labels | auto-save every 2s (change detection, atomic tmp+rename write) |

At boot, if the preset sidecar exists the state is restored via `import_preset` (kind/params included — this resolves the restart-loss problem). The fundamental fix (a kind field on NodeSnapshot) is upstream work.

## 6. Dependency Policy

- **audio-toolkit 10 crates**: all pinned versions from crates.io. No `../audio-toolkit/` sibling needed.
- **Key externals**: tokio, redb, axum, tokio-tungstenite, openraft 0.9, symphonia (via audio-codec-bsd), rtrb, thiserror, serde.
- **FreeBSD-only features** (`bluetooth` feature, netmap, libpulse/libasound FFI): feature-gated or `cfg(freebsd)` so Linux dev hosts keep compiling.

## 7. Related Documents

- [REST-API.md](./REST-API.md) — control API reference
- [AUDIO-NODES.md](./AUDIO-NODES.md) — audio node catalog (23 nodes)
- [RUNBOOK.md](./RUNBOOK.md) — build/run/operations guide
- [TEST-LAYERS.md](./TEST-LAYERS.md) — test coverage matrix
- [KNOWLEDGE.md](./KNOWLEDGE.md) — per-module domain knowledge base
- [GOVERNANCE.md](./GOVERNANCE.md) — governance/policies
- [adr/](./adr/) — architecture decision records (0002–0006)
