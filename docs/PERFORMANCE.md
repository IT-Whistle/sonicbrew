# sonicbrew server — Performance Metrics Reference (PERFORMANCE)

> **Document nature:** This is the measurement and verification standard defining **what sonicbrew measures and what the targets are**. Explanations of performance *mechanisms* (distributed processing axes, FreeBSD kernel utilization, FS strategy, RT-safety separation principle) are **out of scope** here — see [ARCHITECTURE.md](./ARCHITECTURE.md) §7 (distributed 4 axes), §8 (RT-safety boundary), and [KNOWLEDGE.md](./KNOWLEDGE.md) §8.3 (RT separation).
>
> **Separation principle:** This document = **metrics · targets · measurement methods**. Mechanisms (how to achieve them) are consolidated into ARCHITECTURE/KNOWLEDGE. Mixing the two concerns blurs the development context.
>
> **Baseline:** FreeBSD `14.2-RELEASE` (at least p3, production target) · Rust 2021 ed. (MSRV 1.85+) · Written 2026-07-30
> **Target sources:** [p10 §11.3 latency budget](../../notes/p10-architecture-design.md) · [p11 §7a per-module acceptance](../../notes/p11-mvp-scope-design.md) · [p11 §7c quantitative latency targets](../../notes/p11-mvp-scope-design.md)
> **Observability tooling sources:** [test-coverage-heatmap-design.md](../../notes/test-coverage-heatmap-design.md) §Methods 4-9 · [TESTING-STANDARDS.md](../../TESTING-STANDARDS.md) §1-2
> **Scope:** Pre-code (documentation only). Of the targets below, values confirmed in the design specs are marked with citations; unconfirmed numbers are explicitly marked **TBD (benchmark back-calculation)** — arbitrary fabrication is prohibited.

---

## Purpose (one sentence)

> Provides a single point of reference — metric definitions, targets, measurement protocols, and status tracking — so that every performance-related decision in sonicbrew (buffer tuning, crate selection, whether to scale out) is **grounded in measurable criteria**.

---

## 1. Metric Hierarchy — Two-Tier Tables (per-module + system-level)

> sonicbrew performance is measured at **two levels**: (a) **per-module** — whether each individual crate passes its acceptance criteria ([p11 §7a](../../notes/p11-mvp-scope-design.md)); (b) **system-level** — whether the integrated `sonicbrew` binary meets end-to-end targets ([p10 §11.3](../../notes/p10-architecture-design.md) · [p11 §7c](../../notes/p11-mvp-scope-design.md)).
>
> Each metric in the **seven categories** below specifies: **definition · target · measurement method · measurement tool · pass criteria**. Target status is tracked in §5 (status tracking).

### 1.1 RT / Latency (Real-Time / Latency)

> In RT audio, latency and xruns are **critical defects** ([TESTING-STANDARDS.md](../../TESTING-STANDARDS.md) §1 core principles). This category is sonicbrew's strictest gate.

