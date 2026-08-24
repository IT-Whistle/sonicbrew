# sonicbrew Development Progress (PROGRESS)

> **Document type:** **Development progress tracking document** for the sonicbrew subproject (M07–M14 + binary). Architecture: [ARCHITECTURE](./ARCHITECTURE.md), API: [REST-API](./REST-API.md), nodes: [AUDIO-NODES](./AUDIO-NODES.md), operations: [RUNBOOK](./RUNBOOK.md), decision history: [adr/](./adr/).
>
> **Baseline date:** 2026-08-17 · gw-pulse daemon handshake (verified against real PA 17.0) + gw-alsa .so streaming (aplay end-to-end) + netmap probe (kernel API 14) complete · tests 566
> **Workspace:** 9 crates (8 lib + 1 bin) · **566 unit/integration/property tests + 5 self-tests + 7 criterion benches passing** · **~15,500 lines of Rust source**

---

## 0. TL;DR

- ✅ **All sonicbrew modules (M07–M14) implemented at the library level** (traits + codec/parser/negotiation logic + unit/integration tests). All that remains is wiring up the FreeBSD-only runtimes (libpulse·netmap·libasound FFI daemons).
- ✅ **Outbound flush-gap resolved** — audio-graph-bsd `0.4.0` integrated ([§4 Option A](../../docs/audio-graph-bsd-engine-changes.md) of the engine changes plan). All 4 gateways' `RingSink` → `Graph::add_sink`, server RT loop calls `flush_sinks()` → **bidirectional audio working** (verified by self-test).
- ✅ **Two hot-reload paths proven** — `--hot-reload-test` (`RtHandle::install` arc-swap) + `--live-rebuild-test` (`Graph::from_snapshot` topology-based rebuild, flush-compatible).
- ✅ **audio-engine runtime orchestration + production server integration** — `GraphEngine` + `GatewayBridge` (survives rebuilds) + `spawn_rebuild_task` + `BuiltNode` (flush composition). **`--server-engine` mode**: the running server takes control-api REST topology changes → `from_snapshot` rebuild → engine swap → gateways (bridge) survive → **live reload works** (server alive after REST POST /nodes + /links · metrics recording confirmed).
- ✅ **Operational observability** — M14 monitor + Prometheus `/metrics` HTTP endpoint.
- ✅ **Bluetooth input** — `audio-bluetooth-bsd` integrated behind the `bluetooth` feature gate (compiles on Linux, runs on FreeBSD).
- ✅ **20 audio processing nodes** — `audio-engine::nodes` + `EngineServerFactory` kind dispatch.
- ✅ **NodeSpec typed params** — the `NodeParams` enum enables parameterized creation of every kind via REST. All 20 kinds supported. **400 BadRequest on kind/variant mismatch + kind auto-inferred from variant when missing**. ADR 0005.
- ✅ **Multi-port linking** — `from_port`/`to_port` added to `LinkRequest`. Per-port routing.
- ✅ **API completeness** — added `GET /links` (link listing) and `GET /topology` (full graph snapshot: nodes + links in one request).
- ✅ **Audio file decode integration** — `load_file_source(path, looping)`: audio-codec-bsd (FLAC/WAV/PCM magic-byte sniffing) → full decode → `FileSource` buffer playback. Includes a WAV round-trip test (hand-rolled RIFF writer).
- ✅ **Node chaining integration tests** — 6 scenarios (effect chain / modulation / time effects / aux multi-port distribution / stereo / 7-node full mixing console) verified by chaining a real Graph.
- ✅ **Preset persistence** — `Preset` (full graph: nodes + kind/params + links) JSON export/import + `GET/POST /preset` REST + **server-engine autosave (2s interval, atomic tmp+rename writes) + restore on startup**. kind/params fully restored after restart — verified end-to-end on the real server (boot→POST→kill→restart→GET).
- ✅ **--load-file CLI** — loads an audio file as a FileSource node at server-engine startup (auto-connected to a sink when channels match). `FileSource::into_parts` decomposition API.

---

## 1. Module roster (M07–M14 + binary)

