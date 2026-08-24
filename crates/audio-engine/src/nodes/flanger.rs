//! Flanger — LFO-modulated delay with feedback, producing the classic
//! sweeping "comb filter" / "jet plane" effect.
//!
//! Each channel owns an independent ring buffer and LFO phase. The delay time
//! is swept by a sine LFO and read via fractional (linear-interpolated)
//! addressing. A portion of the delayed output is fed back into the input,
//! sharpening the comb peaks. `process` performs only bounded sample math with
//! no allocation, locking, or panicking.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 2π — full LFO cycle.
const TWO_PI: f32 = std::f32::consts::TAU;

/// Maximum feedback factor — kept strictly below 1.0 so the feedback loop
/// cannot diverge even with sustained input.
const FEEDBACK_CEIL: f32 = 0.9;

/// 1-in / 1-out LFO-modulated delay with feedback (flanger).
pub struct FlangerNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    /// Feedback gain applied to the delayed sample when writing.
    feedback: f32,
    /// Wet/dry crossfade (0.0 = dry only, 1.0 = wet only).
    mix: f32,
    /// Centre delay in samples (LFO sweeps around this).
    center_samples: f32,
    /// LFO modulation depth in samples.
    depth_samples: f32,
    /// Per-sample LFO phase increment (2π · rate / sample_rate).
    phase_inc: f32,
    /// Per-channel ring buffers.
    buffers: Vec<Vec<f32>>,
    /// Per-channel write cursor.
    write_pos: Vec<usize>,
    /// Per-channel LFO phase (0..2π).
    lfo_phase: Vec<f32>,
}

impl FlangerNode {
    /// Create a flanger node.
    ///
    /// - `rate_hz`: LFO frequency (clamped to 0.05..10.0).
    /// - `depth_ms`: modulation depth in ms.
    /// - `center_delay_ms`: centre delay in ms (clamped to > `depth_ms`, min 1.0).
    /// - `feedback`: delayed-sample feedback gain (clamped to `0.0..=FEEDBACK_CEIL`).
    /// - `mix`: wet/dry crossfade (clamped to 0.0..1.0).
    /// - `sample_rate` / `channels`: graph configuration.
    #[must_use]
    pub fn new(
        rate_hz: f32,
        depth_ms: f32,
        center_delay_ms: f32,
        feedback: f32,
        mix: f32,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        let rate = rate_hz.clamp(0.05, 10.0);
        let depth = depth_ms.max(0.0);
        // Flanger delays are very short (1-5ms). Ensure the delay is always
        // strictly positive: center > depth.
        let center = center_delay_ms.max(1.0).max(depth + 1.0);
        let fb = feedback.clamp(0.0, FEEDBACK_CEIL);
        let mx = mix.clamp(0.0, 1.0);
        let sr = sample_rate as f32;
        let center_samples = center * 0.001 * sr;
        let depth_samples = depth * 0.001 * sr;
        // Buffer must hold the maximum possible delay (center + depth), plus a
        // one-sample margin so the interpolation neighbour never aliases.
        let buffer_len = (((center + depth) * 0.001 * sr) + 1.0) as usize;
        let buffer_len = buffer_len.max(4);
        let ch = channels.max(1) as usize;
        let phase_inc = TWO_PI * rate / sr;
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            feedback: fb,
            mix: mx,
            center_samples,
            depth_samples,
            phase_inc,
            buffers: vec![vec![0.0; buffer_len]; ch],
            write_pos: vec![0; ch],
            lfo_phase: vec![0.0; ch],
        }
    }

    /// Process one sample for channel `ch`.
    #[inline]
    fn process_sample(&mut self, x: f32, ch: usize) -> f32 {
        let buf = &mut self.buffers[ch];
        let cap = buf.len();
        let wp = self.write_pos[ch];

        // Modulated delay: centre ± depth, swept by sine LFO.
        let lfo = self.lfo_phase[ch].sin();
        let current_delay = self.center_samples + self.depth_samples * lfo;

        // Fractional read position behind the write cursor, wrapped to [0, cap).
        let read_pos_f = (wp as f32 - current_delay).rem_euclid(cap as f32);
        let idx0 = read_pos_f as usize;
        let idx1 = (idx0 + 1) % cap;
        let frac = read_pos_f - idx0 as f32;
        let wet = buf[idx0] * (1.0 - frac) + buf[idx1] * frac;

        // Feedback write: dry input + delayed output × feedback.
        buf[wp] = x + wet * self.feedback;

        // Advance write cursor and LFO phase.
        self.write_pos[ch] = (wp + 1) % cap;
        let phase = &mut self.lfo_phase[ch];
        *phase += self.phase_inc;
        if *phase >= TWO_PI {
            *phase -= TWO_PI;
        }

        x * (1.0 - self.mix) + wet * self.mix
    }
}

