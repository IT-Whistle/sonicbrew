# Domain Concepts

Background knowledge for developers who know Rust but have not worked on audio servers. Each section explains the *domain* first, then grounds it in the sonicbrew implementation. Protocol and math claims are taken from the verified sources in `crates/` — where this document and the code disagree, the code wins.

## 1. The audio graph model

### Nodes, ports, links

sonicbrew processes audio as a **directed graph**. A *node* is one DSP unit
(a sine oscillator, an EQ, a mixer) exposing typed **ports** — a
`PortDescriptor` declares direction, channel count, and sample format
(always planar `f32` here). A **link** connects one output port of a node to
one input port of another (`from`/`from_port`/`to`/`to_port` in the REST
API). Multi-port nodes make port indexes matter: `MixerNode` has N input
ports, `AuxSendNode` has two output ports (main + aux tap), and links target
a specific port index.

Every node implements the `AudioNode` trait (from `audio-core-bsd`, consumed
from crates.io — the single runtime contract between sonicbrew and the
audio-toolkit crate family):

```rust
fn process(&mut self, ctx: &mut ProcessContext,
           in_frames: &[AudioFrame], out_frames: &mut [AudioFrame]);
```

The engine calls `process` once per **block** (a fixed batch of frames), not
per sample; per-sample work happens inside the node in a tight loop.

### Planar f32 buffers

An `AudioFrame` carries one block for all channels in **planar** layout —
all of channel 0's samples contiguously, then all of channel 1's, etc.:

```text
planar stereo, 4 frames:      [L0 L1 L2 L3  R0 R1 R2 R3]
interleaved stereo, 4 frames: [L0 R0  L1 R1  L2 R2  L3 R3]
```

Planar is the graph's internal convention: channel `c`, frame `i` lives at
`samples[c * per_ch + i]`, which keeps per-channel DSP state and buffer
indexing simple. The *outside world* is interleaved — RTP payloads, the
PulseAudio wire — so every gateway converts planar<->interleaved at the edge
(e.g. `net-rtp-aes67/src/codec.rs`).

### Why ports declare channel counts

The port's channel count is declared up front so the engine can **size every
planar buffer when the graph is built** and validate links at build time —
a channel-count mismatch is a build-time rejection, not a mid-block surprise
on the RT thread. Gateway layout conversion also keys off the declared count
(a stereo-only RTP path knows exactly how to de-interleave its payload).

### Block sizes and latency math

The MVP engine configuration is stereo, 48 kHz, **256-frame blocks**
(`crates/sonicbrew/src/main.rs`: `NUM_FRAMES = 256`, `SAMPLE_RATE = 48_000`).
One block's duration:

```text
256 frames / 48000 Hz = 5.33 ms
```

That 5.33 ms is the granularity of everything: the RT loop sleeps one block
per cycle, gateway rings are refilled per block, and graph swaps happen on
block boundaries. End-to-end latency accumulates roughly one block per
buffering stage (ring in, graph, ring out, plus any jitter-buffer depth).

### Cycle structure: process, flush, swap

`GraphEngine::step` runs, in order, every cycle:

1. `process_cycle` — the RT graph pass (allocation-free),
2. `flush_sinks` — drains registered sink nodes *between* cycles,
3. **rebuild swap** — atomically installs a newly built graph if one is
   ready (latest-wins), again between cycles.

Gateway workers survive rebuilds through `GatewayBridge`, which hands them
new rtrb handles for the swapped-in graph.

*Implementation: `crates/audio-engine/src/lib.rs` (`GraphEngine`,
`GatewayBridge`, `spawn_rebuild_task`), `crates/sonicbrew/src/main.rs`
(engine constants, tick loop). Catalog: [AUDIO-NODES.md](./AUDIO-NODES.md).*

## 2. Real-time safety

### What alloc, lock, and panic cost on the audio thread

Audio has a hard per-block deadline: 256 samples at 48 kHz must be produced
every 5.33 ms, every time. The three classic violations:

- **Allocation** — `malloc`/`Vec::push` may take a page fault, contend on
  the allocator's internal lock, or ask the OS for memory; every one is
  *unbounded in time*.
- **Locking** — a contended mutex blocks for as long as the holder wants;
  under priority inversion it can block effectively forever.
- **Panic/unwinding** — panicking runs destructors and formatting, which
  allocate and take locks, tearing the audio path down mid-block.

### Pre-allocation discipline

All state is pre-allocated in constructors; `process` does only bounded
arithmetic (TESTING-STANDARDS §3.2). Cross-thread data moves through
pre-built SPSC rings (`rtrb`) or atomics; immutable state is shared via
`Arc` swaps that happen *between* cycles, never inside one.

### The xrun concept

