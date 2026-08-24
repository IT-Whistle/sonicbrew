# sonicbrew server — Construction Knowledge Base (KNOWLEDGE)

> **Document role:** A knowledge base distilled for anyone building the sonicbrew workspace (M07·M09·M10·M11·M12·M13·M14 + the `sonicbrew` binary), around the question **"what must you know to build each module?"** It is **not a republication** of the source research (P01–P09 HTML) or the unified architecture ([p10 design](../../notes/p10-architecture-design.md), [p11 mvp](../../notes/p11-mvp-scope-design.md)) — only the construction-relevant key facts + section citations remain.
>
> **Baseline:** FreeBSD `14.2-RELEASE` (P01) · Rust 2021 ed. (MSRV 1.85+) · written 2026-07-29
> **Workspace relationship:** `sonicbrew/` **depends on `audio-toolkit/`** (audio core, the `-bsd` crate family) — gateways (gw-*) and net-rtp implement audio-toolkit's `AudioNode` trait (`audio-core-bsd`) to be inserted into the graph ([p10 §6.2](../../notes/p10-architecture-design.md), this document §8.1). **Audio asset (sample/IR/preset) loading is audio-toolkit's local file I/O** (`audio-codec-bsd` FLAC/WAV/PCM decode, `audio-dsp-bsd` IR loading) — **there is no sonicbrew-side asset module/repository** (§8.2).
> **Scope:** this work = pre-code (documentation only). The trait signatures below are **contracts (specifications)** excerpted from P10 §8, not implementations.

---

## How to read

Each module section consists of the following blocks:

- **Role** (one line) — why this module exists from the sonicbrew (server) perspective.
- **Key knowledge** — domain facts you must internalize before building (distilled, not republished).
- **Section references** — the sources of the facts (P0X sections). Consult the sources only when you need more depth.
- **MVP / scope** — this module's implementation timing per [p11](../../notes/p11-mvp-scope-design.md).
- **Pitfalls** — items in P10 §11 (risks/open questions) directly tied to the module.

> The server consists of **7 modules** (M07·M09·M10·M11·M12·M13·M14) + the integration binary `sonicbrew`. The audio core (M01–M06), clock sync (M16), and plugin loader (M15) belong to the audio-toolkit workspace (the `-bsd` crate family), and the server consumes them as path dependencies. Audio asset (sample/IR) loading is handled by audio-toolkit file I/O, so **there is no server-side asset module** (§8.2).

---

## 1. session-store (M07) — distributed session/consensus/topology

- **Role:** The single source of truth for the audio graph topology. Node/link changes go through consensus, persistence, and subscription notification. Owns the "state consensus" axis (Raft) of the distributed architecture.

