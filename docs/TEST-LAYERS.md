# sonicbrew Subproject Test Coverage Matrix (TEST-LAYERS)

> **Document type:** **Layer-by-layer test coverage matrix** for the sonicbrew server modules. Applies the umbrella heatmap policy ([`../../docs/test-coverage-heatmap.html`](../../docs/test-coverage-heatmap.html)) and the 5-layer pyramid of [TESTING-STANDARDS.md](../../TESTING-STANDARDS.md) to the sonicbrew subproject.
>
> **Baseline date:** 2026-08-17 (updated) · **Tests:** **566 unit/integration/property tests + 5 self-tests + 7 criterion benches + 2 cargo-fuzz targets** — fmt/clippy(`-D warnings`)/FreeBSD `cargo check` all GREEN
> **Scope:** This document is limited to the 7 sonicbrew modules + audio-engine. For the 10 audio-toolkit crates, refer to the umbrella heatmap.

---

## 1. Intensity scale (i0–i5) — umbrella heatmap compliance

| Symbol | Meaning | Recommended subtest count |
|------|------|-------------------|
| **i0** | None/irrelevant (no tests needed) | 0 |
| **i1** | Minimal (smoke/existence check) | 1–2 |
| **i2** | Basic (basic cases) | 2–4 |
| **i3** | Normal (core paths + edges) | 4–8 |
| **i4** | Deep (comprehensive — paths/edges/regressions) | 8–16 |
| **i5** | Critical/mandatory (defect = fatal, top priority) | 16+ (unlimited regressions) |
| **✕** | Not applicable (the method cannot be applied, reason stated) | 0 |

---

## 2. sonicbrew module × test method matrix (based on measurement)

> 10 test methods = identical to the umbrella heatmap.

| Module | Unit | Property | Integration | RT-safety | Audio Q | Concurrency | Protocol | Performance | FreeBSD | Sanitizer | Test count |
|------|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| session-store | **i4✓** | **i3✓** | **i4✓** | ✕ | ✕ | **i5✓** | ✕ | i3(bench) | **i2✓** | — | **34** |
| net-rtp-aes67 | **i4✓** | **i3✓** | **i4✓** | — | — | i2 | **i5✓** | **i4✓**(bench) | i1 | **i4✓** | **98** |
| gw-pulse | **i4✓** | — | i2 | — | — | — | **i5✓** | — | i1 | **i4✓** | **82** |
| gw-alsa | **i4✓** | — | i2 | — | — | — | **i4✓** | — | i1 | **i3✓** | **79** |
| gw-browser | **i4✓** | — | **i5✓** | **i3✓** | — | i2 | **i4✓** | **i4✓**(bench) | i1 | **i4✓** | **60** |
| control-api | **i4✓** | — | **i4✓** | ✕ | ✕ | **i4✓** | ✕ | — | i1 | **i3✓** | **84** |
| monitor | **i3✓** | — | **i3✓** | ✕ | ✕ | — | ✕ | **i3✓** | i1 | — | **14** |
| **audio-engine** | **i4✓** | — | **i4✓** | **i3✓** | **i4✓** | **i3✓** | ✕ | — | i1 | — | **84** |
| **bin** sonicbrew | — | ✕ | **i5✓**(self-test) | i3(self-test) | — | — | — | i3(self-test) | i1 | — | **5 self-test** |

### audio-engine detail (20 audio node types + 3 builtins + bridge)

audio-engine is sonicbrew's runtime orchestration layer, containing 23 `AudioNode` implementations (DSP/source/sink) and the live rebuild mechanism:

| Node category | Nodes | Tests |
|--------------|------|:---:|
| **Sources (0-in/1-out)** | SineSource, NoiseSource(white/pink), ToneGenerator(4 waveforms), FileSource(looping) | 4–6 each |
| **Effects (1-in/1-out)** | Gain, Eq(biquad), Compressor, Limiter, NoiseGate, Delay, Chorus, Flanger, Phaser, Reverb(Freeverb), Distortion(4 modes), Bitcrusher, Tremolo, StereoWidener, ChannelMap, Meter | 2–6 each |
| **Mixing/routing** | MixerNode(N-in/1-out), AuxSendNode(1-in/2-out) | 2–5 each |
| **RT orchestration** | GraphEngine, build_graph, GatewayBridge, spawn_rebuild_task | 23 |

> Audio Q (i4): verification of each node's acoustic accuracy — DC pass-through, frequency attenuation, impulse response decay, quantization steps, feedback stability, etc.
> RT-safety (i3): all nodes perform struct allocation only; alloc/lock/panic are forbidden in `process`. The builtins `SineSource`/`Gain`/`Capture` use simple copy/arithmetic only.