An **xrun** (over-run/under-run) is what happens when a bounded buffer
misses its schedule: a consumer reads a frame the producer has not written
(underrun — glitch/repeat), or a producer drops frames because the consumer
has not drained (overrun). In sonicbrew, xruns surface at the edges — the
rtrb rings to gateways, the RTP jitter buffer (a full buffer returns
`PushOutcome::Rejected`, deliberately countable as an xrun) — and are
recorded by `monitor`'s `MetricsRecorder` alongside latency p50/p99.

### The CountingAllocator test technique

"How do you know `process` never allocates?" — count it. The pattern (from
`crates/gw-browser/tests/rt_alloc_free.rs`):

```rust
struct CountingAllocator;
thread_local! {
    static RT_MEASURING: Cell<bool> = const { Cell::new(false) };
    static RT_ALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
}
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = RT_MEASURING.try_with(|m| {
            if m.get() {
                let _ = RT_ALLOC_COUNT.try_with(|c|
                    c.set(c.get().saturating_add(1)));
            }
        });
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { System.dealloc(p, l) }
}
#[global_allocator]
static A: CountingAllocator = CountingAllocator;
```

The measurement window brackets only the `process_cycle` loop (build the
graph freely, warm up two cycles, then measure) and asserts **zero
allocations across 1000 cycles**. The counters are *thread-local* on
purpose: a global measuring flag gives false positives because the libtest
harness does bookkeeping allocations on background threads that land inside
the window.

### Soft-RT polling model (not SCHED_FIFO yet)

sonicbrew's RT thread is a plain `std::thread` that processes one block and
sleeps for one block duration (`FRAME_DURATION = 256/48000` = 5.33 ms) — a
**polling soft-RT loop**, not an `SCHED_FIFO` scheduler thread. Consequences:

- No privileges needed (`rtprio`, `mlockall` unnecessary) — runs
  unprivileged and inside containers/VMs.
- A cycle overrun produces a *late block* (an xrun at the next bounded
  buffer), not a system-wide scheduling hazard.
- The cost is jitter: a normal-priority thread can be delayed by other
  work, so latency percentiles (p50/p99 via `monitor`) — not worst-case
  guarantees — are the honest metric.

The hard-RT upgrade (SCHED_FIFO + memory locking + timer-driven deadline)
remains available later; the alloc-free `process_cycle` discipline is
exactly its prerequisite.

*Implementation: `crates/gw-browser/tests/rt_alloc_free.rs`
(CountingAllocator), `crates/sonicbrew/src/main.rs` (tick loop),
`crates/monitor/src/lib.rs` (xrun/latency metrics), [ARCHITECTURE.md](./ARCHITECTURE.md) §4.*

## 3. DSP fundamentals used by the nodes

The minimum signal-processing theory needed to read the node catalog. Each
subsection maps to code in `crates/audio-engine/src/nodes/`.

### Biquad filters (RBJ cookbook, Direct Form I Transposed)

`EqNode` is a second-order IIR ("biquad") supporting the six RBJ cookbook
shapes: low-pass, high-pass, band-pass, peaking, low-shelf, high-shelf.
From cutoff `f0`, Q, gain in dB, sample rate `Fs`:

```text
w0    = 2*pi*f0 / Fs
alpha = sin(w0) / (2*Q)
A     = 10^(gain_dB / 40)          # peaking/shelf only
```

low-pass row of the coefficient table (all six in `eq.rs`):

```text
b0 = (1 - cos w0)/2    b1 = 1 - cos w0    b2 = (1 - cos w0)/2
a0 = 1 + alpha         a1 = -2 cos w0     a2 = 1 - alpha
```

Coefficients are normalized by `a0` **at construction** (divide once, never
in the loop). The filter runs as Direct Form I Transposed — two state
registers per channel:

```text
y    = b0*x + z1
z1'  = b1*x - a1*y + z2
z2'  = b2*x - a2*y
```

### Envelope followers (one-pole attack/release)

Dynamics nodes (compressor, limiter, gate) track loudness with a peak
envelope — a one-pole smoother whose coefficient depends on direction:

```text
env <- env + coef * (|x| - env)
coef = attack_coef   if |x| > env    (rise quickly)
     = release_coef  otherwise       (fall slowly)
```

A millisecond time constant becomes a per-sample coefficient via

```text
coef = 1 - exp(-1 / (T_ms * 0.001 * Fs))
```

The compressor computes gain from the envelope — above threshold `T`, fold
the excess back by the ratio, plus linear makeup gain:

```text
over       = env / T
compressed = T * over^(1/ratio)
gain       = compressed / env
```

### Delay lines and fractional interpolation

A delay is a per-channel ring buffer with a write cursor; reading "D samples
ago" is `buf[(wp - D) mod N]`. Non-integer D (needed when the delay is
modulated) reads fractionally with **linear interpolation**:

```text
y = s[i0]*(1 - frac) + s[i1]*frac        # i1 = i0 + 1 mod N
```

Feedback (output mixed back into the input) creates echoes and must stay
strictly below 1.0 for BIBO stability: `DelayNode` clamps feedback < 0.99,
`FlangerNode` <= 0.9, and flanger reads *before* writing so the loop gain
stays bounded.

### LFO modulation family: chorus / flanger / tremolo / phaser

A low-frequency oscillator advances a phase every sample:

```text
phase += 2*pi*rate / Fs        (wrapping at 2*pi)
```

- **Chorus** — delay swept by the LFO: `current_delay = center +
  depth*sin(phase)`, read fractionally, **no feedback**. Sweeping delay
  resamples the signal (Doppler detune). The center is kept strictly
  greater than depth — `center = max(5 ms, depth + 1 ms)` — so the delay is
  always positive.
- **Flanger** — the same modulated delay but *short* center with feedback
  <= 0.9, producing the resonant swept-comb sound.
- **Tremolo** — amplitude modulation: the LFO scales the signal inside a
  depth-controlled band (no delay line).
- **Phaser** — the LFO sweeps the cutoff of a cascade of 2–8 first-order
  allpass sections (coefficient `a = tan(w0/2)`), with feedback <= 0.7;
  `base_freq` is clamped to `sr * 0.24` so the modulated cutoff stays below
  Nyquist.

### Freeverb (8 combs + 4 allpasses)

`ReverbNode` is the classic Schroeder/Freeverb architecture, one engine per
channel: input fans out to **8 parallel comb filters**, their sum cascades
through **4 series allpass filters**. Each comb is a delay with a one-pole
lowpass ("damper") in its feedback loop:

```text
output   = buf[i]
lp       = output*(1 - damping) + lp*damping      # damper
buf[i]   = input + lp * feedback                  # feedback loop
feedback = room_size * 0.28 + 0.7                 # 0.7..0.98, always < 1
```

The allpass is `out = -input + buffered`,
`buf[i] = input + 0.5*buffered` (feedback fixed at 0.5, the Schroeder
constant). Delay lengths are the Freeverb 44.1 kHz reference tables scaled
proportionally to `Fs/44100`:

```text
combs:    [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617]
allpass:  [556, 441, 341, 225]
```

The irregularly-spaced lengths decorrelate echo density; the first wet tail
arrives only after ~1200 samples (the shortest comb).

### Waveshaping (distortion)

Distortion is a memoryless nonlinearity `y = f(x)` per sample. `DistortionNode`
offers four: `SoftClip` (tanh — smooth, bounded to ±1, harmonics roll off
gently), `HardClip` (clamp to a threshold), `Foldback` (bounded iterative
folding of over-range values back into range), and `Overdrive` (asymmetric
exp saturation — asymmetry adds even harmonics). Stateless, hence trivially
RT-safe.

### Mid/side stereo

A stereo signal decomposes as mid (sum) and side (difference):

```text
M = (L + R)/2         S = (L - R)/2          # encode
L' = M + S*width      R' = M - S*width       # decode, width clamped 0.0..=2.0
```

Width 0 collapses to mono, 1 is passthrough, 2 doubles the side component
(exaggerated image). Mono input passes through unchanged.

### Bitcrush (sample-and-hold + quantize)

`BitcrusherNode` applies two lo-fi effects in order: **sample-rate
reduction** via a per-channel sample-and-hold counter (the held value
updates once per `hold_factor` samples, clamped 1..=256), then **bit-depth
quantization** to `2^bits` symmetric mid-tread levels — a uniform staircase
centered on zero.

*Implementation: `crates/audio-engine/src/nodes/` (`eq.rs`, `compressor.rs`,
`limiter.rs`, `noise_gate.rs`, `delay.rs`, `chorus.rs`, `flanger.rs`,
`tremolo.rs`, `phaser.rs`, `reverb.rs`, `distortion.rs`,
`stereo_widener.rs`, `bitcrusher.rs`).*

## 4. RTP / AES67 networking

### The RTP header (RFC 3550)

Real-time media over UDP uses RTP. The fixed header is 12 bytes, all
integers big-endian:

```text
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+---+---+---+---+---+---+---+---+---+---+---+---+================+
|V=2| P | X |  CC  |M |     PT      |       sequence number      |
+---+---+---+---+---+---+---+---+---+---+---+---+================+
|                           timestamp                           |
+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+
|                             SSRC                              |
+---------------------------------------------------------------+
```

