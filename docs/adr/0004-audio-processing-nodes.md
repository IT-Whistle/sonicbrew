# ADR 0004 — Audio processing nodes (mixer / EQ / compressor / limiter / meter / channel-map)

- **Status:** Accepted
- **Date:** 2026-08-07
- **Author:** sonicbrew contributors
- **Related:** `ROADMAP-GOVERNANCE.md` §6 (vanity anti-pattern); `TESTING-STANDARDS.md` §3.2 (RT-safety)

## Context

sonicbrew's graph engine (`audio-graph-bsd`) and integration layer (session-store, gateways, control-api) were solid, but the graph contained only three node types: `SineSource`, `Gain`, and `Capture`. An audio server that can only pass a sine through a gain stage is not an audio server — it lacks the **table-stakes** processing that every routing/mixing/effects pipeline needs:

- **Mixing** — sum multiple sources into one bus.
- **Channel handling** — mono↔stereo, panning, channel swap/mute.
- **Equalisation** — frequency shaping (biquad filters).
- **Dynamics** — compression and limiting (preventing clipping).
- **Metering** — real-time peak/RMS level measurement.

`ROADMAP-GOVERNANCE.md` §6 warns against "feature-count competition" (vanity), but audio processing nodes are not features competing with PipeWire — they are the **minimum viable processing** that makes the integration layer useful. Without them, the server can route audio but cannot shape it.

## Decision

Add six `AudioNode` implementations in a new `audio-engine::nodes` module:

| Node | Ports | Function |
|------|-------|----------|
| `MixerNode` | N-in / 1-out | Sum N inputs with per-input gain — the mixing bus |
| `ChannelMapNode` | 1-in / 1-out | Channel routing: swap, mute, pan, mono↔stereo |
| `EqNode` | 1-in / 1-out | RBJ biquad (low/high pass, band pass, peaking, shelf) |
| `CompressorNode` | 1-in / 1-out | Dynamic range compression (threshold/ratio/attack/release/makeup) |
| `LimiterNode` | 1-in / 1-out | Brick-wall limiter (zero-latency) |
| `MeterNode` | 1-in / 1-out | Passthrough + RT-safe peak/RMS via `AtomicU32` |

Every node honours the `AudioNode` RT-safety contract: all state is pre-allocated at construction; `process` does only bounded sample arithmetic with no allocation, locking, or panicking.

The server's `EngineServerFactory` (in `crates/sonicbrew/src/main.rs`) is extended to dispatch these six kinds via the existing `kind`-string match, so `POST /nodes {"kind":"eq"}` creates an `EqNode` in the live graph. Each kind ships with **default parameters** (e.g. EQ = 1 kHz peaking +0 dB passthrough); custom parameters require a `NodeSpec` extension (see Consequences).

## Consequences

- **Positive:** sonicbrew now performs real audio processing — mixing, EQ, dynamics, metering — making the integration layer (gateways, session, control-api) genuinely useful rather than a signal passthrough.
- **Positive:** All nodes are RT-safe by construction (pre-allocated state, bounded loops) and unit-tested (12 node tests + 4 chaining integration tests = 16 new tests).
- **Negative:** `NodeSpec` currently carries only a `kind: Option<String>` — no typed parameters. Users cannot yet specify EQ frequency, compressor threshold, etc. via REST. Each kind uses hardcoded defaults.
- **Mitigation / follow-up:** A future ADR will extend `NodeSpec` with an optional `params` sub-struct (serde-defaulted for backward compatibility), and the factory will read params per kind. This is a wire-protocol change requiring its own ADR per `sonicbrew/GOVERNANCE.md` §2.2 ("control API contract change").

## Compliance

This ADR adds new `AudioNode` types and extends the factory's internal dispatch. It does **not** change:
- The `AudioNode` trait (audio-core-bsd, unchanged).
- The `SessionStore` trait (unchanged).
- The `NodeSpec` wire format (unchanged — new kinds use defaults).

The future `NodeSpec` params extension will be a separate ADR (wire-protocol change, §2.2 gate).

## Related

- `audio-engine/src/nodes/` — the six node implementations + unit tests
- `audio-engine/tests/node_pipeline.rs` — chaining integration tests
- `crates/sonicbrew/src/main.rs` `EngineServerFactory` — kind dispatch
- `ROADMAP-GOVERNANCE.md` §6 — vanity vs table-stakes distinction
- `TESTING-STANDARDS.md` §3.2 — RT-safety contract (alloc=0 in process)
