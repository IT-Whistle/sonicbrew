//! Tremolo — LFO amplitude modulation.
//!
//! The input signal's gain is swept by a sine LFO, producing the classic
//! guitar "volume wobble". A single shared LFO drives all channels so stereo
//! stays in phase. `process` performs only bounded sample math with no
//! allocation, locking, or panicking.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 2π — full LFO cycle.
const TWO_PI: f32 = std::f32::consts::TAU;

/// 1-in / 1-out LFO amplitude modulation (tremolo).
pub struct TremoloNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    /// Modulation depth (0.0 = no wobble, 1.0 = full cut to silence).
    depth: f32,
    /// Per-sample LFO phase increment (2π · rate / sample_rate).
    phase_inc: f32,
    /// Shared LFO phase (0..2π) — advances once per frame.
    lfo_phase: f32,
}

impl TremoloNode {
    /// Create a tremolo node.
    ///
    /// - `rate_hz`: LFO frequency (clamped to 0.1..20.0).
    /// - `depth`: modulation depth (clamped to 0.0..=1.0).
    /// - `sample_rate` / `channels`: graph configuration.
    #[must_use]
    pub fn new(rate_hz: f32, depth: f32, sample_rate: u32, channels: u16) -> Self {
        let rate = rate_hz.clamp(0.1, 20.0);
        let dep = depth.clamp(0.0, 1.0);
        let sr = sample_rate as f32;
        let phase_inc = TWO_PI * rate / sr;
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            depth: dep,
            phase_inc,
            lfo_phase: 0.0,
        }
    }
}

impl AudioNode for TremoloNode {
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
        let ch = inp.channels as usize;
        let n = inp.samples.len().min(out.samples.len());
        if ch == 0 || n == 0 {
            return;
        }
        let per_ch = n / ch;
        // Sample-outer / channel-inner: a single shared LFO computes one gain
        // per frame and applies it to every channel, then advances once. This
        // keeps stereo channels phase-aligned (the LFO must not advance per
        // channel).
        for i in 0..per_ch {
            let gain = (1.0 - self.depth) + self.depth * (0.5 + 0.5 * self.lfo_phase.sin());
            for c in 0..ch {
                let idx = c * per_ch + i;
                out.samples[idx] = inp.samples[idx] * gain;
            }
            self.lfo_phase += self.phase_inc;
            if self.lfo_phase >= TWO_PI {
                self.lfo_phase -= TWO_PI;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_depth_passthrough() {
        // depth=0.0 → gain=1.0 always → output equals input exactly.
        let mut node = TremoloNode::new(5.0, 0.0, 48_000, 1);
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.01).sin()).collect();
        let inp = vec![AudioFrame::from_planar(1, 48_000, input.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48_000);
        node.process(&mut ctx, &inp, &mut out);
        for (got, want) in out[0].samples.iter().zip(input.iter()) {
            assert!(
                (got - want).abs() < 1e-6,
                "zero-depth passthrough mismatch: {got} vs {want}"
            );
        }
    }

    #[test]
    fn full_depth_modulates() {
        // depth=1.0, rate=1Hz, DC input → gain starts at 0.5 (sin(0)=0) and
        // rises. After 256 samples some must be strictly below the input,
        // proving amplitude reduction.
        let mut node = TremoloNode::new(1.0, 1.0, 48_000, 1);
        let input: Vec<f32> = vec![1.0; 256];
        let inp = vec![AudioFrame::from_planar(1, 48_000, input.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48_000);
        node.process(&mut ctx, &inp, &mut out);
        let reduced = out[0].samples.iter().filter(|&&s| s < 1.0).count();
        assert!(
            reduced > 0,
            "full-depth tremolo must reduce some samples below input"
        );
        // First sample: gain = 0.5 + 0.5*sin(0) = 0.5 → output 0.5.
        assert!(
            (out[0].samples[0] - 0.5).abs() < 1e-6,
            "first sample at depth=1, phase=0 should be 0.5, got {}",
            out[0].samples[0]
        );
    }

    #[test]
    fn no_nan_high_rate() {
        // rate=20Hz (max), 10000 samples of a loud sine → all finite & bounded.
        let sr = 48_000u32;
        let n = 10_000usize;
        let mut node = TremoloNode::new(20.0, 0.8, sr, 1);
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (std::f32::consts::TAU * 440.0 * t).sin() * 0.9
            })
            .collect();
        let inp = vec![AudioFrame::from_planar(1, sr, input)];
        let mut out = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, sr);
        node.process(&mut ctx, &inp, &mut out);
        for &s in &out[0].samples {
            assert!(s.is_finite(), "output must be finite, got {s}");
        }
        let peak = out[0].samples.iter().fold(0.0_f32, |a, &s| a.max(s.abs()));
        assert!(peak <= 1.0, "output should stay bounded, peak={peak}");
    }

    #[test]
    fn rate_affects_period() {
        // rate=1Hz vs rate=10Hz over 256 samples → different modulation
        // patterns (10Hz advances the phase 10× faster).
        let sr = 48_000u32;
        let n = 256usize;
        let input: Vec<f32> = (0..n).map(|i| (i as f32 * 0.05).sin()).collect();
        let inp = vec![AudioFrame::from_planar(1, sr, input.clone())];

        let mut slow = TremoloNode::new(1.0, 0.7, sr, 1);
        let mut fast = TremoloNode::new(10.0, 0.7, sr, 1);
        let mut out_s = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
        let mut out_f = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, sr);
        slow.process(&mut ctx, &inp, &mut out_s);
        fast.process(&mut ctx, &inp, &mut out_f);

        let diff_count = out_s[0]
            .samples
            .iter()
            .zip(out_f[0].samples.iter())
            .filter(|(&a, &b)| (a - b).abs() > 1e-5)
            .count();
        assert!(
            diff_count > 0,
            "different LFO rates must yield different outputs"
        );
    }
}
