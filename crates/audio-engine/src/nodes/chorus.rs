//! Chorus — LFO-modulated delay creating pitch-modulated doubling/chorusing.
//!
//! Each channel owns an independent ring buffer and LFO phase. The delay time
//! is swept by a sine LFO, and the delayed signal is read via fractional
//! (linear-interpolated) addressing. `process` performs only bounded sample
//! math with no allocation, locking, or panicking.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 2π — full LFO cycle.
const TWO_PI: f32 = std::f32::consts::TAU;

/// 1-in / 1-out LFO-modulated delay chorus.
pub struct ChorusNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
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

impl ChorusNode {
    /// Create a chorus node.
    ///
    /// - `rate_hz`: LFO frequency (clamped to 0.05..10.0).
    /// - `depth_ms`: modulation depth in ms.
    /// - `center_delay_ms`: centre delay in ms (clamped to > `depth_ms`, min 5.0).
    /// - `mix`: wet/dry crossfade (clamped to 0.0..1.0).
    /// - `sample_rate` / `channels`: graph configuration.
    #[must_use]
    pub fn new(
        rate_hz: f32,
        depth_ms: f32,
        center_delay_ms: f32,
        mix: f32,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        let rate = rate_hz.clamp(0.05, 10.0);
        let depth = depth_ms.max(0.0);
        // Ensure the delay is always strictly positive: center > depth.
        let center = center_delay_ms.max(5.0).max(depth + 1.0);
        let mx = mix.clamp(0.0, 1.0);
        let sr = sample_rate as f32;
        let center_samples = center * 0.001 * sr;
        let depth_samples = depth * 0.001 * sr;
        // Buffer must hold at least the maximum possible delay (center + depth),
        // plus a small margin so the interpolation neighbour never aliases.
        let buffer_len = ((center + depth + 1.0) * 0.001 * sr).round() as usize;
        let buffer_len = buffer_len.max(2);
        let ch = channels.max(1) as usize;
        let phase_inc = TWO_PI * rate / sr;
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
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

        // Write input to ring buffer.
        buf[wp] = x;

        // Modulated delay: centre ± depth, swept by sine LFO.
        let lfo = self.lfo_phase[ch].sin();
        let current_delay = self.center_samples + self.depth_samples * lfo;

        // Fractional read position behind the write cursor, wrapped to [0, cap).
        let read_pos_f = (wp as f32 - current_delay).rem_euclid(cap as f32);
        let idx0 = read_pos_f as usize;
        let idx1 = (idx0 + 1) % cap;
        let frac = read_pos_f - idx0 as f32;
        let wet = buf[idx0] * (1.0 - frac) + buf[idx1] * frac;

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

impl AudioNode for ChorusNode {
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
        // mix=0.0 → output is the dry input untouched.
        let mut chorus = ChorusNode::new(1.0, 5.0, 20.0, 0.0, 48_000, 1);
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.01).sin()).collect();
        let inp = vec![AudioFrame::from_planar(1, 48_000, input.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48_000);
        chorus.process(&mut ctx, &inp, &mut out);
        for (got, want) in out[0].samples.iter().zip(input.iter()) {
            assert!(
                (got - want).abs() < 1e-6,
                "dry passthrough mismatch: {got} vs {want}"
            );
        }
    }

    #[test]
    fn wet_modulates_amplitude() {
        // mix=1.0, rate=1Hz → after processing a varying signal, the output
        // differs from the input due to the modulated delay.
        let sr = 48_000u32;
        let n = sr as usize; // 1 second
        let mut chorus = ChorusNode::new(1.0, 5.0, 20.0, 1.0, sr, 1);
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (std::f32::consts::TAU * 440.0 * t).sin() * 0.5
            })
            .collect();
        let inp = vec![AudioFrame::from_planar(1, sr, input.clone())];
        let mut out = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, sr);
        chorus.process(&mut ctx, &inp, &mut out);

        // Sum of absolute differences between output and input — the modulated
        // delay must produce a measurable deviation.
        let diff: f32 = out[0]
            .samples
            .iter()
            .zip(input.iter())
            .map(|(&o, &i)| (o - i).abs())
            .sum();
        assert!(
            diff > 10.0,
            "output should differ from input due to chorus modulation, diff={diff}"
        );
    }

    #[test]
    fn stereo_independent() {
        // 2 channels: ch0 gets a sine, ch1 gets silence. The chorus should
        // produce output on ch0 but ~silence on ch1, proving per-channel
        // isolation.
        let sr = 48_000u32;
        let n = 2048usize;
        let mut chorus = ChorusNode::new(0.5, 5.0, 20.0, 1.0, sr, 2);
        let per_ch = n / 2;
        let mut input = vec![0.0_f32; n];
        for (i, slot) in input[..per_ch].iter_mut().enumerate() {
            let t = i as f32 / sr as f32;
            *slot = (std::f32::consts::TAU * 440.0 * t).sin() * 0.5;
            // ch1 (input[per_ch..]) stays 0.0 — silence.
        }
        let inp = vec![AudioFrame::from_planar(2, sr, input)];
        let mut out = vec![AudioFrame::from_planar(2, sr, vec![0.0; n])];
        let mut ctx = ProcessContext::new(per_ch, 0, sr);
        chorus.process(&mut ctx, &inp, &mut out);

        // ch1 (silence in) must remain near-silence out.
        let ch1_peak = out[0].samples[per_ch..]
            .iter()
            .fold(0.0_f32, |a, &s| a.max(s.abs()));
        assert!(
            ch1_peak < 1e-6,
            "silent channel should stay silent: {ch1_peak}"
        );
        // ch0 (sine in) must produce non-trivial output.
        let ch0_peak = out[0].samples[..per_ch]
            .iter()
            .fold(0.0_f32, |a, &s| a.max(s.abs()));
        assert!(
            ch0_peak > 0.01,
            "active channel should produce output: {ch0_peak}"
        );
    }

    #[test]
    fn no_nan_overflow() {
        // High rate (5Hz) and long processing (50k samples) → no NaN/Inf.
        let sr = 48_000u32;
        let n = 50_000usize;
        let mut chorus = ChorusNode::new(5.0, 10.0, 25.0, 1.0, sr, 1);
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (std::f32::consts::TAU * 220.0 * t).sin() * 0.8
            })
            .collect();
        let inp = vec![AudioFrame::from_planar(1, sr, input)];
        let mut out = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, sr);
        chorus.process(&mut ctx, &inp, &mut out);

        for &s in &out[0].samples {
            assert!(s.is_finite(), "output must be finite, got {s}");
        }
        let peak = out[0].samples.iter().fold(0.0_f32, |a, &s| a.max(s.abs()));
        assert!(peak <= 2.0, "output should stay bounded, peak={peak}");
    }
}