---

## 3. Layer highlights (aligned with the TESTING-STANDARDS 5-layer pyramid)

| Layer | sonicbrew application | Representative tests |
|-------|---------------|-----------|
| **Layer 0 unit** | inline `#[test]` in all modules | session-store WAL restore, RTP header parsing, REST response codes, audio-engine per-node DSP verification |
| **Layer 1 integration** | `#[tokio::test]` in `tests/` directories | session-store cluster.rs (3-node), gw-browser ws_to_graph, control-api rest_api (84), audio-engine node_pipeline.rs (chaining) |
| **Layer 2 property** | proptest | session-store Mutation round-trip/WAL idempotency, RTP L16 round-trip, gateway/control-api sanitizer proptests |
| **Layer 3 performance** | criterion microbenchmarks + self-test | gw-browser ws_round_trip (3), rtp_codec (4), monitor percentile/bulk samples, bin --self-test 3µs |
| **Layer 4 FreeBSD regression** | native execution on a dedicated test machine (15.1-RELEASE-p2, amd64) | ✅ **build + tests 556/556 + 5 self-tests + server smoke (REST/persistence) passed** (2026-08-17, rust 1.96.1). Continuous regression CI to follow |

### Sanitizer/Fuzz layer (heatmap method 10)
- Net/gateway modules: proptest-based no-panic guarantees on malformed/random inputs + deterministic edge cases (oversized frames, truncated headers, full tag sweep).
- **cargo-fuzz targets (nightly)**: `gw-browser/fuzz/fuzz_targets/ws_parser.rs` + `net-rtp-aes67/fuzz/fuzz_targets/rtp_packet_parser.rs` — libFuzzer coverage-guided fuzzing.

---

## 4. Verification commands (gates)

```bash
# Full workspace quality gate
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                    # 566 passed
cargo check --workspace --target x86_64-unknown-freebsd

# Per-module focus
cargo test -p session-store               # 34
cargo test -p net-rtp-aes67               # 98
cargo test -p gw-browser                  # 60
cargo test -p gw-pulse                    # 82
cargo test -p gw-alsa                     # 79
cargo test -p control-api                 # 84
cargo test -p monitor                     # 14
cargo test -p audio-engine                # 84

# criterion performance benches (regression gates)
cargo bench -p gw-browser -- ws_round_trip
cargo bench -p net-rtp-aes67 -- rtp_codec

# Deterministic self-tests (server entry point)
cargo run -p sonicbrew -- --self-test             # bidirectional flush + monitor
cargo run -p sonicbrew -- --gateway-live-reload-test
```

---

## 5. Remaining gaps (unmet vs. heatmap targets)

| Cell | Target | Current | Notes |
|----|:---:|:---:|------|
| gw-browser Protocol/Sanitizer | i5 | i4/i5 | WS RFC 6455 masking/fragmentation tests need to be added; cargo-fuzz target set up (nightly run) |
| net-rtp FreeBSD/Sanitizer | i5 | **i4✓**/i4 | native stats (98/98) + netmap probe (API 14, vmx0/VALE geometry) + ring TX (`nm_port_test`) + **full RTP loopback through the kernel VALE switch (`vale_loopback`, 8/8 bit-exact — zero-copy path verified end-to-end)**. Added to the regression suite as §8 |
| bin FreeBSD | i5 | **i3✓** | Native: build + tests + 5 self-tests + server smoke (live reload/persistence) + gw-pulse handshake (real PA 17.0) + gw-alsa aplay streaming passed. Nightly regression CI to follow |
| AES67 full compliance | — | out of scope | SAP/SDP, FEC, SRTP, PTP — upon distributed expansion |

---

## Related documentation

- [umbrella test coverage heatmap](../../docs/test-coverage-heatmap.html) — full 19-module × 10-method matrix
- [TESTING-STANDARDS.md](../../TESTING-STANDARDS.md) — 5-layer pyramid + RT-specific gates
- [PROGRESS.md](./PROGRESS.md) — sonicbrew build progress
- [ADR-0003](./adr/0003-openraft-multi-node-consensus.md) — openraft multi-node consensus
- [ADR-0004](./adr/0004-audio-processing-nodes.md) — 6 audio processing nodes (initial)
- [ADR-0005](./adr/0005-nodespec-typed-params.md) — NodeSpec typed params
- [ADR-0006](./adr/0006-multi-port-linking-and-topology.md) — multi-port links + GET /links + GET /topology