| ID | Crate | Layer | Pri | Status | Tests | Lines | Key technology |
|----|----------|------|-----|------|:---:|:---:|------|
| **M07** | `session-store` | L3 | P1 | ✅ P1 openraft multi-node | 31 | ~2300 | `SessionStore` trait + `DistributedRaftEngine` (openraft 0.9.25: leader election/log replication/snapshot + `RaftLogStore` + `RaftStateMachine` + cluster integration tests) + `RaftEngine` (single-node redB WAL persistence + in-memory `TopologySnapshot` + tokio broadcast). |
| **M09** | `net-rtp-aes67` | L4 | P1 | ✅ codec+transport+loop+jitter (integrated) | 26 | ~1200 | RTP RFC 3550 codec + L16/L24 framing + `UdpTransport` (`recv_rtp_with_seq`) + recv/send worker loops + **`JitterBuffer` (wrap-aware reordering) integrated into the recv loop** (in-order decoding + loss skip). netmap is a `cfg(freebsd)` stub. |
| **M10** | `gw-pulse` | L5 | P1 | ✅ parser+gateway | 19 | 670 | PulseAudio native protocol parser (20B header + SampleSpec + strings) + `PulseGateway` register. libpulse FFI deferred. |
| **M11** | `gw-alsa` | L5 | P2 | ✅ domain+gateway | 17 | 760 | ALSA `snd_pcm_format_t` subset + hw_params constraint-narrowing negotiation + format mapping + `AlsaGateway`. libasound .so behind a feature gate. |
| **M12** | `gw-browser` | L5 | P0 | ✅ MVP | 17 | 832 | WebSocket-only gateway (tokio-tungstenite) + audio-graph-bsd `RingSource`/`RingSink` + 6-byte wire format + Opus sub-path (`opus` feature). |
| **M13** | `control-api` | L5 | P0 | ✅ MVP + CRUD + topology | 85 | ~1000 | `ControlApi` trait + REST (`GET/POST/DELETE /nodes`·`/links` + **`GET /topology`**) + `kind`/`params` + shared `KindRegistry`/`ParamsRegistry` + `NodeParams` typed enum (20 kinds) + **multi-port linking** (`from_port`/`to_port` + validation). |
| **M14** | `monitor` | L5 | P1 | ✅ + `/metrics` | 6 | 450 | `MetricsRecorder` (latency p50/p99 + xrun + Prometheus export) + `serve_metrics` raw-HTTP. kqueue is a `cfg(freebsd)` stub. |
| — | `sonicbrew` | (bin) | — | ✅ integration + engine server + typed params | 5 self-test | ~1800 | Server entry point: default mode (RT loop + WS/REST/`/metrics` + Bluetooth) + **`--server-engine`** (audio-engine + GatewayBridge + serve_with_io + spawn_rebuild_task — production live reload) + **`render_node`** (kind+params dispatch, 20 kinds) + 5 deterministic self-tests. |
| — | `audio-engine` | (runtime) | — | ✅ live rebuild + gateway bridge + 20 processing nodes | 84 | ~2100 | `GraphEngine` + `build_graph` + `NodeFactory` + `spawn_rebuild_task` + `builtins` (3 kinds) + **`GatewayBridge`** + **20 audio nodes** (mixer/aux_send/eq/compressor/limiter/meter/channel_map/delay/noise_gate/noise_source/tone_generator/reverb/chorus/flanger/distortion/phaser/bitcrusher/file_source/tremolo/stereo_widener). |

> **Test totals:** 566 unit/integration/property tests + 5 binary self-tests + 7 criterion benches. Linux + FreeBSD native both GREEN. fmt/clippy (`-D warnings`) / FreeBSD `cargo check` all GREEN.

---

## 2. Cross-cutting features

### 2.1 Bidirectional audio (flush-gap resolved) ★
Integrates audio-graph-bsd 0.4.0's `Flushable`/`SinkNode`/`Graph::add_sink`/`flush_sinks` (= [engine changes plan](../../docs/audio-graph-bsd-engine-changes.md) §4 Option A):
- The 4 gateways' (M10/M11/M12/M09) `RingSink` registration: `add_node` → **`add_sink`** (flushable tracking).
- Server RT loop: every cycle, **`flush_sinks()`** (off-RT) after `process_cycle` → stash handed off to the outbound `rtrb`.
- **Verification:** `--self-test` → `outbound peak=0.4999 after flush_sinks (1 sink)` (inbound = outbound peak match → proof of bidirectional flow).

