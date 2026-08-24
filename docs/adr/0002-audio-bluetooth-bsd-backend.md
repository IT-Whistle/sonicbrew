# ADR 0002 — audio-bluetooth-bsd: optional Bluetooth A2DP input backend

- **Status:** Accepted (note)
- **Date:** 2026-08-01
- **Author:** sonicbrew contributors
- **Related:** audio-toolkit [ADR 0009](../../../docs/adr/0009-input-bt-interface-contract.md) (BT crate internal input-path decision)

## Context

`audio-bluetooth-bsd` 0.1.0 is published to crates.io as the **10th** audio-toolkit
crate. It is a **Bluetooth A2DP input backend** that implements `audio-io-bsd`'s
`AudioBackend` trait: it spawns `virtual_bt_speaker` writing to a virtual OSS device
`/dev/dspBT`, and a worker reads that OSS device (raw s16), converts s16→f32,
deinterleaves to planar, resamples 44.1 kHz→48 kHz, and pushes `AudioFrame`s into an
rtrb ring (`BtInputSource`). The OSS/daemon FFI is gated behind
`cfg(target_os = "freebsd")` so the pure logic compiles and tests on the Linux dev host.

Until now `sonicbrew`'s documentation and dependency roster described audio-toolkit as
**9 crates** (`audio-core-bsd` + the processing crates + `audio-plugin-bsd` +
`audio-clock-bsd`)) and made **no mention** of `audio-bluetooth-bsd`. This was a
documentation gap, not a design gap: `audio-bluetooth-bsd` is **not a sonicbrew module**
(the sonicbrew roster has no bluetooth slot). Bluetooth audio enters sonicbrew exactly like
any other audio device — through the backend abstraction (`audio-io-bsd`), not as
a gateway.

Two related facts necessitate recording the decision explicitly:

1. A *browser* transport variant named `BrowserTransport::WebRTC` exists in `gw-browser`
   (the browser gateway). The name resemblance invites confusion with "bluetooth audio", but the two are
   **unrelated** — WebRTC is a browser send/receive transport stub (future, after the
   WebSocket MVP). This ADR documents the distinction so future contributors do not
   conflate them.
2. `sonicbrew` consumes
   audio-toolkit from crates.io. `audio-bluetooth-bsd` 0.1.0 is therefore *available*
   without any `../audio-toolkit/` sibling checkout, but it is **not currently a
   `sonicbrew` workspace dependency** — it becomes relevant only when sonicbrew actively
   surfaces a Bluetooth capture device.

## Decision

1. **Acknowledge `audio-bluetooth-bsd`** (0.1.0) as an **available optional** audio-toolkit
   backend, consumed through the **`audio-io-bsd` `AudioBackend` abstraction**. It is
   **not** a sonicbrew module (M0x) and is **not** registered as a `gw-*` gateway node —
   it is a kernel-level input source, identical in role to a generic OSS/cpal capture device.
2. **Documentation corrected** (README/ARCHITECTURE/CONTRIBUTING): the audio-toolkit roster
   is updated from 9 → 10 crates, and the backend entry path is recorded.
3. **`BrowserTransport::WebRTC` is browser-only** and distinct from bluetooth input;
   the docs now state this explicitly.
4. **Feature-gated integration:** when sonicbrew later wires Bluetooth capture in, the
   dependency and code path will live behind a **`bluetooth`** feature (default off) on the
   consuming crate, mirroring how `audio-io-bsd` keeps `cpal` behind `cpal-backend` and
   `audio-opus-bsd` behind `opus`. This keeps the FreeBSD `cargo check` gate
   free of the BT daemon/OSS runtime FFI unless explicitly opted in.

## Consequences

- **Positive:** sonicbrew docs now accurately reflect the published audio-toolkit crate
  set; no contributor can mistake WebRTC for bluetooth, or assume a phantom bluetooth
  module. The backend is ready to consume via the existing `AudioBackend` seam with no
  contract change.
- **Negative:** version drift between `sonicbrew`'s (future) pin and new
  `audio-bluetooth-bsd` releases must be tracked manually — the same trade-off accepted in
  crates.io pins for the other 9 crates (see root `Cargo.toml`).
- **Constraint:** the BT crate's OSS/`virtual_bt_speaker` FFI is `cfg(target_os =
  "freebsd")`-gated. On the Linux dev host only the pure-logic path (s16→f32, deinterleave,
  resample, ring) is exercised; real A2DP capture requires a FreeBSD runtime. Integration
  behind the `bluetooth` feature preserves this boundary.
- **No `AudioNode`/trait contract change:** this is a documentation + availability note.
  Consuming `audio-bluetooth-bsd` through `AudioBackend` requires no ADR-gated contract
  change under GOVERNANCE §2.2 (the seam already exists).

## Reference

- **audio-toolkit [ADR 0009](../../../docs/adr/0009-input-bt-interface-contract.md)** — the
  `audio-bluetooth-bsd` *internal* input-path decision (option A: direct raw OSS read via
  `read_oss_s16`, single `BtInputSource` ring, s16→f32 via `audio_io_bsd::sample_conv`,
  44.1→48 kHz resample owned by the BT worker). That ADR governs the BT crate's *own*
  architecture; this ADR governs sonicbrew's *consumption* posture.
