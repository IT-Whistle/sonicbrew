# sonicbrew Audio Node Catalog

> The 23 `AudioNode` implementations provided by `audio-engine`. Every node honors the RT-safety contract: **all state is pre-allocated at construction, and `process` does only bounded arithmetic (no alloc/lock/panic, TESTING-STANDARDS §3.2)**.

## Summary

| Category | Node | Ports | Function |
|------|------|------|------|
| Source | SineSource¹ | 0-in / 1-out | Fixed sine wave (pre-computed) |
| Source | NoiseSource | 0-in / 1-out | White/pink noise (xorshift64 + Paul Kellet filter, seedable) |
| Source | ToneGenerator | 0-in / 1-out | Multi-waveform oscillator (sine/square/saw/triangle, phase accumulator) |
| Source | FileSource | 0-in / 1-out | Preloaded-buffer playback (looping/position tracking/end detection) |
| Effect | Gain¹ | 1-in / 1-out | Linear gain |
| Effect | EqNode | 1-in / 1-out | RBJ biquad EQ (6 filter types) |
| Effect | CompressorNode | 1-in / 1-out | Dynamic range compression (peak envelope) |
| Effect | LimiterNode | 1-in / 1-out | Brickwall limiter (zero latency) |
| Effect | NoiseGateNode | 1-in / 1-out | Noise gate (attack/hold/release) |
| Effect | DelayNode | 1-in / 1-out | Feedback delay (wet/dry mix) |
| Effect | ChorusNode | 1-in / 1-out | LFO-modulated delay chorus (fractional interpolation) |
| Effect | FlangerNode | 1-in / 1-out | LFO + feedback flanger |
| Effect | PhaserNode | 1-in / 1-out | LFO-modulated allpass cascade (N stages) |
| Effect | ReverbNode | 1-in / 1-out | Schroeder/Freeverb (8 comb + 4 allpass) |
| Effect | DistortionNode | 1-in / 1-out | Waveshaper (soft/hard clip, foldback, overdrive) |
| Effect | BitcrusherNode | 1-in / 1-out | Bit-depth quantization + sample-rate reduction (S&H) |
| Effect | TremoloNode | 1-in / 1-out | LFO amplitude modulation |
| Effect | StereoWidenerNode | 1-in / 1-out | mid/side stereo width control |
| Effect | ChannelMapNode | 1-in / 1-out | Channel routing (swap/mute/pan/mono↔stereo) |
| Effect | MeterNode | 1-in / 1-out | Passthrough + atomic peak/RMS metering |
| Routing | MixerNode | N-in / 1-out | Input-summing mix bus (per-input gain) |
| Routing | AuxSendNode | 1-in / 2-out | Aux split (out0=main passthrough, out1=aux tap with send_level applied) |
| Sink | Capture¹ | 1-in / 0-out | Observation sink (reads input scratch via `Graph::read_input`) |

¹ Lives in the `builtins` module. The rest are in `audio_engine::nodes`.

## Source Nodes

### NoiseSource
```rust
NoiseSource::new(color: NoiseColor, amp: f32, seed: u64, channels: u16)
// NoiseColor::{White, Pink}
```
Same seed → same sequence (reproducible). REST kind `"noise"`.

### ToneGenerator
```rust
ToneGenerator::new(waveform: Waveform, freq: f32, amp: f32, sample_rate: u32, channels: u16)
// Waveform::{Sine, Square, Saw, Triangle}
```
Phase accumulator + wrapping, so even long runs never overflow. REST kind `"tone"`.

### FileSource
```rust
FileSource::new(samples: Vec<f32>, channels: u16, sample_rate: u32, looping: bool)
FileSource::into_parts(self) -> (Vec<f32>, u16, u32, bool)
// Observation: is_ended(), position(), total_frames()
```
Plays a planar buffer. Empty buffer = immediate end (silence). REST kind `"file"` is only reachable via the `--load-file` CLI / FileBufferRegistry.

### File loading (worker-thread only)
```rust
audio_engine::nodes::load_file_source(path, looping) -> Result<FileSource, CodecError>
```
Sniffs FLAC/WAV/PCM by magic bytes via audio-codec-bsd and decodes the whole file. **Never call from the RT thread.**

## Effect Node Highlights

### EqNode — RBJ biquad
`FilterType::{LowPass, HighPass, BandPass, Peaking, LowShelf, HighShelf}`, Direct Form I Transposed. Coefficients are computed at construction.

### ReverbNode — Freeverb structure
Independent engine per channel: 8 parallel combs (with damping lowpass, feedback = room_size×0.28+0.7) → summed → 4 series allpass (feedback 0.5). Comb/allpass lengths scale the 44.1kHz reference constants proportionally to sample_rate. The first wet tail arrives after ~1200 samples.

### DelayNode / ChorusNode / FlangerNode — time effects
- **Delay**: per-channel ring buffer, feedback clamped <0.99
- **Chorus**: LFO-modulated fractional delay + linear interpolation (no feedback, center > depth guaranteed)
- **Flanger**: chorus + feedback (clamped ≤0.9), read-then-write ordering keeps it stable

### DistortionNode — 4-mode waveshaper (stateless)
`SoftClip` (tanh) / `HardClip` / `Foldback` (bounded iterative folding) / `Overdrive` (asymmetric exp saturation).

### PhaserNode — shared-LFO allpass cascade
N-stage (2–8) 1st-order allpass with coefficient `a = tan(w₀/2)`; the loop order (samples outer, channels inner) aligns stereo phase. base_freq is clamped to sr/4 (Nyquist-safe).

### BitcrusherNode
Sample-rate reduction (per-channel S&H counter) → bit-depth quantization (2^bits symmetric levels, mid-tread).

## Routing / Mixing

### MixerNode
`MixerNode::new(n: usize, gains: Vec<f32>, channels: u16)` — N-input summing bus. `set_gain` (control thread only).

### AuxSendNode — the heart of the aux bus
```rust
AuxSendNode::new(send_level: f32, channels: u16)  // 0.0..=1.0
```
Output 0 = main (passthrough), output 1 = aux (`input × send_level`). **Multi-port links** (`from_port` on `POST /links`) connect each output to a separate path:

```
Source ─► AuxSend ─ out0(main) ─► Mixer in0 ─► ...
                  └─ out1(aux)  ─► Reverb ──► Mixer in1
```

## REST kind Dispatch

`EngineServerFactory`'s `render_node` dispatches the 20 kinds (NodeParams table in [REST-API.md](./REST-API.md)). kind/params mismatches are rejected up front by control-api with a 400; with params omitted, the node is created with the kind's defaults. Unknown kinds fall back to a `Gain(1.0)` passthrough (avoids hard rebuild failures).

## RT Orchestration (same crate)

- `GraphEngine::step` — process_cycle (RT) → flush_sinks (between cycles) → rebuild swap (between cycles, latest-wins)
- `GatewayBridge` — gateway workers survive rebuilds (transparently swapped to new rtrb handles)
- `spawn_rebuild_task` — subscribes to `TopologyEvent` → `build_graph` → loads the RebuildSlot
- `BuiltNode::{Plain, Sink}` — flushable sinks register via `add_sink` so that `flush_sinks` drains them