| Metric | Definition | Target | Status | Measurement method | Measurement tool | Pass criteria |
|------|------|--------|------|-----------|-----------|----------|
| **xrun rate** | Buffer underrun/overrun count per unit time | **0 (absolute)** | Confirmed (the MVP spec §7a) | Monitor the xrun counter during sustained playback of 1 second or longer | Custom harness xrun counter (`--features rt-safety --test rt_xrun`) | xrun = 0 during 1s+ playback; xrun = 0 over 1000 cycles under load |
| **RT pass latency** | Wall-clock time of a single `process_cycle(256)` call | < budget (local < 5ms) | TBD (benchmark back-calculation) | Wall-clock measurement (p50/p99) of one `process_cycle` invocation | criterion (`process_cycle_256`) | p99 < single-cycle budget (within 5.3ms for a 256-frame buffer @ 48k) |
| **End-to-end latency** | Total time from source input → sink output | Per path (§4 latency budget table) | Confirmed (the MVP spec §7c) | Send timestamp ↔ audible-time deviation | `std::time::Instant` (current) / kqueue timestamps (future) | Local < 5ms / WS one-way < 100ms (§4) |
| **Jitter** | Standard deviation of consecutive latency measurements | < TBD µs | TBD (benchmark back-calculation) | Compute σ from N latency measurements | criterion statistics + dtrace USDT probes | p99 jitter < TBD× the single-frame period (48k = 20.8µs) |
| **RT-thread dynamic allocation** | Count of `alloc` calls inside RT-thread entry points (`process_cycle`/`process_into_buffer`) | **0 (absolute)** | Confirmed (the MVP spec §7a  #3) | Count RT-path allocations with `CountingAllocator` | `CountingAllocator` (`--features rt-safety --test rt_alloc_free`) | alloc = 0 across 5 RT-designated nodes × 100 cycles |

### 1.2 Throughput

> Measurement protocols for throughput are finalized, but absolute numbers will be confirmed after benchmark back-calculation.

| Metric | Definition | Target | Status | Measurement method | Measurement tool | Pass criteria |
|------|------|--------|------|-----------|-----------|----------|
| **Concurrent streams** | Number of simultaneous audio streams a single node can process without xruns | TBD (benchmark back-calculation) | TBD | Incrementally increase load → detect the xrun threshold | Custom load generator + xrun counter | xrun = 0 at the target stream count |
| **Total throughput** | Total bytes/second based on ch × rate × depth | N × 384 kB/s (48k/2ch/f32) | Confirmed (formula) | Active stream count × per-stream data rate | Arithmetic + netstat throughput cross-check | 1 stream = 48000×2×4 B/s = 384 000 B/s ≈ 3.07 Mbps; N streams = N × 384 kB/s |
| **RTP Gbps** | netmap zero-copy throughput of `net-rtp-aes67` | TBD Gbps | TBD | Measure packets/second processed on netmap rings | netmap statistics + `gstat` | 6.144 Mbps per AES67 48k/L24 multicast stream; verify linear scaling to N streams |

> **Per-stream data-rate formula:** `sample_rate × channels × bytes_per_sample`. Baseline 48 000 Hz / 2 ch / f32 (4 B) = **384 kB/s ≈ 3.072 Mbps**. For L24 (3 B) AES67 = 288 kB/s. The target concurrent stream count (N) is a benchmark back-calculation.

### 1.3 Resource

| Metric | Definition | Target | Status | Measurement method | Measurement tool | Pass criteria |
|------|------|--------|------|-----------|-----------|----------|
| **CPU% / stream** | CPU usage per processed stream | TBD %/stream | TBD | Measure CPU usage across stream counts → linear-regression slope | `top`/`systat` + dtrace profile | CPU per unit < TBD; linearity R² > TBD |
| **RT-thread alloc count** | Dynamic allocation on the RT path (restated from §1.1; independent resource metric) | **0** | Confirmed (the MVP spec §7a) | `CountingAllocator` | `CountingAllocator` harness | alloc = 0 (absolute) |
| **peak RSS** | Process peak resident memory | TBD MB | TBD | Measure RSS at peak load | `/proc` equivalent — FreeBSD `procstat -r` / `ps -o rss` | peak RSS < TBD MB @ target stream count |
| **ARC hit rate** | ZFS ARC (Adaptive Replacement Cache) hit rate for asset loads | TBD % | TBD | ARC statistics under repeated asset (sample/IR) loads | `arcstat` (FreeBSD ZFS) | Hit rate > TBD % (avoids disk I/O on asset reloads) |

> **ARC note:** Audio asset (sample/IR) loading is local file I/O in audio-toolkit ([KNOWLEDGE.md](./KNOWLEDGE.md) §8.2). The ARC hit rate is not directly controlled by the sonicbrew server, but it indirectly affects asset-load latency — tracked as a system-level resource metric.

### 1.4 Audio Quality

> Quality targets are confirmed in the [p11 §7a](../../notes/p11-mvp-scope-design.md) acceptance criteria. SNR/PESQ/bit-exact are quantitative measurements against **golden samples** ([test-coverage-heatmap §Method 5](../../notes/test-coverage-heatmap-design.md)).

| Metric | Definition | Target | Status | Measurement method | Measurement tool | Pass criteria |
|------|------|--------|------|-----------|-----------|----------|
| **SNR** | Signal-to-noise ratio before/after resampler conversion | **≥ 90 dB** | Confirmed (the MVP spec §7a  #1-2) | Measure SNR after converting a 440Hz tone between 48k↔44.1k/96k | `--features quality --test snr_threshold -p audio-resample-bsd` + `fixtures/audio/` | 48k↔44.1k SNR ≥ 90dB **and** 48k↔96k SNR ≥ 90dB |
| **PESQ** | Voice-quality score of an Opus encode→decode round trip | **≥ 3.5** | Confirmed (the MVP spec §7a  #1) | Compute PESQ after an Opus 48k/2ch round-trip | `--features quality --test opus_pesq -p audio-opus-bsd` | PESQ ≥ 3.5 **or** SNR ≥ 30dB (alternative criterion) |
| **Codec transparency** | Bit accuracy of lossless paths (PCM/WAV) | **bit-exact (byte-exact)** | Confirmed (the MVP spec §7a  #2) | Byte-compare original ↔ decoded output | `--features quality --test flac_byte_exact -p audio-codec-bsd` | Sample values byte-identical to the original (lossless) |
| **Codec bitrate accuracy** | Deviation of the actual bitrate after applying Opus `set_bitrate` | Target ±10% | Confirmed (the MVP spec §7a  #3) | Measure encoding bitrate after `set_bitrate(64000)` | Custom harness | Actual bitrate ∈ [57 600, 70 400] bps (64000 ±10%) |
| **DSP accuracy** | Amplitude ratio after `GainProcessor` gain application | amplitude × gain ±1% | Confirmed (the MVP spec §7a  #2) | Measure input/output amplitude ratio after applying gain 0.5 | Unit test | |output| / |input| ∈ [0.495, 0.505] |

### 1.5 Recovery / Availability

> Failure-recovery targets for the distributed session (`session-store` ). For distributed-concept background, see [ARCHITECTURE.md](./ARCHITECTURE.md) §7.1 (session consensus axis).

| Metric | Definition | Target | Status | Measurement method | Measurement tool | Pass criteria |
|------|------|--------|------|-----------|-----------|----------|
| **RTO (leader election · failover)** | Time from leader failure to completion of new leader election | < 1 s (target) | TBD — benchmark back-calculation (P04 §5.1 pre-vote rationale) | Kill the leader → measure round-trip time to new leader election | openraft metrics + custom timer | Failover < 1s (based on pre-vote + frequent heartbeats, [P04 §5.1-5.2](../../docs/p04-distributed-system.html)) |
| **RPO (WAL durability)** | Acceptable data loss on failure (transactions not yet reflected in the WAL) | 0 (target) | TBD — benchmark back-calculation | Topology change → WAL fsync → forced kill → count losses after recovery | `session-store` integration test (`--test session_persistence`) | Topology loss after restart = 0 (WAL replay fully restores, [the MVP spec §7a  #2](../../notes/p11-mvp-scope-design.md)) |
| **Session restore time** | Time to restore the topology via WAL replay after a process restart | TBD ms | TBD | Wall-clock from stop → restart → `get_topology` responding normally | Integration-test timer | Restore < TBD ms (by topology size) |
| **Φ-accrual detection latency** | Time from node failure to the Φ-accrual verdict | TBD s | TBD | Isolate a node → time until the verdict signal fires | openraft metrics | Variable-confidence based — accuracy of distinguishing "slow" vs "dead" > TBD% |

### 1.6 Distributed / Scaling

> The MVP is single-node (self-leader), so distributed metrics are **measurement targets for the multi-node phase**. Only the measurement protocols are finalized ([ARCHITECTURE.md](./ARCHITECTURE.md) §7.3 distributed sync visualization).

| Metric | Definition | Target | Status | Measurement method | Measurement tool | Pass criteria |
|------|------|--------|------|-----------|-----------|----------|
| **Scaling efficiency** | Total-throughput growth versus node count (compared to ideal linear) | TBD % | TBD (multi-node phase) | Measure total throughput while scaling nodes 1→2→3→N | Distributed benchmark harness | Throughput / (N × single-node throughput) > TBD % |
| **Inter-node RTP latency** | One-way RTP transfer latency between distributed nodes | < 1 ms | Confirmed (design spec) | Timestamp deviation from node-A send → node-B receive (PTP-synchronized) | netmap timestamps + PTP (`audio-clock-bsd` ) | RTP one-way < 1ms (AES67-grade) |
| **Inter-node RTP jitter** | Variation of inter-node RTP latency | < TBD µs | TBD (multi-node phase) | σ of consecutive RTP packet latencies | RTP header timestamp analysis | p99 jitter < TBD µs |
| **Load-balancing efficiency** | Balance of stream distribution across distributed nodes | TBD | TBD (multi-node phase) | Measure the per-node stream-count distribution | Distributed metrics collection | Coefficient of variation (CV) < TBD |

### 1.7 Network

> The `net-rtp-aes67` netmap zero-copy path plus gateway (`gw-*`) network processing. Mechanisms (netmap/FEC/jitter buffer) are covered in [KNOWLEDGE.md](./KNOWLEDGE.md) §2 (net-rtp).

| Metric | Definition | Target | Status | Measurement method | Measurement tool | Pass criteria |
|------|------|--------|------|-----------|-----------|----------|
| **Packet-loss resilience (FEC)** | No audio-quality degradation within the tolerable packet-loss rate after forward error correction (FEC) is applied | TBD % | TBD (multi-node phase) | Inject deliberate packet drops → FEC recovery → measure SNR | Packet-drop injector + SNR measurement | SNR ≥ 90dB maintained at loss rates ≤ TBD% |
| **Tolerable packet-loss rate** | Maximum packet-loss rate that still holds the quality threshold (SNR ≥ 90dB) | TBD % | TBD (multi-node phase) | Ramp the loss rate up from 0 → detect the SNR threshold crossing | Packet-drop injector + SNR measurement | Threshold loss rate ≥ TBD% |
| **Jitter-buffer depth** | Frames in the receive-side RTP playout buffer | TBD frames | TBD (multi-node phase) | Cross-measure buffer setting × jitter resilience | netmap buffer statistics | depth × 1/rate < latency budget (jitter absorption vs added-latency tradeoff) |
| **netmap throughput** | Packets processed per unit time on netmap rings | TBD Mpps | TBD | netmap ring packet counters | `netstat -s` + netmap statistics | Throughput > packet rate required by the target stream count |
| **Gateway throughput** | WebSocket frame throughput of `gw-browser` | TBD fps | TBD | Measure WS frames/second | criterion (`ws_round_trip`) | Throughput > target client count × frame rate |

---

## 2. Measurement Protocol — FreeBSD-Native Observability Tool Mapping

> Measurements follow the [TESTING-STANDARDS.md](../../TESTING-STANDARDS.md) 5-layer pyramid (unit → property → integration → performance → FreeBSD target) and the commands defined in [test-coverage-heatmap §Methods 4-9](../../notes/test-coverage-heatmap-design.md). The table below maps each metric category to its observability tools.

| Measurement category | Tool | Target metrics | Representative command / usage |
|-----------|------|-----------|---------------------|
| **RT latency / throughput (microbench)** | `cargo bench` (criterion) | RT pass latency · throughput · WS round-trip | `cargo bench -p audio-resample-bsd -- resample_latency` · `cargo bench -p audio-graph-bsd -- process_cycle_256` · `cargo bench -p gw-browser -- ws_round_trip` |
| **Latency-budget regression** | criterion + latency-budget feature | End-to-end latency regression detection | `cargo bench --features latency-budget -p audio-io-bsd -- local_playback_latency` (alert on p99 +20% degradation) |
| **RT safety (alloc=0)** | `CountingAllocator` custom harness | RT-thread dynamic allocation = 0 | `cargo test --features rt-safety --test rt_alloc_free -p audio-resample-bsd` |
| **xrun regression** | Custom xrun counter (fake device) | xrun rate = 0 | `cargo test --features rt-safety --test rt_xrun -p audio-io-bsd` |
| **RT scheduler priority** | Real-time scheduler verification in a FreeBSD VM | Whether RT threads are granted rtprio | (inside a bhyve VM) `cargo test --target x86_64-unknown-freebsd --features rt-scheduler --test rt_priority` |
| **Quality (SNR/PESQ/bit-exact)** | Custom harness + `fixtures/audio/` golden samples | SNR · PESQ · transparency | `cargo test --features quality --test snr_threshold -p audio-resample-bsd` · `--test opus_pesq -p audio-opus-bsd` · `--test flac_byte_exact -p audio-codec-bsd` |
| **Quality invariants (randomized)** | `proptest` / `quickcheck` | Resampler round-trip · topology invariants · mixer linearity | `cargo test --features property -p audio-resample-bsd` (round-trip SNR≥90dB, frequency preservation) |
| **CPU profiling** | `cargo bench --profile-time` + dtrace | CPU%/stream · hotspots | `cargo bench -p audio-dsp-bsd -- dsp_fft -- --profile-time=5` · `dtrace -n 'profile-997 { @[ustack] = count; }'` |
| **Kernel/user profiling** | `hwpmc`(8) + `pmcstat` | System calls · kernel hot paths | `pmcstat -S instructions -O sonicbrew.pmc` → `pmcstat --text sonicbrew.pmc` |
| **Network/IO counters** | `netstat -s` · `gstat` · netmap statistics | netmap throughput · packet loss · IO wait | `netstat -s -p udp` · `gstat -a` (disk) · netmap ring counters |
| **ZFS ARC statistics** | `arcstat`(8) | ARC hit rate (asset-load cache) | `arcstat 1` (live ARC hits/misses at 1-second intervals) |
| **Concurrency stress** | `cargo test --test-threads=N` + `loom` (optional) | Lock-free rings · race conditions | `cargo test --features concurrency -p audio-graph-bsd -- --test-threads=4` · `cargo test --features loom -- loom_checked` |
| **FreeBSD target regression** | Native execution in a bhyve VM | FreeBSD-native measurements of all metrics | `cargo test --target x86_64-unknown-freebsd --workspace` (inside the VM) |

### Test-Node Environment

| Item | Value |
|------|-----|
| **Production target** | FreeBSD `14.2-RELEASE` (at least p3) |
| **bhyve test nodes** | FreeBSD `15.1-RELEASE` VMs (e.g. the `192.168.39.x` cluster including `192.168.39.2`) |
| **Target triple** | `x86_64-unknown-freebsd` |
| **VM prerequisites** | `rustup target add x86_64-unknown-freebsd` · `pkg install alsa-lib opus` |
| **RT verification constraint** | Real-time scheduler (rtprio) verification **must run inside a bhyve VM** — on non-FreeBSD hosts only `CountingAllocator` (alloc=0) can be verified (constraint note in [test-coverage-heatmap §7](../../notes/test-coverage-heatmap-design.md)) |

---

## 3. Latency Budget Table (from the architecture design)

> The [p10 §11.3](../../notes/p10-architecture-design.md) latency budget plus the [p11 §7c](../../notes/p11-mvp-scope-design.md) MVP measurable criteria. Per-path budgets and where the latency is consumed.

| Path | Target latency | Consumption points (where the latency goes) | Status | Rationale |
|------|----------|--------------------------------------|------|------|
| **Local playback** ( OSS via ALSA) | **< 5 ms** | 256-frame buffer @ 48k = 5.3ms (theoretical); measured < 10ms allowed when via-ALSA overhead is tolerated | Confirmed (the MVP spec §7c); measured < 10ms allowed | JACK-grade (P01 §9);  output-stream buffer |
| **Distributed RTP** ( netmap) | **< 1 ms** (one-way) | netmap zero-copy ring I/O + RTP header processing + PTP timestamp alignment | Confirmed (design spec); measured at multi-node rollout | AES67 standard; PTP hardware timestamps |
| **Browser WebSocket**  | **< 100 ms** (one-way); **< 200 ms** (round-trip) | WS frame deserialization (worker) → rtrb ring → graph RT processing → Sink → WS send | Confirmed (the MVP spec §7c) | WebSocket relaxation of WebRTC 20-100ms (simpler protocol) |
| **Browser WebRTC**  | **20–100 ms** | DTLS-SRTP handshake + Opus RTP encode/decode + network jitter | Confirmed (design spec); MVP stub | Typical for WebRTC (P08 §1); out of MVP scope per decision #2 |
| **PulseAudio compatibility**  | **50–200 ms** | PulseAudio protocol deserialization + sample-rate mapping + conversion overhead | Confirmed (design spec); measured at multi-node rollout | Typical for PulseAudio (P02 §3) |

> **Note:** The "measured < 10ms allowed" for local playback is an MVP relaxation that tolerates the via-ALSA overhead caused by the absence of a cpal OSS backend (see the note in [p11 §7c](../../notes/p11-mvp-scope-design.md)). Whether to build a direct OSS backend will be decided after prototype benchmarks ([p10 §11.4 #2](../../notes/p10-architecture-design.md)).

---

## 4. Target Status Tracking

> Tracks whether each target is confirmed. **Do not fabricate** — unconfirmed numbers may only move to confirmed after measurement.

| Status | Meaning |
|------|------|
| **Confirmed** | Target verified in the design specs — update the source first when changing |
| **TBD (benchmark back-calculation)** | Measurement protocol finalized but the number is not — derive it from benchmark results, then confirm |
| **Measuring** | Benchmark currently in progress — record results in the §6 template |

| Category | Metric | Target | Status |
|------|------|--------|------|
| RT/latency | xrun rate | 0 (absolute) | Confirmed (the MVP spec §7a) |
| RT/latency | RT-thread dynamic allocation | 0 (absolute) | Confirmed (the MVP spec §7a  #3) |
| RT/latency | End-to-end latency (local) | < 5ms (measured < 10ms allowed) | Confirmed (the MVP spec §7c) |
| RT/latency | End-to-end latency (WS one-way) | < 100ms | Confirmed (the MVP spec §7c) |
| RT/latency | RT pass latency (process_cycle) | within single-cycle budget | TBD (benchmark back-calculation) |
| RT/latency | jitter | < TBD µs | TBD (benchmark back-calculation) |
| Throughput | Total throughput (formula) | N × 384 kB/s | Confirmed (formula) |
| Throughput | Concurrent streams | TBD | TBD (benchmark back-calculation) |
| Throughput | RTP Gbps | TBD Gbps | TBD (benchmark back-calculation) |
| Resource | CPU%/stream | TBD % | TBD (benchmark back-calculation) |
| Resource | peak RSS | TBD MB | TBD (benchmark back-calculation) |
| Resource | ARC hit rate | TBD % | TBD (benchmark back-calculation) |
| Quality | SNR (resampling) | ≥ 90 dB | Confirmed (the MVP spec §7a  #1-2) |
| Quality | PESQ (Opus round-trip) | ≥ 3.5 | Confirmed (the MVP spec §7a  #1) |
| Quality | Codec transparency (PCM/WAV) | bit-exact | Confirmed (the MVP spec §7a  #2) |
| Quality | Bitrate accuracy | ±10% | Confirmed (the MVP spec §7a  #3) |
| Quality | DSP gain accuracy | ±1% | Confirmed (the MVP spec §7a  #2) |
| Recovery/availability | RTO (failover) | < 1s (target) | TBD — benchmark back-calculation (P04 §5.1 rationale) |
| Recovery/availability | RPO (WAL durability) | 0 (target) | TBD — benchmark back-calculation |
| Recovery/availability | Session restore time | TBD ms | TBD (benchmark back-calculation) |
| Distributed/scaling | Inter-node RTP latency | < 1ms | Confirmed (design spec); measured at multi-node rollout |
| Distributed/scaling | Scaling efficiency | TBD % | TBD (multi-node phase) |
| Distributed/scaling | Inter-node RTP jitter | < TBD µs | TBD (multi-node phase) |
| Distributed/scaling | Load-balancing efficiency | TBD | TBD (multi-node phase) |
| Network | FEC resilience (tolerable loss rate) | TBD % | TBD (multi-node phase) |
| Network | Jitter-buffer depth | TBD frames | TBD (multi-node phase) |
| Network | netmap throughput | TBD Mpps | TBD (benchmark back-calculation) |
| Network | Gateway throughput | TBD fps | TBD (benchmark back-calculation) |

---

## 5. Measurement Results Recording Template

> An empty template for recording future benchmark results. Copy and fill in a row for each measurement run. **The Target column quotes §1/§3/§4 of this document — do not modify it arbitrarily.**

### 5.1 RT/latency measurement results

| Date | Node/environment | Metric | Target | Measured (p50) | Measured (p99) | Pass? | Notes (measurement conditions) |
|------|-----------|------|--------|-------------|-------------|-------|-----------------|
| _e.g. 2026-__ | _e.g. bhyve 192.168.39.2_ | _e.g. RT pass latency_ | _e.g. < single-cycle budget_ | _____ | _____ | __ | _____ |
| | | | | | | | |
| | | | | | | | |

### 5.2 Throughput measurement results

| Date | Node/environment | Metric | Target | Measured | Pass? | Notes (stream count · sample rate · channels) |
|------|-----------|------|--------|--------|-------|-----------------------------|
| | | _e.g. concurrent streams_ | _e.g. TBD_ | _____ | __ | _e.g. N=__, 48k, 2ch, f32_ |
| | | | | | | |
| | | | | | | |

### 5.3 Resource measurement results

| Date | Node/environment | Metric | Target | Measured | Pass? | Notes |
|------|-----------|------|--------|--------|-------|------|
| | | _e.g. CPU%/stream_ | _e.g. TBD %_ | _____ | __ | |
| | | _e.g. peak RSS_ | _e.g. TBD MB_ | _____ | __ | |
| | | _e.g. ARC hit rate_ | _e.g. TBD %_ | _____ | __ | |
| | | | | | | |

### 5.4 Quality measurement results

| Date | Node/environment | Metric | Target | Measured | Pass? | Notes (golden sample · codec settings) |
|------|-----------|------|--------|--------|-------|--------------------------|
| | | _e.g. SNR (48k↔44.1k)_ | _e.g. ≥ 90dB_ | _____ dB | __ | _e.g. tone_440_48k_2ch.wav_ |
| | | _e.g. PESQ (Opus)_ | _e.g. ≥ 3.5_ | _____ | __ | _e.g. 48k/2ch, bitrate=64k_ |
| | | | | | | |
| | | | | | | |

### 5.5 Recovery/availability measurement results

| Date | Node/environment | Metric | Target | Measured | Pass? | Notes (topology size · WAL engine) |
|------|-----------|------|--------|--------|-------|------------------------------|
| | | _e.g. RTO (failover)_ | _e.g. < 1s_ | _____ s | __ | _e.g. redb, 10 nodes_ |
| | | _e.g. RPO (WAL durability)_ | _e.g. 0_ | _____ occurrences | __ | |
| | | | | | | |

### 5.6 Distributed/scaling measurement results

| Date | Cluster (node count) | Metric | Target | Measured | Pass? | Notes |
|------|-------------------|------|--------|--------|-------|------|
| | _e.g. 3 nodes_ | _e.g. scaling efficiency_ | _e.g. TBD %_ | _____ % | __ | |
| | | _e.g. inter-node RTP latency_ | _e.g. < 1ms_ | _____ ms | __ | |
| | | | | | | |

### 5.7 Network measurement results

| Date | Node/environment | Metric | Target | Measured | Pass? | Notes (NIC · netmap mode) |
|------|-----------|------|--------|--------|-------|------------------------|
| | | _e.g. FEC resilience_ | _e.g. TBD %_ | _____ % | __ | _e.g. ixgbe, zero-copy_ |
| | | _e.g. netmap throughput_ | _e.g. TBD Mpps_ | _____ Mpps | __ | |
| | | | | | | |

---

## Related Documents

- [ARCHITECTURE.md](./ARCHITECTURE.md) §3.3 (latency budget summary) · §7 (distributed 4-axis mechanisms) · §8 (RT-safety boundary mechanisms) — performance *mechanism* explanations
- [KNOWLEDGE.md](./KNOWLEDGE.md) §8.3 (RT separation principle) · §2 (net-rtp netmap mechanisms) · §7 ( metrics tooling) — domain knowledge
- [notes/p10-architecture-design.md](../../notes/p10-architecture-design.md) §11.3 (latency budget source) · §7.2 (RT-safety separation) · §11.4 (open items)
- [notes/p11-mvp-scope-design.md](../../notes/p11-mvp-scope-design.md) §7a (per-module acceptance source) · §7c (quantitative latency-target source)
- [notes/test-coverage-heatmap-design.md](../../notes/test-coverage-heatmap-design.md) §Methods 4-9 (measurement commands) · §7 (measurement regression thresholds) — test-coverage observability
- [TESTING-STANDARDS.md](../../TESTING-STANDARDS.md) §1-2 (5-layer pyramid · RT-specific gates)
- [PROGRESS.md](./PROGRESS.md) — progress + upcoming work (distributed metrics are a later measurement target)

---

**End of document.** This document defines "what to measure and what the targets are" for sonicbrew performance. Values confirmed in the design specs are marked with citations; unconfirmed numbers are explicitly marked TBD (benchmark back-calculation) — arbitrary fabrication is prohibited. For performance-achievement *mechanisms*, refer to ARCHITECTURE/KNOWLEDGE.
