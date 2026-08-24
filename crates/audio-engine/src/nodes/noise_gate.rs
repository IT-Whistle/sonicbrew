//! Threshold-based noise gate — attenuates signals below a threshold to
//! silence, with attack/hold/release envelope smoothing.
//!
//! All parameters are fixed at construction; the per-channel envelope and
//! hold-counter state is pre-allocated. `process` does bounded sample math
//! with a peak-detection envelope follower and smooth gain ramping — no
//! allocation or locking.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 1-in / 1-out noise gate.
pub struct NoiseGateNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    threshold: f32,    // linear amplitude
    attack_coef: f32,  // gain opening coefficient (per sample)
    release_coef: f32, // gain closing coefficient
    hold_samples: usize,
    /// Per-channel envelope follower state.
    envelope: Vec<f32>,
    /// Per-channel hold counter (samples remaining above-threshold grace).
    hold_counter: Vec<usize>,
    /// Per-channel smoothed gain (0.0 = closed, 1.0 = open).
    gain: Vec<f32>,
}

impl NoiseGateNode {
    /// Create a noise gate.
    ///
    /// - `threshold_db`: level below which the gate closes (dB).
    /// - `attack_ms`: time for the gate to fully open.
    /// - `hold_ms`: minimum time the gate stays open after signal drops.
    /// - `release_ms`: time for the gate to fully close.
    /// - `sample_rate` / `channels`: graph configuration.
    #[must_use]
    pub fn new(
        threshold_db: f32,
        attack_ms: f32,
        hold_ms: f32,
        release_ms: f32,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        let threshold = 10.0_f32.powf(threshold_db / 20.0);
        let attack_coef = time_constant(attack_ms, sample_rate);
        let hold_samples = (hold_ms * 0.001 * sample_rate as f32) as usize;
        let release_coef = time_constant(release_ms, sample_rate);
        let ch = channels as usize;
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            threshold,
            attack_coef,
            release_coef,
            hold_samples,
            envelope: vec![0.0; ch],
            hold_counter: vec![0; ch],
            gain: vec![0.0; ch],
        }
    }

    /// Process one sample for channel `ch`, updating envelope + gate gain.
    #[inline]
    fn process_sample(&mut self, x: f32, ch: usize) -> f32 {
        let abs_x = x.abs();
        let env = &mut self.envelope[ch];
        // Peak-detection envelope: fast attack, slow release.
        let coef = if abs_x > *env {
            self.attack_coef
        } else {
            self.release_coef
        };
        *env = *env + coef * (abs_x - *env);

        // Gate logic: above threshold → open + refresh hold; below → consume hold.
        let target = if *env > self.threshold {
            self.hold_counter[ch] = self.hold_samples;
            1.0
        } else if self.hold_counter[ch] > 0 {
            self.hold_counter[ch] -= 1;
            1.0
        } else {
            0.0
        };

        // Smooth the gain toward target: attack when opening, release when closing.
        let g = &mut self.gain[ch];
        let g_coef = if target > *g {
            self.attack_coef
        } else {
            self.release_coef
        };
        *g = *g + g_coef * (target - *g);

        x * *g
    }
}

/// Convert a time constant in ms to a per-sample one-pole coefficient.
fn time_constant(ms: f32, sample_rate: u32) -> f32 {
    if ms <= 0.0 {
        return 1.0;
    }
    let samples = ms * 0.001 * sample_rate as f32;
    1.0 - (-1.0 / samples).exp()
}

impl AudioNode for NoiseGateNode {
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
        for c in 0..ch {
            let offset = c * per_ch;
            for i in 0..per_ch {
                let idx = offset + i;
                out.samples[idx] = self.process_sample(inp.samples[idx], c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_does_not_panic() {
        let _ = NoiseGateNode::new(-30.0, 1.0, 10.0, 50.0, 48_000, 2);
    }

    #[test]
    fn below_threshold_gates_signal() {
        // threshold -20 dB ≈ 0.1 linear; input 0.01 is well below.
        let mut gate = NoiseGateNode::new(-20.0, 1.0, 10.0, 50.0, 48_000, 1);
        let inp = vec![AudioFrame::from_planar(1, 48_000, vec![0.01; 512])];
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; 512])];
        let mut ctx = ProcessContext::new(512, 0, 48_000);
        gate.process(&mut ctx, &inp, &mut out);
        let last = out[0].samples[511].abs();
        assert!(
            last < 0.01,
            "below-threshold output {last} should be attenuated below input 0.01"
        );
    }

    #[test]
    fn above_threshold_passes_signal() {
        // threshold -40 dB ≈ 0.01 linear; input 0.5 is well above.
        let mut gate = NoiseGateNode::new(-40.0, 1.0, 10.0, 50.0, 48_000, 1);
        let inp = vec![AudioFrame::from_planar(1, 48_000, vec![0.5; 512])];
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; 512])];
        let mut ctx = ProcessContext::new(512, 0, 48_000);
        gate.process(&mut ctx, &inp, &mut out);
        let last = out[0].samples[511].abs();
        assert!(
            (last - 0.5).abs() < 0.05,
            "above-threshold output {last} should ≈ input 0.5"
        );
    }
}
