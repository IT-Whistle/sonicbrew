//! N-input → 1-output mixing bus with per-input gain.
//!
//! The core of any audio server: sum multiple sources into one bus, each
//! scaled by an independent gain. All gains and the input-port table are
//! pre-allocated at construction; `process` is a bounded double loop.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// A mixing bus node: sums `N` mono/stereo inputs into one output, each scaled
/// by a fixed per-input gain set at construction.
pub struct MixerNode {
    in_ports: Vec<PortDescriptor>,
    out_port: [PortDescriptor; 1],
    /// Per-input linear gain (same length as `in_ports`).
    gains: Vec<f32>,
}

impl MixerNode {
    /// Create an `n`-input mixer with the given per-input gains.
    ///
    /// `gains.len()` must equal `n`; surplus/missing entries are clamped.
    /// `channels` sets the port channel count (1 = mono, 2 = stereo).
    #[must_use]
    pub fn new(n: usize, gains: Vec<f32>, channels: u16) -> Self {
        let in_ports = (0..n)
            .map(|_| PortDescriptor::input(channels, SampleFormat::F32))
            .collect();
        let mut g = gains;
        g.resize(n, 0.0);
        Self {
            in_ports,
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            gains: g,
        }
    }

    /// Adjust a single input's gain (control thread, NOT RT — mutates a Vec
    /// element; safe because the graph scheduler never calls this during
    /// `process`).
    pub fn set_gain(&mut self, input: usize, gain: f32) {
        if let Some(g) = self.gains.get_mut(input) {
            *g = gain;
        }
    }
}

impl AudioNode for MixerNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.in_ports
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
        let Some(out) = out_frames.get_mut(0) else {
            return;
        };
        // Zero the output bus first.
        for s in &mut out.samples {
            *s = 0.0;
        }
        // Sum each scaled input.
        for (i, frame) in in_frames.iter().enumerate() {
            let g = self.gains.get(i).copied().unwrap_or(0.0);
            let n = frame.samples.len().min(out.samples.len());
            for k in 0..n {
                out.samples[k] += frame.samples[k] * g;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixes_two_inputs_with_gain() {
        let mut mixer = MixerNode::new(2, vec![0.5, 0.25], 1);
        let inp = vec![
            AudioFrame::from_planar(1, 48000, vec![0.8; 4]),
            AudioFrame::from_planar(1, 48000, vec![0.4; 4]),
        ];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 4])];
        let mut ctx = ProcessContext::new(4, 0, 48000);
        mixer.process(&mut ctx, &inp, &mut out);
        // 0.8*0.5 + 0.4*0.25 = 0.4 + 0.1 = 0.5
        for s in &out[0].samples {
            assert!((s - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn zero_gain_mutes_input() {
        let mut mixer = MixerNode::new(1, vec![0.0], 1);
        let inp = vec![AudioFrame::from_planar(1, 48000, vec![1.0; 4])];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 4])];
        let mut ctx = ProcessContext::new(4, 0, 48000);
        mixer.process(&mut ctx, &inp, &mut out);
        for s in &out[0].samples {
            assert!(s.abs() < 1e-9);
        }
    }
}
