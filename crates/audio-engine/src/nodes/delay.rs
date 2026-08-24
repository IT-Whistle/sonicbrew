//! Digital delay line — a classic feedback delay (echo) effect with
//! wet/dry mix.
//!
//! Each channel owns an independent ring buffer pre-allocated at construction.
//! `process` performs bounded per-sample reads/writes into the ring with no
//! allocation, locking, or panicking. Feedback is clamped below unity at
//! construction so the line cannot diverge.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// Maximum feedback factor — kept strictly below 1.0 so a sustained input
/// cannot make the delay line diverge to infinity.
const FEEDBACK_CEIL: f32 = 0.99;

/// 1-in / 1-out digital delay line with feedback and wet/dry mix.
pub struct DelayNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    /// Per-channel ring buffers, each `max_delay_samples` long.
    buffer: Vec<Vec<f32>>,
    /// Per-channel write cursor into `buffer` (0..max_delay_samples).
    write_pos: Vec<usize>,
    /// Read offset in samples behind the write cursor (1..max_delay_samples).
    delay_samples: usize,
    /// Feedback gain applied to the delayed sample when writing.
    feedback: f32,
    /// Wet/dry crossfade (0.0 = dry only, 1.0 = wet only).
    mix: f32,
}

impl DelayNode {
    /// Create a delay node.
    ///
    /// - `max_delay_samples`: capacity of the ring buffer per channel.
    /// - `delay_samples`: initial read offset (clamped to `1..=max_delay_samples`).
    /// - `feedback`: delayed-sample feedback gain (clamped to `0.0..=FEEDBACK_CEIL`).
    /// - `mix`: wet/dry crossfade (clamped to `0.0..=1.0`).
    /// - `channels`: channel count — drives buffer allocation.
    #[must_use]
    pub fn new(
        max_delay_samples: usize,
        delay_samples: usize,
        feedback: f32,
        mix: f32,
        channels: u16,
    ) -> Self {
        let cap = max_delay_samples.max(1);
        let delay = delay_samples.clamp(1, cap);
        let fb = feedback.clamp(0.0, FEEDBACK_CEIL);
        let mx = mix.clamp(0.0, 1.0);
        let ch = channels.max(1) as usize;
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            buffer: vec![vec![0.0; cap]; ch],
            write_pos: vec![0; ch],
            delay_samples: delay,
            feedback: fb,
            mix: mx,
        }
    }

    /// Process one sample for channel `ch`.
    #[inline]
    fn process_sample(&mut self, x: f32, ch: usize) -> f32 {
        let cap = self.buffer[ch].len();
        let wp = self.write_pos[ch];
        // The delayed sample sits `delay_samples` behind the write cursor.
        let read_pos = (wp + cap - self.delay_samples) % cap;
        let delayed = self.buffer[ch][read_pos];

        // Feedback write: dry signal + delayed feedback. The written value is
        // also the wet path — this matches the classic echo convention where
        // the first delayed tap reflects the feedback gain (1.0 → fb → fb² …).
        let wet = x + delayed * self.feedback;
        self.buffer[ch][wp] = wet;

        let dry = x * (1.0 - self.mix);
        self.write_pos[ch] = (wp + 1) % cap;
        dry + wet * self.mix
    }
}

impl AudioNode for DelayNode {
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
    fn delay_produces_echo() {
        // delay=4, feedback=0.5, mix=1.0 (wet only). Impulse at t=0 should
        // produce echoes at t=4 (0.5), t=8 (0.25), ...
        let mut delay = DelayNode::new(16, 4, 0.5, 1.0, 1);
        let mut input = vec![0.0_f32; 16];
        input[0] = 1.0;
        let inp = vec![AudioFrame::from_planar(1, 48_000, input.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; 16])];
        let mut ctx = ProcessContext::new(16, 0, 48_000);
        delay.process(&mut ctx, &inp, &mut out);

        let s = &out[0].samples;
        // First echo at index 4: wet = x(0) + delayed(1.0) * feedback(0.5) = 0.5,
        // output = wet * mix(1.0) = 0.5.
        assert!((s[4] - 0.5).abs() < 1e-6, "first echo at [4]: {}", s[4]);
        // Second echo at index 8: delayed is now 0.5, wet = 0 + 0.5*0.5 = 0.25.
        assert!((s[8] - 0.25).abs() < 1e-6, "second echo at [8]: {}", s[8]);
        // Dry+feedback tap at t=0: wet = 1.0 + 0*0.5 = 1.0, with mix=1.0 the
        // output is the wet signal (1.0).
        assert!(
            (s[0] - 1.0).abs() < 1e-6,
            "initial wet tap at [0]: {}",
            s[0]
        );
    }

    #[test]
    fn dry_mix_passes_signal() {
        // mix=0.0 → output is the dry input untouched.
        let mut delay = DelayNode::new(8, 4, 0.5, 0.0, 1);
        let input: Vec<f32> = (0..16).map(|i| i as f32 * 0.01).collect();
        let inp = vec![AudioFrame::from_planar(1, 48_000, input.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; 16])];
        let mut ctx = ProcessContext::new(16, 0, 48_000);
        delay.process(&mut ctx, &inp, &mut out);
        for (got, want) in out[0].samples.iter().zip(input.iter()) {
            assert!(
                (got - want).abs() < 1e-7,
                "dry passthrough mismatch: {got} vs {want}"
            );
        }
    }

    #[test]
    fn feedback_clamped() {
        // feedback=2.0 should be clamped to 0.99 — the line must stay bounded
        // even after many cycles of sustained unity input.
        let mut delay = DelayNode::new(4, 2, 2.0, 1.0, 1);
        let inp = vec![AudioFrame::from_planar(1, 48_000, vec![1.0; 4096])];
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; 4096])];
        let mut ctx = ProcessContext::new(4096, 0, 48_000);
        delay.process(&mut ctx, &inp, &mut out);
        let peak = out[0].samples.iter().fold(0.0_f32, |a, &s| a.max(s.abs()));
        // With feedback clamped to 0.99 the steady-state is 1/(1-0.99)=100.
        // The key assertion is finiteness + well below what unclamped fb=2.0
        // would produce (2^2048 ≈ inf).
        assert!(peak.is_finite(), "peak must be finite: {peak}");
        assert!(
            peak < 200.0,
            "peak {peak} suggests feedback was not clamped"
        );
    }

    #[test]
    fn stereo_channels_independent() {
        // Two channels with the same delay should produce identical per-channel
        // echoes when fed identical impulses.
        let mut delay = DelayNode::new(8, 4, 0.5, 1.0, 2);
        // Planar stereo: [ch0 (8 samples), ch1 (8 samples)].
        let mut input = vec![0.0_f32; 16];
        input[0] = 1.0; // ch0 impulse
        input[8] = 1.0; // ch1 impulse
        let inp = vec![AudioFrame::from_planar(2, 48_000, input)];
        let mut out = vec![AudioFrame::from_planar(2, 48_000, vec![0.0; 16])];
        let mut ctx = ProcessContext::new(8, 0, 48_000);
        delay.process(&mut ctx, &inp, &mut out);
        let s = &out[0].samples;
        // ch0 echo at index 4, ch1 echo at index 8+4=12.
        assert!((s[4] - 0.5).abs() < 1e-6, "ch0 echo: {}", s[4]);
        assert!((s[12] - 0.5).abs() < 1e-6, "ch1 echo: {}", s[12]);
    }
}