### 2.2 Hot-reload (2 paths)
- `--hot-reload-test`: `RtHandle::install` (arc-swap atomic replacement) — `gain 1.0→0.5 live swap: peak 0.5000→0.2500 (ratio 0.500)`.
- `--live-rebuild-test`: `Graph::from_snapshot(snapshot, factory)` topology-based rebuild + owned-Graph swap — **flush-compatible** (avoids the `&mut`/`&self` conflict). sonicbrew-side `NodeId→Kind` registry factory (NodeSnapshot carries no type tag, so a side table is maintained).
- **Design note:** the server RT loop **owns the Graph by value** for flush; `RtHandle` is proven by a separate test. Unifying flush (`&mut`) + `RtHandle` (`&self`) is follow-up work.

### 2.3 Observability
- `MetricsRecorder`: per-cycle `process_cycle` latency (µs) measurement → p50/p99/min/max/avg + xrun counters.
- `serve_metrics`: raw-HTTP `GET /metrics` → Prometheus text (`--metrics-addr`, default 9003).

### 2.4 Bluetooth input
- `audio-bluetooth-bsd` 0.1.0 (A2DP input backend) integrated behind the `bluetooth` feature (off by default). `bt_input.rs` bridges `BtInputSource`→rtrb→`RingSource`. `cfg(freebsd)` gate: compiles on Linux, runs on FreeBSD.

---

## 3. How to run

```bash
# Build / lint / test (Linux dev host)
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                    # 566 passed

# FreeBSD target typecheck (full build/run in bhyve VM)
cargo check --workspace --target x86_64-unknown-freebsd

# Deterministic self-tests (no server needed)
cargo run -p sonicbrew -- --self-test           # bidirectional flush + monitor
cargo run -p sonicbrew -- --hot-reload-test     # RtHandle arc-swap
cargo run -p sonicbrew -- --live-rebuild-test   # from_snapshot topology rebuild
cargo run -p sonicbrew -- --engine-live-rebuild-test  # audio-engine end-to-end (RaftEngine→TopologyEvent→rebuild→swap)
cargo run -p sonicbrew -- --gateway-live-reload-test  # gateway bridge live reload (WS workers survive the rebuild)

# Server mode (REST + WS + /metrics)
cargo run -p sonicbrew -- --ws-addr 127.0.0.1:9001 --api-addr 127.0.0.1:9002 --metrics-addr 127.0.0.1:9003
#   GET  http://127.0.0.1:9002/nodes        → node list (JSON)
#   POST http://127.0.0.1:9002/nodes        → 201 + NodeId
#   POST http://127.0.0.1:9002/links        → 201 + LinkId
#   GET  http://127.0.0.1:9003/metrics       → Prometheus text

# Optional features
cargo check -p sonicbrew --features bluetooth   # Bluetooth input typecheck
cargo check -p gw-browser --features opus       # Opus sub-path typecheck (libopus linking on FreeBSD)
```

---

## 4. Dependencies (crates.io)