impl AudioNode for FlangerNode {
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
    fn dry_passthrough() {
        // mix=0.0 → output is the dry input untouched (feedback still circulates
        // inside the buffer but never reaches the output path).
        let mut flanger = FlangerNode::new(1.0, 2.0, 5.0, 0.5, 0.0, 48_000, 1);
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.01).sin()).collect();
        let inp = vec![AudioFrame::from_planar(1, 48_000, input.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48_000);
        flanger.process(&mut ctx, &inp, &mut out);
        for (got, want) in out[0].samples.iter().zip(input.iter()) {
            assert!(
                (got - want).abs() < 1e-6,
                "dry passthrough mismatch: {got} vs {want}"
            );
        }
    }

    #[test]
    fn feedback_affects_output() {
        // feedback=0.5 vs feedback=0.0 → different outputs on a 440Hz sine.
        // The buffer must process well beyond one delay period so the feedback
        // loop closes and recirculates — a window shorter than the centre delay
        // sees the delayed read only hitting zero-initialised buffer slots, so
        // feedback never enters the output path.
        let sr = 48_000u32;
        let n = 4096usize;
        let input: Vec<f32> = (0..n)
            .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin() * 0.5)
            .collect();
        let run = |fb: f32| -> Vec<f32> {
            let mut fl = FlangerNode::new(1.0, 1.0, 3.0, fb, 0.5, sr, 1);
            let inp = vec![AudioFrame::from_planar(1, sr, input.clone())];
            let mut out = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
            let mut ctx = ProcessContext::new(n, 0, sr);
            fl.process(&mut ctx, &inp, &mut out);
            out[0].samples.clone()
        };
        let no_fb = run(0.0);
        let with_fb = run(0.5);
        let diff: f32 = no_fb
            .iter()
            .zip(with_fb.iter())
            .map(|(&a, &b)| (a - b).abs())
            .sum();
        assert!(diff > 1.0, "feedback should change output, diff={diff}");
    }

    #[test]
    fn no_nan_with_feedback() {
        // feedback=0.9, rate=5Hz, 10k samples → no NaN/Inf.
        let sr = 48_000u32;
        let n = 10_000usize;
        let mut fl = FlangerNode::new(5.0, 2.0, 5.0, 0.9, 0.5, sr, 1);
        let input: Vec<f32> = (0..n)
            .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin() * 0.8)
            .collect();
        let inp = vec![AudioFrame::from_planar(1, sr, input)];
        let mut out = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, sr);
        fl.process(&mut ctx, &inp, &mut out);
        for &s in &out[0].samples {
            assert!(s.is_finite(), "output must be finite, got {s}");
        }
    }

    #[test]
    fn low_feedback_stable() {
        // feedback=0.3, 5k samples → finite (no divergence).
        let sr = 48_000u32;
        let n = 5_000usize;
        let mut fl = FlangerNode::new(0.5, 2.0, 5.0, 0.3, 0.5, sr, 1);
        let input: Vec<f32> = (0..n)
            .map(|i| (std::f32::consts::TAU * 220.0 * i as f32 / sr as f32).sin() * 0.8)
            .collect();
        let inp = vec![AudioFrame::from_planar(1, sr, input)];
        let mut out = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, sr);
        fl.process(&mut ctx, &inp, &mut out);
        for &s in &out[0].samples {
            assert!(s.is_finite(), "output must be finite, got {s}");
        }
    }
}