- **seq** (u16): +1 per packet — the reordering key, wraps at 65536.
- **timestamp** (u32): media-clock position of the *first frame* in the payload.
- **SSRC** (u32): random stream identifier.
- **PT** (7 bits): payload format. The crate maps L16 stereo = 10 and
  L16 mono = 11 (RFC 3551 static types), L24 = 96 and Opus = 97 (dynamic
  types — in real deployments agreed via SDP).

### L16/L24 payload framing

Uncompressed PCM payloads are **big-endian, interleaved** signed samples —
L16 is 2 bytes/sample, L24 is 3 bytes (sign extended from bit 23). The graph
is planar f32, so the codec converts layout and scale:

```text
encode (L16): int = round(clamp(f, -1, 1) * 32767)
decode (L16): f   = int / 32768
encode (L24): int = round(clamp(f, -1, 1) * 8388607)    # 2^23 - 1
decode (L24): f   = int / 8388608                        # 2^23
```

The deliberate asymmetry (32767 up / 32768 down; 2^23-1 up / 2^23 down) is
the standard audio convention and keeps round-trip error under one LSB.

### Per-channel timestamp advance

The RTP media clock counts *frames* — one sample per channel — not bytes. A
payload of `len` bytes carries `len / (bytes_per_sample * channels)` frames,
and the next packet's timestamp must advance by exactly that count. Hence
`timestamp_advance(len, codec, channels)`: 16 bytes of stereo L16 = 4 frames
= timestamp +4. Advancing by bytes instead would run the stream 2x (stereo)
or 3x (L24) fast.

### Jitter buffer: wrap-aware reordering

UDP reorders and duplicates packets; the receive side re-orders by sequence
number. Because `u16` wraps, comparisons use *signed* arithmetic — the
crate's `forward_distance(from, to)` is `to.wrapping_sub(from) as i16`,
correct within a half-window of 32768 packets. `JitterBuffer` semantics: the
first pushed packet establishes the emission baseline; packets ahead of
`next_seq` are buffered (capacity-bounded, newest loses on overflow),
behind-or-already-seen seqs are duplicates and dropped; an explicit
gap-skip advances past losses.

### AES67: what sonicbrew has vs not

AES67 is professional-audio-over-IP interoperability: RTP media plus PTP
clock sync, multicast distribution, and SAP/SDP session announcement.

| Capability | In `net-rtp-aes67`? |
|------------|---------------------|
| RTP media (RFC 3550 header codec) | have |
| L16/L24 PCM framing | have |
| UDP transport + send/recv worker loops | have |
| Wrap-aware jitter buffer | have |
| Zero-copy netmap/VALE dataplane (FreeBSD) | have (`cfg(freebsd)`) |
| PTP (IEEE 1588) clock alignment | not yet (out of scope) |
| Multicast + SAP/SDP session announcement | not yet |
| FEC, SRTP/DTLS | not yet |

*Implementation: `crates/net-rtp-aes67/src/{codec.rs, jitter.rs,
transport.rs, worker.rs}`.*

## 5. The PulseAudio native protocol

### The unix-socket daemon model

A PulseAudio daemon listens on a unix domain socket; clients speak the
native protocol directly over it (no HTTP/RPC layer). The gateway
(`gw-pulse`) implements the protocol in **pure Rust** instead of binding the
LGPL `libpulse`, so parser and daemon client share one implementation and
build identically on Linux and FreeBSD. Socket discovery, in order:
`$PULSE_SERVER` (honored only with a `unix:` prefix),
`$XDG_RUNTIME_DIR/pulse/native`, `/var/run/pulse/native`,
`/usr/local/var/run/pulse/native`, `/tmp/pulse-native`. The client speaks
protocol **version 35** (PulseAudio 14+ and pipewire-pulse).

### Packet framing: the 20-byte descriptor

Every packet — control *and* audio — is framed by a fixed 20-byte descriptor,
all integers big-endian:

```text
offset  field    type    meaning
------  -------  ------  ---------------------------------------------
0       length   u32 BE  body length in bytes
4       channel  u32 BE  0xFFFFFFFF = control packet; otherwise the
                         stream's channel index (memblock = audio)
8       offset   u64 BE  seek offset (memblock positioning only)
16      flags    u32 BE  frame flags
```

The **command opcode is not in the descriptor**: the body is a tagstruct
whose first two `u32` fields are the command and the request tag. The
channel field is the control/audio discriminator that trips everyone up:
control packets MUST carry channel `0xFFFFFFFF` — the server treats any
other value as a memblock (audio) frame and silently consumes it, so no
reply ever comes back. Audio flows the opposite way: a memblock's channel
is the *stream index* assigned at `CREATE_PLAYBACK_STREAM` time.

### The tagstruct wire format

A tagstruct is a sequence of typed fields, each prefixed with a one-byte
ASCII tag:

```text
tag  type           payload
---- -------------- ----------------------------------------------
'L'  U32            u32 BE
'B'  U8             u8
'1'  BOOLEAN_TRUE   (none)
'0'  BOOLEAN_FALSE  (none)
't'  STRING         UTF-8 bytes + NUL          -- NO length prefix
'N'  STRING_NULL    (none; ends proplists / absent optionals)
'x'  ARBITRARY      u32 BE length + raw bytes
'R'  U64            u64 BE
'P'  PROPLIST       (STRING key + u32 len + 'x' value)* then 'N'
'a'  SAMPLE_SPEC    rate u32 BE + channels u8 + format u8 (6 bytes)
'm'  CHANNEL_MAP    u8 count + position bytes
'v'  CVOLUME        RAW u8 count + RAW u32 BE volumes
```

Three asymmetries worth memorizing:

- **Strings are NUL-terminated with no length prefix** — readers scan for
  the NUL (exactly what `pa_tagstruct_gets` does).
- **Proplist values are double-encoded**: the value's length appears once
  as the `u32` the reader fetches, then again inside the `'x'` arbitrary
  tag's own length prefix (what `pa_tagstruct_get_proplist` parses).
- **Cvolume fields are raw**: after the `'v'` tag, the channel count is a
  bare `u8` and each volume a bare `u32` (no tags) — inherited from PA's C
  writer. `VOLUME_NORM` = 0x10000 is unity per channel.

### The handshake

1. `AUTH` (opcode 8): `U32` protocol version + optionally an `ARBITRARY`
   256-byte cookie (`$PULSE_COOKIE`, `~/.config/pulse/cookie`, or
   `/usr/local/etc/pulse/cookie`). Without a cookie only cookie-auth-disabled
   servers accept you; refusal arrives as `ERROR`.
2. `SET_CLIENT_NAME` (opcode 9): an `application.*` proplist; the generic
   `REPLY` carries the assigned client index.
3. `QUERY_INFO` (opcode 20): no arguments; the reply carries server info
   (daemon version, default sink/source names).

Replies to `AUTH`/`SET_CLIENT_NAME` use the generic `REPLY` opcode — the
protocol has no per-command success opcodes. Servers also emit *unsolicited*
frames (memfd registration, srbchannel probes, subscription events, and
`REQUEST` flow-control pings carrying tag `0xFFFFFFFF`), so the client reads
frames until its echoed request tag comes back. All I/O is blocking with a
5 s timeout; drive it from a worker thread.

### CREATE_PLAYBACK_STREAM (v35) lifecycle

1. **Create** (opcode 3), field order per `protocol-native.c` v35: sample
   spec (`'a'`), channel map (`'m'`), sink index (`u32::MAX` = default) +
   sink name (null string for default), buffer metrics (`maxlength`,
   `corked=false`, `tlength`, `prebuf`, `minreq` — `u32::MAX` defaults),
   `syncid`, `cvolume` (`VOLUME_NORM` per channel), then the version-gated
   flag blocks: 7 booleans (v>=12), muted/adjust_latency/proplist (v>=13),
   volume_set/early_requests (v>=14), three booleans (v>=15),
   relative_volume (v>=17), passthrough (v>=18), `n_formats = 0` (v>=21).
   The reply's first `u32` is the assigned stream index.
2. **Memblock writes**: each audio chunk is a 20-byte descriptor with
   `channel = <stream index>` (NOT `0xFFFFFFFF`), offset 0, flags 0, and a
   raw interleaved FLOAT32LE payload. Fire-and-forget: the server's
   `REQUEST` flow control is deliberately ignored (overruns drop
   server-side — the documented MVP bridge behavior).
3. **Teardown**: `DELETE_PLAYBACK_STREAM` (opcode 4), a single `u32` stream
   index.

*Implementation: `crates/gw-pulse/src/daemon.rs` (handshake, playback),
`crates/gw-pulse/src/tags.rs` (tagstruct), `crates/gw-pulse/src/codec.rs`
(descriptor, command opcodes).*

## 6. The ALSA PCM plugin ABI

### The libasound dlopen model

An ALSA config names a PCM plugin by *type*; libasound maps the type to a
filename and `dlopen`s it at `snd_pcm_open` time:

```text
pcm.sonicbrew {
    type sonicbrew               # → libasound_module_pcm_sonicbrew.so
    socket "/tmp/sonicbrew.sock" # or: server "10.0.0.4" [port 9001]
    channels 2
    rate 48000
}
```

libasound then resolves `_snd_pcm_sonicbrew_open` (the expansion of
`SND_PCM_PLUGIN_ENTRY(sonicbrew)`) in that `.so` and calls it with the
parsed config subtree, the stream direction, and an out-pointer for the
PCM handle. The whole contract is a C ABI, so `gw-alsa-plugin` carries
`#[repr(C)]` mirrors of the plugin-SDK structs, pinned field-for-field by
the `abi_layout_matches_alsa` offset tests: `snd_pcm_ioplug` is 120 bytes
and its callback struct 160 on LP64, at protocol 1.0.2 (`0x010002`) — a
layout frozen since 2006.