audio-toolkit family (all from crates.io, no path deps):
| Crate | Version | Notes |
|---|---|---|
| `audio-graph-bsd` | **0.4.0** | `topology`+`distributed` features. Provides the flush API (`Flushable`/`add_sink`/`flush_sinks`) + `RtHandle` + `from_snapshot`. |
| `audio-core-bsd` | 0.1.1 | Shared `AudioNode`/`AudioFrame`/`ProcessContext`/`PortDescriptor`. |
| `audio-resample-bsd` | 0.2.1 | rubato RT-safe. |
| `audio-opus-bsd` | 0.1.2 | Opus codec (gw-browser `opus` feature, libopus linking). |
| `audio-dsp-bsd` / `audio-codec-bsd` / `audio-io-bsd` / `audio-plugin-bsd` / `audio-clock-bsd` | 0.3.1 / 0.1.1 / 0.3.1 / 0.1.1 / 0.1.1 | Catalog registration (some not yet consumed). |
| `audio-bluetooth-bsd` | 0.1.0 | BT A2DP input (this project's 10th toolkit crate, `bluetooth` feature). |

Common: tokio, redb, axum, hyper, tokio-tungstenite, metrics, tracing, rtrb, thiserror(2), serde, arc-swap(distributed).
| `openraft` | **0.9.25** | Distributed Raft consensus (M07 `DistributedRaftEngine`: leader election/log replication/snapshot). `storage-v2` + `single-snapshot-data` features. |

---

## 5. Decisions / ADR

| ADR | Content | Location |
|-----|------|------|
| **0002** | `audio-bluetooth-bsd` backend approved (L1, not a sonicbrew module, feature-gate) | `docs/adr/0002-audio-bluetooth-bsd-backend.md` |
| **0003** | openraft multi-node consensus (M07 session-store: single-node→multi-node, `DistributedRaftEngine`, WAL schema compatibility) | `docs/adr/0003-openraft-multi-node-consensus.md` |
| **0004** | 6 audio processing nodes (audio-engine::nodes + EngineServerFactory kind dispatch, shipped with default parameters) | `docs/adr/0004-audio-processing-nodes.md` |
| **0005** | NodeSpec typed params (REST parameterized node creation, `NodeParams` enum + `ParamsRegistry`) | `docs/adr/0005-nodespec-typed-params.md` |
| **0006** | Multi-port linking + GET /links + GET /topology (`LinkRequest` from_port/to_port + `LinkInfo` + `TopologyInfo`) | `docs/adr/0006-multi-port-linking-and-topology.md` |
| — | audio-graph-bsd engine changes (flush accessor) plan — **§4 implementation complete in 0.4.0** | `../../docs/audio-graph-bsd-engine-changes.md` |

---

## 6. Future work

> The build roadmap (Phases 1–5, M07–M14) is **closed with all modules complete** — see the §1 module roster and [adr/](./adr/) for the completion history. Only environment-/upstream-dependent items remain below.

### FreeBSD VM environment needed
| Item | Current status |
|------|-----------|
| ✅ **FreeBSD regression suite — ALL GREEN (2026-08-18)** | `scripts/freebsd-regression.sh` (7 sections) on the dedicated 15.1 machine: fmt+clippy · workspace build · **573/573 tests** · 5 self-tests · server-engine smoke (live reload + preset persistence) · gw-pulse live handshake (+ `--play` playback) · netmap ring I/O (`nm_port_test`). One-shot rerunnable as 8 sections (VALE RTP loopback included as §8). **Nightly automation live: `scripts/nightly-regression.sh` via cron at 03:30 (14-day log retention under `logs/`, `logs/LAST_FAILED` marker on failure)** |
| ✅ **gw-pulse live daemon + playback streaming (2026-08-17)** | **Verified against real PulseAudio 17.0**: AUTH (cookie) + SET_CLIENT_NAME + GET_SERVER_INFO round trip (protocol v35, `examples/handshake` EXIT=0). **`--play` end-to-end: CREATE_PLAYBACK_STREAM (index 0) + 144,000 frames (3 s 440 Hz sine FLOAT32LE) memblock writes + DELETE — PLAY_OK.** Pure-Rust Unix-socket implementation — no libpulse FFI. tagstruct bugs fixed against upstream C sources: channel=0xFFFFFFFF, ASCII tags, NUL-terminated strings, proplist double-encoding, **cvolume raw fields (no per-value tags)**, CREATE payload v35 field order |
| ✅ **gw-alsa PCM plugin (2026-08-17)** | **aplay end-to-end streaming verified**: libasound dlopens the .so → FLOAT_LE negotiation → bridge TCP handshake (magic 0x53424E52) → 24,000 f32 frames received. ABI version symbol (`__snd_pcm_sonicbrew_open_dlsym_pcm_001`) + ioplug set_param_list ordering (after create) fixed |
| ✅ **netmap capability probe (2026-08-17)** | `examples/netmap_probe`: NIOCGINFO API 14 negotiation against the real kernel + successful geometry queries for the vmx0 NIC (tx/rx 4 rings × 512 slots) and VALE vale1:1 (1 ring × 1024 slots). First runtime step of the zero-copy backend |
| ✅ **netmap zero-copy RTP — FULL loopback verified (2026-08-18)** | **`vale_loopback` LOOPBACK_PASS: 8 RTP packets sent through one VALE port's TX ring and received intact (bit-exact payload) from the peer port's delivery ring — the complete zero-copy path (RTP encode → TX slot → kernel VALE switch → RX slot → RTP parse) live-verified.** The three secrets unlocked by systematic C-matrix experimentation: (1) **memid pinning** — the kernel assigns DIFFERENT memory allocators to independently registered ports (reader=2, writer=3 → zero switching); all ports of a switch must share the first port's `nr_arg2` (`open_with_memid`); (2) **fresh-ring priming** — a fresh VALE RX/delivery ring's slot descriptors are stale garbage until acknowledged (`prime_rx`: head=cur=tail + rxsync); (3) **TX slot accounting** — after txsync the kernel reports `tail` = last-consumed+1 (head=1,tail=0 means 1 in flight, NOT full); free = num_slots − (head−tail mod n). Ring roles on a vale port: SEND on `ring_ofs[0]` (dir=1), DELIVERY arrives on `ring_ofs[tx+host_tx]` (dir=0). Also fixed: sync ioctls pass 0 (not null) matching C callers; the earlier `4/8 deliveries` C artifact was the same accounting bug |

**2026-08-18 machine-down incident — RESOLVED (not a kernel panic):** an "unreachable" window during VALE debugging was a client-side VPN dropout. After the machine returned (new DHCP lease 192.168.62.107), `/var/crash` was empty, `/var/log/messages` had no panic lines, and the full regression suite passed unchanged — the netmap experiments were never the cause. Investigation hardening was kept (sync ioctls pass `0`, not `ptr::null_mut()`). |
| OSS/cpal real output | No audio PCI device in the VM (no /dev/dsp) — p11 §8.4 demo + latency measurement once hardware is available. The `audio-io-bsd` cpal gate is kept |
| Bluetooth A2DP runtime | Compile-only verification on Linux (`bluetooth` feature) |

### upstream (audio-toolkit) dependencies
| Item | Note |
|------|------|
| flush + RtHandle single integration | Reconciling `flush_sinks(&mut)` ↔ `RtHandle(&self)` — design complete ([engine changes plan](../../docs/audio-graph-bsd-engine-changes.md) §5), requires audio-graph-bsd 0.5 |
| `NodeSnapshot` kind/params fields | Root fix is persistence — currently worked around via the preset sidecar (§0) |

### Next decision points (conditional promotion)
- **AES67 full compliance** (SAP/SDP, FEC, SRTP, PTP M16) — when expanding to distributed
- **WebRTC** (str0m) — G4 phase after WebSocket MVP validation
- **pre/post-fader send** — AuxSendNode extension
- **Direct OSS backend** — if cpal-via-ALSA latency >10ms (p11 decision #5)

---

## 7. Known limitations

- **Dev host**: Linux x86_64 (no libasound/libopus/cpal) → cpal/Opus/netmap/libpulse are feature-gated or `cfg(freebsd)`. Full FreeBSD build/run is the bhyve VM regression layer (TESTING-STANDARDS Layer 4).
- **flush vs RtHandle**: the server RT loop owns the Graph by value for flush (`&mut`); `RtHandle` is covered by a separate test. Unifying them is upstream work (§6).
- **Live rebuild + gateway rtrb**: recreating gateway nodes allocates new rtrb rings → the WS worker's existing handles are severed. Internal-node (DSP/gain) rebuilds work fine; a gateway-surviving rebuild is a tracked task.
- **Cargo.lock**: committed because this is a binary/server (.gitignore policy).

---

**Related documents:** [ARCHITECTURE](./ARCHITECTURE.md) · [REST-API](./REST-API.md) · [AUDIO-NODES](./AUDIO-NODES.md) · [RUNBOOK](./RUNBOOK.md) · [INTERNALS](./INTERNALS.md) · [CONCEPTS](./CONCEPTS.md) · [FREEBSD](./FREEBSD.md) · [TEST-LAYERS](./TEST-LAYERS.md) · [GOVERNANCE](./GOVERNANCE.md) · [TESTING-STANDARDS](../../TESTING-STANDARDS.md) · [engine changes plan](../../docs/audio-graph-bsd-engine-changes.md)
