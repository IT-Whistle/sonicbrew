//! Minimal RT-safe test/source/sink [`AudioNode`]s used by the engine's tests
//! and available to callers that need simple deterministic nodes.
//!
//! All three allocate **only at construction**; `process` is bounded loops with
//! no allocation, locking, or panicking — the same RT contract the engine
//! demands of `process_cycle`.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 0-in / 1-out source that emits a pre-computed mono sine wave each cycle.
///
/// Allocates the waveform once in [`SineSource::new`]; `process` is a single
/// bounded `copy_from_slice`.
pub struct SineSource {
    out_port: [PortDescriptor; 1],
    sine: AudioFrame,
}
impl SineSource {
    /// Build a mono sine of `freq` Hz at amplitude `amp` (0.0..=1.0), sized to
    /// `num_frames` samples at `sample_rate`.
    #[must_use]
    pub fn new(freq: f32, amp: f32, num_frames: usize, sample_rate: u32) -> Self {
        let samples: Vec<f32> = (0..num_frames)
            .map(|i| {
                (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin() * amp
            })
            .collect();
        Self {
            out_port: [PortDescriptor::output(1, SampleFormat::F32)],
            sine: AudioFrame::from_planar(1, sample_rate, samples),
        }
    }
}
impl AudioNode for SineSource {
    fn inputs(&self) -> &[PortDescriptor] {
        &[]
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_port
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        _in_frames: &[AudioFrame],
        out_frames: &mut [AudioFrame],
    ) {
        let Some(out) = out_frames.get_mut(0) else {
            return;
        };
        let n = self.sine.samples.len().min(out.samples.len());
        out.samples[..n].copy_from_slice(&self.sine.samples[..n]);
        out.channels = self.sine.channels;
        out.sample_rate = self.sine.sample_rate;
    }
}

/// 1-in / 1-out linear gain (RT-safe: bounded loop, no alloc).
pub struct Gain {
    gain: f32,
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
}
impl Gain {
    #[must_use]
    pub fn new(gain: f32) -> Self {
        Self {
            gain,
            in_port: [PortDescriptor::input(1, SampleFormat::F32)],
            out_port: [PortDescriptor::output(1, SampleFormat::F32)],
        }
    }
}
impl AudioNode for Gain {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.in_port
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_port
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        in_frames: &[AudioFrame],
        out_frames: &mut [AudioFrame],
    ) {
        let (Some(inp), Some(out)) = (in_frames.first(), out_frames.get_mut(0)) else {
            return;
        };
        let n = inp.samples.len().min(out.samples.len());
        for k in 0..n {
            out.samples[k] = inp.samples[k] * self.gain;
        }
    }
}

/// 1-in / 0-out sink whose `process` is a no-op. Callers/tests read its input
/// scratch via [`Graph::read_input`](audio_graph_bsd::Graph::read_input) after a
/// cycle (same pattern as the sonicbrew binary's `CaptureSinkNode`).
pub struct Capture {
    in_port: [PortDescriptor; 1],
}
impl Capture {
    #[must_use]
    pub fn new() -> Self {
        Self {
            in_port: [PortDescriptor::input(1, SampleFormat::F32)],
        }
    }
}
impl Default for Capture {
    fn default() -> Self {
        Self::new()
    }
}
impl AudioNode for Capture {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.in_port
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &[]
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        _in_frames: &[AudioFrame],
        _out_frames: &mut [AudioFrame],
    ) {
        // Intentionally empty: the upstream output is already copied into this
        // node's input scratch by the graph scheduler; observers read it there.
    }
}