- **Key knowledge:**
  - **Distributed audio session model:** an audio server's "session" = *the current graph topology* (which nodes are connected by which links) + *the currently connected clients* + *routing*. This state must agree across distributed nodes ([P04 §3, §5.5](../../docs/p04-distributed-system.html)).
  - **Raft consensus:** replicate the topology canon (canonical form) across multiple nodes via leader election + log replication. <1s failover target (pre-vote + frequent heartbeats) ([P04 §5.1–5.2](../../docs/p04-distributed-system.html)).
  - **Φ-accrual failure detection + failover:** detection based on variable confidence rather than fixed timeouts — distinguishing "slow" from "dead" audio nodes. This judgment is the basis for leader promotion/failover ([P04 §5.2](../../docs/p04-distributed-system.html)).
  - **WAL persistence (handled by `redb` itself):** topology changes are first written to a write-ahead log and then applied → replayed back in after a restart. Durable storage is owned by **session-store's own embedded K/V (`redb`/`sled`)** — **not delegated to a separate storage module** ([P04 §3](../../docs/p04-distributed-system.html), [p10 §7.1 L1](../../notes/p10-architecture-design.md)).
  - **MVP decision:** start with `openraft` **single-node (self-leader) mode** — distributed consensus (leader election/log replication/Φ-accrual) **extends to multi-node distribution** after single-node validation. When scaling out, integrate with `carp(4)` HA VIPs ([p11 §0 decision #3, §3.2](../../notes/p11-mvp-scope-design.md)).

- **Section references:** [P04 §3](../../docs/p04-distributed-system.html) (storage/session layer), [P04 §5.1–5.2](../../docs/p04-distributed-system.html) (Raft/Φ-accrual/failover), [p10 §7.1](../../notes/p10-architecture-design.md) (SessionStore layer), [p10 §8 M07](../../notes/p10-architecture-design.md).
- **MVP / scope:** Included in MVP (P0). **Not included** in the P12 prototype (P12 = M01+M02+M04+M12-WS). That is, the P12 demo runs `demo-route` on a hardcoded graph without session-store ([p11 §8.3](../../notes/p11-mvp-scope-design.md)).
- **Pitfalls:** `openraft` single-node is validated, but **multi-node consensus is unvalidated** (MVP risk). Scaling out requires re-validation of Raft log replication + `carp(4)` HA VIP integration ([p10 §5.1, §11.4 #3](../../notes/p10-architecture-design.md)).

---

## 2. net-rtp-aes67 (M09) — RTP/AES67 zero-copy distributed network audio

- **Role:** Pro/broadcast audio path. Zero-copy transmit/receive of RTP/UDP audio packets + AES67 compatibility. Owns the "distributed network audio" axis of the distributed architecture.

- **Key knowledge:**
  - **AES67 standard:** 48/96 kHz, L16/L24 PCM, PTPv2 synchronization, SAP/SDP discovery. Multicast RTP/UDP — audio distributed between nodes as packets ([P04 §2.1, §2](../../docs/p04-distributed-system.html)).
  - **netmap zero-copy:** direct NIC packet I/O via kernel-bypass mmap rings — the key to AES67 ultra-low latency (<1 ms). The Linux counterpart is AF_XDP, but FreeBSD integration is more mature ([P05 §2](../../docs/p05-freebsd-network-leverage.html)).
  - **⚠ NIC support gate mandatory:** `netmap` only works on supported NICs (ix/ixgbe/igb/ixl). **Gate it behind a backend trait** — unsupported NICs fall back to standard sockets ([p10 §11.2](../../notes/p10-architecture-design.md)).
  - **PTP clock integration (`audio-clock-bsd` / M16):** precise clocks are mandatory for RTP timestamp alignment and playback-buffer fill. `audio-clock-bsd` provides PTP clock (t_ns) ↔ audio sample index (s) conversion (PLL/FLL loops) — with AES67 compatibility, M16 is promoted P2→P1 ([P04 §4](../../docs/p04-distributed-system.html), [p10 §4.3, §11.4 #1](../../notes/p10-architecture-design.md)).
  - **SRTP:** `libsrtp2` (BSD-3) — reuse of the WebRTC security path (DTLS-SRTP) ([P03 §6](../../docs/p03-foundational-libraries.html)).
  - **MVP decision:** out of MVP scope (P1). The MVP is single-host. netmap RTP/AES67 is adopted when distributed/AES67-compatibility requirements arise ([p11 §3.1](../../notes/p11-mvp-scope-design.md)).

- **Section references:** [P04 §2](../../docs/p04-distributed-system.html) (AES67 distributed audio), [P04 §4](../../docs/p04-distributed-system.html) (PTP/clock domain), [P05 §2](../../docs/p05-freebsd-network-leverage.html) (netmap zero-copy), [P03 §6](../../docs/p03-foundational-libraries.html) (libsrtp2), [p10 §8 M09, §5.1](../../notes/p10-architecture-design.md).
- **MVP / scope:** out of MVP scope (P1).
- **Pitfalls:** `netmap-rs` 0.3 is active, but the NIC gate is a precondition. As with the absence of io_uring, backend-trait separation is a design principle. The level of FreeBSD PTP hardware timestamp support determines whether AES67 compatibility is achievable ([p10 §5.2, §11.2, §11.4 #1](../../notes/p10-architecture-design.md)).

---

## 3. gw-pulse (M10) — PulseAudio protocol-compatible gateway

- **Role:** Linux PulseAudio applications connect unmodified by recognizing sonicbrew as a PulseAudio daemon.

- **Key knowledge:**
  - **PipeWire model inheritance:** sonicbrew's implementation of the model where PipeWire transparently accepts existing PulseAudio clients (including Firefox/Chromium) through the `pipewire-pulse` compatibility layer ([P02 §5](../../docs/p02-linux-audio-servers.html)).
  - **PulseAudio protocol structure:** daemon + native protocol (Unix socket/TCP) + module architecture. After deserialization, a Source node is inserted into the core graph via the **same standard `AudioFrame` interface** (`audio-core-bsd` AudioNode) ([P02 §3](../../docs/p02-linux-audio-servers.html), [p10 §6.1, §6.2](../../notes/p10-architecture-design.md)).
  - **Sample-rate mapping:** app → server runtime report. Resampling against the server's 48 kHz base (`audio-resample-bsd`) ([p10 §6.1](../../notes/p10-architecture-design.md), [P07 §1](../../docs/p07-audio-fundamentals.html)).
  - **License:** `libpulse` LGPL-2.1 **dynamic-linking obligation**. The gateway is isolated in a separate .so/jail so the Rust-only core's licensing simplicity is preserved ([p10 §8 M10, §6.3](../../notes/p10-architecture-design.md)).
  - **MVP decision:** out of MVP scope (P1). The MVP prioritizes browser compatibility (WebSocket). PulseAudio compatibility comes when extending to Linux desktop compatibility ([p11 §3.1](../../notes/p11-mvp-scope-design.md)).

- **Section references:** [P02 §3, §5](../../docs/p02-linux-audio-servers.html) (PulseAudio protocol/pipewire-pulse), [P03 §3](../../docs/p03-foundational-libraries.html) (libpulse), [p10 §8 M10](../../notes/p10-architecture-design.md).
- **MVP / scope:** out of MVP scope (P1).
- **Pitfalls:** Protocol deserialization complexity + conversion overhead (tens of µs–ms). Latency target 50–200 ms (typical PulseAudio, [p10 §11.3](../../notes/p10-architecture-design.md)).

---

## 4. gw-alsa (M11) — ALSA(L) PCM plugin emulation gateway

- **Role:** Linux ALSA applications connect to sonicbrew via an `libasound` PCM plugin (the `pipewire-alsa` model).

- **Key knowledge:**
  - **ALSA PCM plugin interface:** `libasound_module_pcm_*` — the client negotiates sample rate/channels via `snd_pcm_hw_params` → aligned to the server's internal 48 kHz (`audio-resample-bsd`) ([P02 §2](../../docs/p02-linux-audio-servers.html), [P03 §2](../../docs/p03-foundational-libraries.html)).
  - **FreeBSD limitation inherited:** on FreeBSD, ALSA **cannot access hardware directly** — it goes through an emulation layer over OSS (`audio/alsa-lib` port). M11 inherits this limitation as-is ([P01 §3](../../docs/p01-freebsd-audio-ecosystem.html), [p10 §8 M11](../../notes/p10-architecture-design.md)).
  - **License:** `libasound` LGPL-2.1 dynamic linking.
  - **Lowest priority:** in a FreeBSD-first project, ALSA(L) compatibility comes last ([p10 §9.2 order 12](../../notes/p10-architecture-design.md)).
  - **MVP decision:** out of MVP scope (P2, lowest).

- **Section references:** [P02 §2](../../docs/p02-linux-audio-servers.html) (ALSA plugin chain), [P03 §2](../../docs/p03-foundational-libraries.html) (ALSA/libasound), [P01 §3](../../docs/p01-freebsd-audio-ecosystem.html) (ALSA-on-OSS limitation), [p10 §8 M11](../../notes/p10-architecture-design.md).
- **MVP / scope:** out of MVP scope (P2).
- **Pitfalls:** on FreeBSD the path is ALSA-via-emulation (not direct hardware) → the limits of the emulation layer over OSS. Concurrent-playback and hotplug-migration limitations (same root cause as the missing cpal OSS backend, [p10 §11.1, §11.2](../../notes/p10-architecture-design.md)).

---

## 5. gw-browser (M12) — WebAudio/WebSocket/WebRTC browser gateway ★ (core of the shared goal)

- **Role:** Gateway with three browser-compatible sub-paths. **MVP = WebSocket sub-path only**. The entry point of sonicbrew's shared goal (unmodified browser connectivity).

- **Key knowledge:**
  - **Three sub-paths (P08 §1/§3/§5):**
    - (a) **WebRTC** — `RTCPeerConnection` + Opus RTP over DTLS-SRTP (low-latency bidirectional).
    - (b) **WebSocket** — binary PCM/Opus frames (simple bidirectional/recording). **← MVP implementation target.**
    - (c) **HTTP/MSE/HLS** — one-way streaming playback.
  - **MVP decision (key):** **WebSocket sub-path only**. WebRTC/HLS = stub/TODO. Reason — avoid the unverified `str0m`/`webrtc-rs` FreeBSD CI risk ([p11 §0 decision #2, §3.2](../../notes/p11-mvp-scope-design.md)).
  - **Sample-rate mapping (48 kHz):** WebRTC mandates 48000 Hz (RFC 7587). WebSocket reports at runtime. Server-side `audio-resample-bsd` resampling (browser 44.1/48k ↔ server 48k) ([p10 §6.1](../../notes/p10-architecture-design.md), [P08 §1](../../docs/p08-browser-audio.html), [P07 §1](../../docs/p07-audio-fundamentals.html)).
  - **WebAudio nodes ↔ server graph 1:1:** the P08 §2 Web Audio node-graph model can be mapped onto the core graph — the browser's `AudioContext` node structure is projected onto server-side nodes ([P08 §2](../../docs/p08-browser-audio.html), [p10 §6.2](../../notes/p10-architecture-design.md)).
  - **Dataflow (P12 demo):** WebSocket receive → binary PCM (48k/2ch/f32) → `AudioFrame` conversion → Source node insertion. Reverse direction Sink → WebSocket send (bidirectional) ([p11 §2, §8.3](../../notes/p11-mvp-scope-design.md)).
  - **Codec sub-path:** on Opus frame reception, `audio-opus-bsd` decodes → `AudioFrame` conversion ([p11 §2](../../notes/p11-mvp-scope-design.md)).
  - **License:** `tungstenite`/`tokio-tungstenite` (WebSocket, pure Rust). `str0m` (WebRTC, MIT/Apache, **MVP stub**).

- **Section references:** [P08 §1, §2, §3, §5](../../docs/p08-browser-audio.html) (WebRTC/WebAudio/WebSocket/HLS), [P09 §6](../../docs/p09-rust-audio-ecosystem.html) (str0m/webrtc-rs), [p10 §6.1, §8 M12](../../notes/p10-architecture-design.md).
- **MVP / scope:** Included in MVP (P0, WebSocket only) + included in P12 (the browser entry point of the P12 cut). Core module of the shared goal.
- **Pitfalls:** the WebRTC sub-path stub must remain **compilable but functionally unimplemented** (decision #2). Introducing WebRTC requires re-examining SFU vs MCU ([p10 §11.4 #6](../../notes/p10-architecture-design.md)).

---

## 6. control-api (M13) — control plane (gRPC/REST), node-graph manipulation API

- **Role:** Control API for external control clients (admin UI, automation). Node-graph manipulation, session management, monitoring queries.

- **Key knowledge:**
  - **Graph manipulation API:** `list_nodes` / `create_node(spec)` / `link(from, to)` / `load_module(path)` — exposes M07's (SessionStore) topology changes as an API ([p10 §8 M13](../../notes/p10-architecture-design.md), [P04 §5.5](../../docs/p04-distributed-system.html)).
  - **REST first:** the MVP is REST-first (`axum`/`hyper`), with gRPC (`tonic`) optional ([p11 §2 M13](../../notes/p11-mvp-scope-design.md)).
  - **HA:** virtual endpoint via `carp(4)` VIP — clients see a single endpoint, with automatic promotion on failure ([p10 §5.1, §8 M13](../../notes/p10-architecture-design.md), [P04 §6.1](../../docs/p04-distributed-system.html)).
  - **Authorization:** mTLS + Capsicum restriction (static crate, separate jail) ([p10 §8 M13](../../notes/p10-architecture-design.md)).
  - **Module loading:** the MVP is static-link only — dynamic `.so` load/unload depends on M15 (`audio-plugin-bsd`, P1) ([p11 §2 M13, §3.1](../../notes/p11-mvp-scope-design.md)).
  - **MVP decision:** included in MVP (P0). But dynamic module loading is static-only.

- **Section references:** [P04 §5.5](../../docs/p04-distributed-system.html) (graph manipulation + control plane), [P04 §6.1](../../docs/p04-distributed-system.html) (carp HA), [p10 §8 M13](../../notes/p10-architecture-design.md).
- **MVP / scope:** Included in MVP (P0). Not included in the P12 prototype (the P12 demo uses the hardcoded `demo-route`, [p11 §8.3](../../notes/p11-mvp-scope-design.md)).
- **Pitfalls:** new module (no direct P0X phase basis) — synthesized from P04 §5.5 graph manipulation + general control-plane practice. A kqueue(2)-based event loop is recommended ([p10 §5.1, §8 M13](../../notes/p10-architecture-design.md)).

---

## 7. monitor (M14) — RT audio metrics/observability

- **Role:** RT audio metrics (latency, xruns, CPU, buffer levels), distributed tracing, logging. Low-overhead metric collection.

- **Key knowledge:**
  - **kqueue event loop:** consolidates sockets, timers, signals, and AIO into a single queue — fewer syscalls than the epoll+timerfd+signalfd split model. `EVFILT_TIMER` schedules buffer flushes/heartbeats precisely (no fd needed) ([P05 §1](../../docs/p05-freebsd-network-leverage.html)).
  - **Audio metric definitions:** xrun (buffer underrun/overrun counts), latency (p50/p99 µs), CPU, buffer level ([P07 §6](../../docs/p07-audio-fundamentals.html)).
  - **Measurement tooling:** `metrics` crate + `tracing` (Rust standard). Prometheus/Grafana integration (`export_prometheus`) ([p10 §8 M14](../../notes/p10-architecture-design.md)).
  - **MVP decision:** out of MVP scope (P1). The MVP keeps **only minimal metrics** (latency/xrun counts) — measured via custom timestamp logging with `std::time::Instant`. The full M14 feature set (event loop/Prometheus) comes later ([p11 §3.1, §7c measurement tooling](../../notes/p11-mvp-scope-design.md)).
  - **Observability ↔ control linkage:** M13's control-API monitoring query endpoints consume M14 metrics.

- **Section references:** [P05 §1](../../docs/p05-freebsd-network-leverage.html) (kqueue event loop), [P07 §6](../../docs/p07-audio-fundamentals.html) (xrun/latency measurement), [p10 §8 M14](../../notes/p10-architecture-design.md).
- **MVP / scope:** out of MVP scope (P1). During the MVP, latency is measured with minimal `std::time::Instant` logging.
- **Pitfalls:** no io_uring → async I/O must be redesigned around `aio(4)` + `EVFILT_AIO` (the monitor's async metric-collection path) ([p10 §5.1, §11.2](../../notes/p10-architecture-design.md)).

---

## 8. Common knowledge — audio-toolkit integration + asset loading + FreeBSD environment + the distributed story

### 8.1 Gateway/transport nodes = insertion as audio-toolkit AudioNodes ★

> This is the **single meeting point** between sonicbrew (server) and audio-toolkit, and the most important integration pattern.

- **Principle:** every gateway (M10/M11/M12) and net-rtp (M09), after protocol-specific deserialization, is inserted into the core graph via the **same standard `AudioFrame` interface**. This is sonicbrew's implementation of the PipeWire "every client becomes a graph node" model (P02 §4) ([p10 §6.2](../../notes/p10-architecture-design.md)).
- **Implementation pattern:** gw-*/net-rtp implement audio-toolkit's `AudioNode` trait (`audio-core-bsd`, M02) and register as Source/Sink nodes via `Graph::add_node`. For the implemented insertion pattern, see [ARCHITECTURE.md](./ARCHITECTURE.md) §4.
- **`AudioFrame` standard:** `{ channels: u16, sample_rate: u32, samples: Vec<f32> }` — consistent across all node inputs/outputs ([p10 §8 M02](../../notes/p10-architecture-design.md)).

### 8.2 Audio asset (sample/IR) loading = audio-toolkit file I/O ★

> **There is no sonicbrew-side asset module/repository.** All audio asset loading is handled by audio-toolkit's **local file I/O**.

- **Sample/recording decode:** FLAC/WAV/PCM files are read from local files and decoded into `AudioFrame`s by `audio-codec-bsd` (M06, `symphonia`-based multi-format decoder) ([P06 §1, §4](../../docs/p06-audio-codecs.html), [P03 §4](../../docs/p03-foundational-libraries.html)).
- **IR (impulse response) loading:** IRs for convolution reverb are loaded from local files by `audio-dsp-bsd` (M03 `ConvolverReverb::load_ir`) ([P07 §5](../../docs/p07-audio-fundamentals.html), [p10 §8 M03](../../notes/p10-architecture-design.md)).
- **Implication:** assets are "just local files" — the sonicbrew server has no repository abstraction, replication, or versioning layer; whichever node needs them reads the files directly with audio-toolkit decoders. Placing assets on distributed nodes is the responsibility of the operations layer (jail mounts/file synchronization), not of sonicbrew modules.

### 8.3 RT-safety separation principle (sonicbrew modules comply too)

- The only things callable directly from an RT audio thread are `rubato` (`audio-resample-bsd`, explicitly RT-safe) and lightweight cpal callbacks ([P09 §7](../../docs/p09-rust-audio-ecosystem.html)).
- Decode (symphonia), encoding (opus), and networking (str0m/WebSocket) run on **separate worker threads** and hand off to the RT thread via `rtrb`/`ringbuf` lock-free ring buffers ([p10 §7.2](../../notes/p10-architecture-design.md)).
- **sonicbrew implication:** gw-browser's WebSocket receive thread is never an RT thread — worker → `rtrb` → graph RT processing.

### 8.4 FreeBSD environment (jails/carp)

- **Module isolation:** each sonicbrew module (M07/M09/M13) is separated into its own `jail(8)` — component privilege separation ([P04 §6.4](../../docs/p04-distributed-system.html)).
- **Capability sandbox:** per-fd rights via `cap_rights_limit(2)` (Capsicum). Gateway modules (M10/M11/M12) use the fd = object-capability model after `cap_enter(2)` — damage is localized when one module is compromised ([P05 §4](../../docs/p05-freebsd-network-leverage.html)).
- **HA VIP:** `carp(4)` (VRRP-equivalent) for M07 session HA + M13 control-API VIP ([P04 §6.1](../../docs/p04-distributed-system.html)).
- **NIC redundancy:** multipathing the M09 RTP/M16 PTP paths via `lagg(4)` (LACP/active-backup) — preventing audio dropouts ([P04 §6.2](../../docs/p04-distributed-system.html)).
- **MVP limitation:** the MVP is statically linked — jails/Capsicum/carp/lagg are **activated when scaling out**. During the MVP, the only FreeBSD kernel dependency is kqueue (M13 minimum) ([p11 §5.2 kernel dependency table](../../notes/p11-mvp-scope-design.md)).

### 8.5 The distributed-concept borrowing story (sonicbrew's identity)

> sonicbrew aims beyond single-host integration (PipeWire-class) at a **distributed-first audio graph server** ([p10 §1](../../notes/p10-architecture-design.md)). Distribution consists of the four axes below:

| Axis | Owner | Essence |
|----|------|------|
| **State consensus** | M07 session-store | Replicates the topology canon across multiple nodes via Raft leader election + log replication. Session-state persistence (WAL) is handled by session-store itself (`redb`) |
| **Clock sync** | `audio-clock-bsd` (M16) | PTPv2 (IEEE 1588-2008) hardware timestamping + NTP fallback. PTP clock (t_ns) ↔ sample index (s) conversion (PLL/FLL) |
| **Distributed network audio** | M09 net-rtp-aes67 | AES67 multicast RTP/UDP + netmap zero-copy. Audio packets distributed between nodes |
| **Distributed graph** | M02 core graph (`audio-core-bsd`) | The topology is treated as a single graph spanning distributed nodes (P04 §5.5) |

- **Evolution path:** the MVP starts with `openraft` single-node (self-leader) — all four axes are **validated on a single host first**, then extended to multi-node distribution. HA failover is reinforced with `carp(4)` VIPs ([P04 §5–6](../../docs/p04-distributed-system.html), [p11 §0 decision #3](../../notes/p11-mvp-scope-design.md)).

### 8.6 License compatibility (sonicbrew is BSD-2)

- **Pure Rust (BSD-2 compatible):** `openraft`, `redb`/`sled`, `axum`/`hyper`, `tonic`, `tungstenite`, `metrics`, `tracing`, `rtrb`, `realfft`, `rubato`, `symphonia` (MPL-2.0, linking OK; modifications require MPL disclosure of the affected files) ([p10 appendix B](../../notes/p10-architecture-design.md)).
- **LGPL dynamic linking:** `libpulse` (M10), `libasound` (M11) — separate .so/jail to protect the core's licensing simplicity.
- **BSD-3 system libraries:** libopus (`audio/opus` port, `audio-opus-bsd`).

### 8.7 FreeBSD network-optimization kernel — exact operating model + gain/loss conditions

> §8.4 (environment) and §2/§6/§7 (modules) cite these features from a module perspective. This section distills the **exact operating model of the kernel features (theoretical background)** + the **gain/loss conditions from sonicbrew's perspective**. **All features [designed/planned — not implemented in the sonicbrew server]** (§8.4 MVP limitation: activated when scaling out).

- **kqueue(2)/kevent(2) — unified event notification [designed/planned]:** consolidates sockets (`EVFILT_READ`/`WRITE`), timers (`EVFILT_TIMER`, no fd needed), signals (`EVFILT_SIGNAL`, no handler needed), AIO (`EVFILT_AIO`), filesystem (`EVFILT_VNODE`), and processes (`EVFILT_PROC`) into a single kernel event queue **as filters**. A single `kevent()` call performs registration (`changelist`) and retrieval (`eventlist`) at the same time (epoll splits them into `epoll_ctl`/`epoll_wait`). Level-triggered by default, edge-triggered with `EV_CLEAR`. **Gain:** the event loop for tens of thousands of WebSocket clients (M12) consolidates into a single queue → fewer syscalls; `udata` eliminates dispatch lookups. **Loss conditions:** struct-processing overhead versus epoll for workloads with very short callback chains (single fd, tiny event counts); with edge triggers (`EV_CLEAR`), a deadlock unless `read`/`recv` is fully drained to `EAGAIN`. When designing on top of `tokio`/`mio`, no direct coding is needed (automatic kqueue backend) ([P05 §1](../../docs/p05-freebsd-network-leverage.html), §7 monitor).
- **netmap(4)/VALE — zero-copy packet dataplane [designed/planned]:** exposes the NIC's TX/RX queues directly to userspace as mmap rings (`netmap_ring` → `netmap_slot` → fixed buffers). Bypasses the kernel socket layer and mbuf allocation → achieves 10GbE line rate (14.88 Mpps) on less than one core. VALE (in-kernel learning bridge switch, ~20 Mpps per core) and netmap pipes (inter-process, 100 Mpps+) share the same API. **Gain:** µs-scale RTP transmit/receive — the key to the net-rtp-aes67 (M09) dataplane (§2). **Loss conditions:** **native netmap support in the NIC driver is mandatory** (ix/ixgbe/igb/ixl/em/re/vtnet/cxgbe; `mlx5en`·`vmx` unverified) — unsupported NICs run in emulation mode (only 3–5× faster than raw sockets, not line rate); presupposes a backend-trait gate + standard-socket fallback ([P05 §2](../../docs/p05-freebsd-network-leverage.html), §2 Pitfalls).
- **sendfile(2)/copy_file_range(2) — zero-copy transfer [designed/planned]:** in-kernel direct transfer for file→socket (`sendfile`) / file→file (`copy_file_range`, BRT block cloning on ZFS) — zero userspace buffer copies. **Gain:** removes CPU copy load when serving large sample banks/IRs; **works on both UFS and ZFS**. **Loss conditions:** **semantic differences** such as `sendfile`'s return type and `copy_file_range` not returning EXDEV ("same API, different behavior" from Linux) — these cause silent errors in cross-platform code, so the semantic differences must be encoded in the backend trait ([P05 §3](../../docs/p05-freebsd-network-leverage.html)).
- **CAP_RIGHTS(2)/Capsicum — per-fd capability sandbox [designed/planned]:** `cap_enter(2)` irreversibly cuts off global namespace access → from then on, each fd is an object capability (refined into read/write/ioctl etc. via `cap_rights_limit`). **Gain:** isolation of untrusted code — when `audio-plugin-bsd` (M15) loads dynamic plugins, one compromised plugin → damage localized at fd granularity. **Loss conditions:** after `cap_enter`, file paths cannot be opened (only pre-opened fds are usable) → forces deliberate load-order design; not portable to Linux (SECCOMP/Landlock differ semantically) ([P05 §4](../../docs/p05-freebsd-network-leverage.html), §8.4 capability sandbox).
- **netgraph(4)/ipfw(8) dummynet — QoS/shaping [designed/planned]:** graph (DAG)-based network stack nodes + WF2Q+/FQ-CoDel/PIE traffic shaping. **Gain:** RTP jitter control and bandwidth allocation (M09). **Loss conditions:** limited Rust bindings (`ipfw-rs` is CLI-wrapper level, netgraph FFI is manual) + high configuration complexity ([P05 §5](../../docs/p05-freebsd-network-leverage.html)).

### 8.8 FreeBSD filesystem characteristics — UFS vs ZFS vs tmpfs, data-delivery implications

> The **background theory** for §8.2 (asset loading = audio-toolkit file I/O). There is no sonicbrew-side storage module — this section covers only how the FS affects **sample loading speed and WAL durability**.

- **UFS (UFS2/ffs):** block-based, soft updates + SU+J journaling. **Low per-op overhead** (no checksums, no CoW) → favorable for metadata, small files, and low-latency scratch. Supports `sendfile`/`mmap`.
- **ZFS (OpenZFS):** CoW + end-to-end checksums. **ARC** (memory cache — hot samples), **L2ARC** (SSD second-level cache — warm samples), **ZIL/SLOG** (synchronous-write log — guarantees WAL durability), compression (lz4/zstd), snapshots/send-recv (incremental block replication). **Favorable for read-heavy bulk storage (sample banks) + durability (WAL).** Downside: higher per-op write costs due to CoW/checksums ([P04 §3](../../docs/p04-distributed-system.html) ZFS snapshot/send).
- **tmpfs:** RAM filesystem, µs access, volatile. For the hottest working sets.
- **Zero-copy paths:** `sendfile` (file→socket), `mmap` (file→memory→DSP) — **work on both UFS and ZFS** ([P05 §3](../../docs/p05-freebsd-network-leverage.html)).
- **Core principle:** **RT path = zero file I/O** (pre-loaded via `rtrb` lock-free rings, §8.3). FS impact is confined to exactly two points — **sample loading speed** (ARC/L2ARC caching + `mmap`/`sendfile`) and **WAL durability** (ZFS ZIL/SLOG, §1 session-store) — RT audio frame processing itself is FS-independent.

### 8.9 Distributed processing patterns — for performance scaling (not recovery)

> Where §8.5 (the four distributed-concept axes) covers **state consensus, recovery, and durability**, this section covers **performance-scaling (throughput/latency distribution)** patterns. All are [designed/planned — not implemented in the sonicbrew server], tied to the P04 distributed graph.

- **Distributed graph execution:** distribute independent subtrees of the audio DSP graph across multiple nodes → load-balancing DSP between nodes. The topology canon is kept consistent by M07 (Raft), and the distributed graph (M02 core) is treated as a single graph ([P04 §5.5](../../docs/p04-distributed-system.html), §1).
- **Intra-node parallel DSP:** process independent branches on a single node in parallel across cores — when a graph's branch points do not depend on each other, parallel composition within a thread pool fits inside the RT budget.
- **Edge processing:** initial processing (DC offset/unpacking) immediately upon RTP packet reception in the netmap dataplane (right at the NIC) — µs-scale savings (§2, §8.7 netmap).
- **Distributed sample-bank caching:** each node caches hot/warm samples in its local ARC/L2ARC (§8.8 ZFS) — loaded via local file I/O (`mmap`/`sendfile`) without a network hop (§8.2).
- **Federated mixing:** each node performs partial mixes of its subgraph and only the results are combined (tree/hierarchical mixing) — avoids the bandwidth bottleneck of gathering all audio onto one node.

---

## Related documents

- [ARCHITECTURE.md](./ARCHITECTURE.md) — architecture reflecting implementation status (2026-08-08 — the construction-plan/roadmap documents were consolidated and retired upon completion).
- [../ARCHITECTURE.md](../../ARCHITECTURE.md) — developer summary of the overall architecture.
- [../notes/p10-architecture-design.md](../../notes/p10-architecture-design.md) — single source of truth for the unified architecture (815 lines).
- [../notes/p11-mvp-scope-design.md](../../notes/p11-mvp-scope-design.md) — MVP scope finalized (567 lines).
- [../audio-toolkit/](../audio-toolkit/) — audio core workspace (the `-bsd` crate family: audio-core-bsd, audio-codec-bsd, audio-dsp-bsd, audio-clock-bsd, etc.).

---

**End of document.** The section references in each module section state explicitly that this document does not replace the sources (P0X). The construction roadmap concluded with all modules complete (progress history is in PROGRESS.md).