### The version-symbol check

Before trusting a freshly loaded plugin, libasound looks up a *second*
symbol: the expansion of `SND_DLSYM_BUILD_VERSION` (`global.h`), which for
this plugin is `_` + entry-point name + `_dlsym_pcm_001`:

```text
__snd_pcm_sonicbrew_open_dlsym_pcm_001
```

libasound only verifies the symbol **exists** (via `dlsym`) — it is never
called. Without it every `snd_pcm_open` fails with "unable to verify
version for symbol"; the crate therefore exports it as a `#[no_mangle]`
static byte.

### The ioplug callback table

The ioplug SDK is a 20-slot callback struct; the plugin implements exactly
five slots and leaves the other fifteen `None` (asserted by a unit test):

- **start / stop** — no-ops: the wire is armed by an *eager* connect and
  handshake at open time (a down server fails `snd_pcm_open` itself with
  `-EIO`, the pulse/jack plugin convention), and the socket deliberately
  stays open across stop/start cycles.
- **pointer** — the transferred-frame counter modulo the negotiated buffer
  size (`hw_frames % buffer_size`).
- **transfer** — moves `size` frames between ALSA *channel areas* and the
  bridge socket. The per-sample address formula is the generic ioplug one,
  `addr + (first + (offset + frame) * step) / 8` bits — exact for
  interleaved f32, where `first` is the channel slot and `step` the frame
  stride. Playback gathers areas → interleaved LE f32 wire bytes; capture
  scatters the other way.
- **close** — reclaims the whole `Box`'d plugin state from the `io`
  pointer libasound hands back (which is why `io` must remain the struct's
  first field).

### The set_param_list ordering constraint

HW constraints — single-element lists pinning `RW_INTERLEAVED`,
`FLOAT_LE` (format 14), the conf `channels`/`rate` — are registered via
`snd_pcm_ioplug_set_param_list`, but only **after**
`snd_pcm_ioplug_create`: the call stores into constraint lists that
`create` allocates inside the ioplug handle, so calling it first
dereferences unallocated state (a SIGSEGV). Every reference plugin —
pulse, jack — creates first and pins constraints second.

### The verified end-to-end result

On 2026-08-17 the live path was verified on the FreeBSD host with `aplay`:
libasound `dlopen`ed the `.so`, negotiated `FLOAT_LE`, the plugin
completed the bridge TCP handshake (magic `0x53424E52`), and **24,000 f32
frames** arrived server-side.

*Implementation: `crates/gw-alsa-plugin/src/lib.rs` (ABI mirrors,
callbacks, entry point, ordering), `crates/gw-alsa-plugin/src/bridge.rs`
(pure-Rust socket protocol), [PROGRESS.md](./PROGRESS.md) §future-work.*

## 7. Session consensus (Raft)

### Why a topology needs one source of truth

Everything downstream of the control plane — the engine's rebuild factory,
the gateway bridge, the autosave exporter — renders *some* topology
snapshot. Multiple REST clients mutating concurrently, plus restarts
mid-edit, need one component that serializes edits into a single ordered
history. That is the `SessionStore`: every mutation receives a monotonic
`MutationId`, is applied in order, and fans out to consumers as a
`TopologyEvent` on a 64-slot tokio broadcast channel. Raft enters when
that single history must survive the *failure of a node*.

### Leader election

A Raft cluster of 2N+1 voters tolerates N node failures by electing one
node as the serialization point. Each node expects heartbeats at a fixed
interval; when the election timeout passes without one (80–120 ms in the
test config, randomized per node), the node increments its term, declares
itself a candidate, and requests votes. A majority of grants makes it
leader; split votes dissolve because the timeouts are randomized.

### Log replication

Client writes go to the leader, which appends them to its log and streams
`AppendEntries` to followers. An entry is *committed* once a majority has
acknowledged it — and only committed entries reach the state machine, in
log order. `DistributedRaftEngine::apply_mutation` blocks until openraft's
`client_write` returns, yielding the committed `mutation_id` from the
response.

### Snapshots

Replaying an ever-growing log is wasteful for a node that is far behind,
so Raft periodically compacts it: the state machine serializes its current
`TopologySnapshot` plus openraft `SnapshotMeta` (bincode) into a redb
snapshot table (`RaftSnapshotBuilder`). A restarting or lagging node
installs the snapshot and then replays only the log entries after it.

### Single-node RaftEngine vs multi-node DistributedRaftEngine

- **`RaftEngine`** (MVP): one process is its own authoritative log — an
  in-memory `TopologySnapshot` backed by a redb WAL; no voting, no
  network, fully synchronous calls.
- **`DistributedRaftEngine`** (ADR-0003): wraps an
  `Arc<Raft<TypeConfig>>` running on a tokio runtime. openraft is fully
  async while `SessionStore` is sync, so `apply_mutation` bridges by
  *spawning* `client_write` on the runtime `Handle` and blocking on a
  `std::sync::mpsc` receiver — blocking on a std channel (not
  `Handle::block_on`) is safe even when the caller itself sits on the
  runtime, because it never tries to drive the reactor. `get_topology`
  reads the locally applied state directly from the redb database shared
  with the state machine (`StateMachineReader`), skipping a Raft round
  trip.

Single-process deployments construct `RaftEngine`; multi-node replication
constructs `DistributedRaftEngine` — the two are distinct types, so
reverting to single-node operation is simply not constructing the
distributed one. `spawn_cluster` builds an N-node in-process cluster over
a loopback network for the integration suite.

*Implementation: `crates/session-store/src/lib.rs` (trait, `RaftEngine`),
`crates/session-store/src/{distributed.rs, raft_log_store.rs,
raft_state_machine.rs, raft_network.rs, raft_types.rs}`,
[ADR-0003](./adr/0003-openraft-multi-node-consensus.md).*

## 8. Persistence models

### WAL semantics (redb, mutation replay)

The engine's redb database is a single table, `u64 → Vec<u8>`, mapping
mutation id → serialized `Mutation`. redb orders `u64` keys numerically,
so iterating the table *is* append order. Two invariants make it a true
write-ahead log:

- **Persist before apply** — a mutation is committed to redb *before* the
  in-memory snapshot is touched, so a persistence failure never leaves the
  snapshot ahead of the log.
- **Replay on open** — `RaftEngine::open` replays every logged mutation
  into a fresh `TopologySnapshot` and resumes the id counter past the
  replayed count; the next mutation continues the history rather than
  restarting it.

### The sidecar preset (kind/params the store lacks)

Upstream `NodeSnapshot` carries ports but **no `kind`/`params` fields**,
so the controller's typed parameters live only in in-memory side
registries — a restart restores the topology from the WAL but silently
loses them. The `Preset` DTO closes that gap at the API level:
`export_preset` snapshots the entire graph state (nodes with
label/kind/params + links, `PRESET_VERSION = 1`), and `import_preset`
restores it — replace-semantics (delete every existing node, then
re-create the preset's nodes and links) and deliberately non-transactional:
a failure mid-import leaves the partial state in place, with no rollback.

### Change-detect + tmp+rename autosave

The server's `sonicbrew-autosave` thread wakes every 2 s, exports a
preset, and compares the pretty-printed JSON against the last write — an
unchanged graph costs one serialize and zero I/O. A changed graph is
written to `sonicbrew-dev.preset.json.tmp` and then `rename(2)`d onto the
target; rename is atomic within a filesystem, so a crash never leaves a
half-written preset. The policy is latest-wins: an abrupt kill loses at
most 2 s of edits. Boot reverses the flow — `Preset::from_json_file` →
`import_preset` — before the first graph build.

### Why both exist

The WAL is *authoritative and per-mutation*: every accepted mutation is
durable, transactionally, in order. The sidecar is *derived and holistic*:
it captures the kind/params the WAL structurally cannot hold (until
`NodeSnapshot` grows those fields upstream), at the cost of a 2 s window
and a non-transactional restore. Deleting the sidecar loses only typed
parameters; deleting the WAL loses the graph.

*Implementation: `crates/session-store/src/lib.rs` (WAL),
`crates/control-api/src/lib.rs` (`Preset`), `crates/sonicbrew/src/main.rs`
(autosave thread, boot restore).*

## 9. Control-plane protocol design

### The REST resource model

control-api exposes the graph as plain REST over axum/hyper (gRPC deliberately
skipped in the MVP):

```text
GET    /nodes         NodeInfo[]           200
POST   /nodes   body NodeSpec        →    201 + CreateNodeResponse{id}
DELETE /nodes/{id}                        204 | 404 (store cascades links)
POST   /links   body LinkRequest      →   201 + LinkResponse{id}
DELETE /links/{id}                        204 | 404
GET    /preset        Preset               200
POST   /preset   body Preset          →   204 | 4xx
```

`load_module` exists on the trait but returns `Unimplemented` — the MVP is
statically linked; hot plugin loading is future work. New node ids are derived as
`max(existing node ids) + 1` (0 when the graph is empty) — deterministic
and observable through `get_topology`. Labels and kinds, which
`NodeSnapshot` also lacks, ride in the controller's side registries with a
`node-{id}` fallback label.

### Typed params as an externally-tagged enum

`NodeParams` is a 20-variant enum whose wire form is serde's default
*externally tagged* representation — the variant name wraps an inner
object:

```json
{"label":"eq1","kind":"eq","params":{"Eq":{"freq":1000,"gain_db":3,"q":0.707}}}
{"label":"gain","kind":"gain","params":{"Gain":{"gain":0.5}}}
```

Every field carries `#[serde(default = "...")]`, so *partial params* are
the norm, not an error: a client may send only `freq` and the remaining EQ
fields fall back to per-variant defaults, and omitting `params` entirely
is backward-compatible (the factory applies per-kind defaults). The
variant must agree with the sibling `kind` string (checked via
`NodeParams::kind_name`) — `{"kind":"eq","params":{"Gain":…}}` is a 400
rather than a silent factory fallback — and when `kind` is omitted but
`params` present, the kind is inferred from the variant and recorded as if
declared.

### The LinkId positional caveat

`LinkId` is an **index into the topology's edge vector**, not a stable
handle: removing a link shifts the id of every later edge, so a `LinkId`
captured before a deletion may address a different edge afterwards.
Clients must re-fetch the topology after any mutation that changes edges.
Node deletion has the matching property — incident links disappear by
cascade inside `Mutation::RemoveNode` rather than through individual
`RemoveLink` mutations.

*Implementation: `crates/control-api/src/lib.rs` (DTOs,
`GraphController`, `RestApi`, `status_for` mapping).*

## 10. Glossary

- **xrun** — over-run or under-run: a bounded buffer missed its schedule,
  so a consumer read unwritten frames or a producer dropped some. Xruns
  surface at the graph edges (rings, jitter buffers) and are counted by
  `monitor`.
- **tlength** — PulseAudio playback-stream metric: the target total buffer
  length in bytes the client requests (sent as `u32::MAX` = server
  default here).
- **prebuf** — PulseAudio metric: bytes to prebuffer before playback
  starts, so the stream does not underrun immediately (also `u32::MAX`).
- **planar** — sample layout with each channel's samples contiguous; the
  graph's internal convention, converted at every gateway edge.
- **ioplug** — alsa-lib's PCM plugin SDK: a `#[repr(C)]` handle plus a
  20-slot callback table at protocol 1.0.2.
- **memid** — netmap's memory-allocator id assigned per port at
  registration; all ports of one VALE switch must share the first port's
  `nr_arg2`, or their slots do not alias (the zero-switching trap).
- **VALE** — netmap's in-kernel virtual software switch, the zero-copy
  test fabric for the RTP dataplane (`vale1:1`).
- **tagstruct** — PulseAudio's typed wire sequence: a one-byte ASCII tag
  prefixes each field (`L` = u32, `t` = NUL-terminated string, ...).
- **proplist** — PulseAudio key/value metadata; the value length is
  double-encoded (an outer `u32` plus the inner `'x'` length prefix).
- **cvolume** — PulseAudio per-channel volume vector; after the `'v'` tag
  the count and every volume are *raw* (no tags). `VOLUME_NORM` =
  0x10000 = unity.
- **memblock** — PulseAudio's audio-data frame: a 20-byte descriptor whose
  `channel` field is the *stream index* (control frames carry `0xFFFFFFFF`
  instead).
- **jitter** — packet arrival-time variance; absorbed by a reorder buffer
  keyed on the wrap-aware RTP sequence number.
- **SSRC** — RTP synchronization source: the random `u32` identifying one
  stream.
- **WAL** — write-ahead log: append each mutation durably *before*
  applying it in memory, so replay rebuilds the exact pre-crash state.
- **sidecar** — a derived file written alongside the primary store (here:
  the preset JSON) capturing fields the store's schema lacks.
- **round-trip latency** — input-to-output delay through the whole chain;
  accumulates roughly one block (5.33 ms at MVP settings) per buffering
  stage.
- **headroom** — the margin between nominal operating level and full
  scale (0 dBFS); a limiter threshold of -6 dBFS reserves 6 dB of it.
- **DF1T** — Direct Form I Transposed: the two-register biquad evaluation
  order used by `EqNode`.
- **RBJ** — Robert Bristow-Johnson's Audio EQ Cookbook: the standard
  biquad coefficient formulas for the six EQ shapes.
- **S&H** — sample-and-hold: hold each sample for N cycles; the
  bitcrusher's sample-rate-reduction stage.
- **aux send** — a scaled copy of a signal routed to a second bus;
  `AuxSendNode`'s output port 1, while port 0 carries the main path.
- **kind** — the string naming a node's DSP type (`"eq"`, `"gain"`),
  validated against the `NodeParams` variant at create time.
