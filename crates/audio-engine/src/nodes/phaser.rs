//! Phaser — LFO-swept allpass cascade producing the classic sweeping
//! "notch" phaser effect.
//!
//! A chain of first-order allpass filters (default 4 stages) has its cutoff
//! frequency modulated by a single shared sine LFO. The allpass output is
//! crossfaded with the dry signal, producing moving notches (frequency
//! cancellations). A feedback path recirculates the allpass-chain output to
//! sharpen the notches. `process` performs only bounded sample math with no
//! allocation, locking, or panicking.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 2π — full LFO cycle.
const TWO_PI: f32 = std::f32::consts::TAU;

/// Maximum feedback factor — kept below 1.0 so the feedback loop cannot
/// diverge. Phaser notches can sharpen the effective gain, so the ceiling is
/// stricter than the flanger/delay nodes.
const FEEDBACK_CEIL: f32 = 0.7;

/// State for one first-order allpass stage (per channel).
#[derive(Clone, Copy, Default)]
struct AllpassStage {
    xm1: f32,
    ym1: f32,
}

/// 1-in / 1-out LFO-swept allpass cascade phaser.
///
/// The LFO phase is shared across all channels (a single sweep), while the
/// allpass stage and feedback state are independent per channel.
pub struct PhaserNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    /// Feedback gain applied to the allpass-chain output.
    feedback: f32,
    /// Wet/dry crossfade (0.0 = dry only, 1.0 = wet only).
    mix: f32,
    /// Number of allpass stages per channel.
    num_stages: usize,
    /// Base cutoff frequency the LFO modulates around (Hz).
    base_freq: f32,
    /// LFO modulation depth (0.0..1.0).
    depth: f32,
    /// Per-sample LFO phase increment (2π · rate / sample_rate).
    phase_inc: f32,
    /// Sample rate in Hz.
    sample_rate: f32,
    /// Per-channel allpass stage states, laid out [ch0_s0, ch0_s1, …, ch1_s0, …].
    stages: Vec<AllpassStage>,
    /// Per-channel feedback accumulator (last allpass-chain output).
    feedback_state: Vec<f32>,
    /// Shared LFO phase (0..2π), advanced once per sample-time.
    lfo_phase: f32,
}

impl PhaserNode {
    /// Create a phaser node.
    ///
    /// - `rate_hz`: LFO frequency (clamped to 0.05..10.0).
    /// - `base_freq`: allpass centre cutoff the LFO sweeps around (Hz).
    /// - `depth`: LFO modulation depth (clamped to 0.0..1.0).
    /// - `feedback`: allpass-chain output feedback (clamped to `0.0..=FEEDBACK_CEIL`).
    /// - `mix`: wet/dry crossfade (clamped to 0.0..1.0).
    /// - `stages`: number of allpass stages (clamped to 2..=8).
    /// - `sample_rate` / `channels`: graph configuration.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rate_hz: f32,
        base_freq: f32,
        depth: f32,
        feedback: f32,
        mix: f32,
        stages: usize,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        let rate = rate_hz.clamp(0.05, 10.0);
        let sr = sample_rate as f32;
        // Keep the peak modulated cutoff (base * (1 + depth)) below Nyquist so
        // the allpass coefficient stays finite: with depth ≤ 1, base ≤ sr/4.
        let base = base_freq.clamp(10.0, sr * 0.24);
        let dep = depth.clamp(0.0, 1.0);
        let fb = feedback.clamp(0.0, FEEDBACK_CEIL);
        let mx = mix.clamp(0.0, 1.0);
        let ns = stages.clamp(2, 8);
        let ch = channels.max(1) as usize;
        let phase_inc = TWO_PI * rate / sr;
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            feedback: fb,
            mix: mx,
            num_stages: ns,
            base_freq: base,
            depth: dep,
            phase_inc,
            sample_rate: sr,
            stages: vec![AllpassStage::default(); ch * ns],
            feedback_state: vec![0.0; ch],
            lfo_phase: 0.0,
        }
    }

    /// Process one sample for channel `ch` using the shared LFO coefficient `a`.
    #[inline]
    fn process_sample(&mut self, x: f32, ch: usize, a: f32) -> f32 {
        // Inject feedback from the previous allpass-chain output.
        let mut wet = x + self.feedback_state[ch] * self.feedback;

        // Cascade through the per-channel first-order allpass stages:
        //   y[n] = -a·x[n] + x[n-1] + a·y[n-1]
        let base = ch * self.num_stages;
        for s in 0..self.num_stages {
            let stage = &mut self.stages[base + s];
            let y = -a * wet + stage.xm1 + a * stage.ym1;
            stage.xm1 = wet;
            stage.ym1 = y;
            wet = y;
        }

        // Store the chain output for next-sample feedback.
        self.feedback_state[ch] = wet;

        x * (1.0 - self.mix) + wet * self.mix
    }

    /// Compute the shared LFO-modulated allpass coefficient for the current
    /// `lfo_phase`. The 1st-order allpass coefficient is `tan(w₀/2)`, expanded
    /// as `sin(w₀) / (1 + cos(w₀))`.
    #[inline]
    fn allpass_coef(&self) -> f32 {
        let lfo = self.lfo_phase.sin();
        let fc = self.base_freq * (1.0 + self.depth * (0.5 + 0.5 * lfo));
        let w0 = TWO_PI * fc / self.sample_rate;
        let (sin_w, cos_w) = w0.sin_cos();
        sin_w / (1.0 + cos_w)
    }
}

impl AudioNode for PhaserNode {
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
        // Sample-outer / channel-inner so the shared LFO advances exactly once
        // per sample-time regardless of channel count.
        for i in 0..per_ch {
            let a = self.allpass_coef();
            for c in 0..ch {
                let idx = c * per_ch + i;
                out.samples[idx] = self.process_sample(inp.samples[idx], c, a);
            }
            // Advance the shared LFO once per sample-time.
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
    fn dry_passthrough() {
        // mix=0.0 → output is the dry input untouched (the allpass chain and
        // feedback still run internally but never reach the output crossfade).
        let mut ph = PhaserNode::new(0.5, 800.0, 0.5, 0.3, 0.0, 4, 48_000, 1);
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.01).sin()).collect();
        let inp = vec![AudioFrame::from_planar(1, 48_000, input.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48_000);
        ph.process(&mut ctx, &inp, &mut out);
        for (got, want) in out[0].samples.iter().zip(input.iter()) {
            assert!(
                (got - want).abs() < 1e-6,
                "dry passthrough mismatch: {got} vs {want}"
            );
        }
    }

    #[test]
    fn wet_affects_signal() {
        // mix=1.0, 256 samples of a 440Hz sine → the allpass cascade (plus
        // feedback) shifts phase and reshapes amplitude, so the output must
        // differ measurably from the input.
        let sr = 48_000u32;
        let n = 256usize;
        let mut ph = PhaserNode::new(0.5, 800.0, 0.5, 0.3, 1.0, 4, sr, 1);
        let input: Vec<f32> = (0..n)
            .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin() * 0.5)
            .collect();
        let inp = vec![AudioFrame::from_planar(1, sr, input.clone())];
        let mut out = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, sr);
        ph.process(&mut ctx, &inp, &mut out);
        let diff: f32 = out[0]
            .samples
            .iter()
            .zip(input.iter())
            .map(|(&o, &i)| (o - i).abs())
            .sum();
        assert!(diff > 1.0, "wet path should differ from input, diff={diff}");
    }

    #[test]
    fn no_nan_overflow() {
        // rate=5Hz, feedback=0.5, 10k samples of a 440Hz tone → all outputs finite.
        let sr = 48_000u32;
        let n = 10_000usize;
        let mut ph = PhaserNode::new(5.0, 800.0, 0.5, 0.5, 0.5, 4, sr, 1);
        let input: Vec<f32> = (0..n)
            .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin() * 0.8)
            .collect();
        let inp = vec![AudioFrame::from_planar(1, sr, input)];
        let mut out = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, sr);
        ph.process(&mut ctx, &inp, &mut out);
        for &s in &out[0].samples {
            assert!(s.is_finite(), "output must be finite, got {s}");
        }
        let peak = out[0].samples.iter().fold(0.0_f32, |a, &s| a.max(s.abs()));
        assert!(peak <= 4.0, "output should stay bounded, peak={peak}");
    }

    #[test]
    fn feedback_changes_output() {
        // feedback=0.5 vs feedback=0.0 → different outputs on a 440Hz sine.
        // Feedback recirculates the allpass-chain output, deepening the notches.
        let sr = 48_000u32;
        let n = 4096usize;
        let input: Vec<f32> = (0..n)
            .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin() * 0.5)
            .collect();
        let run = |fb: f32| -> Vec<f32> {
            let mut ph = PhaserNode::new(0.5, 800.0, 0.5, fb, 0.5, 4, sr, 1);
            let inp = vec![AudioFrame::from_planar(1, sr, input.clone())];
            let mut out = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
            let mut ctx = ProcessContext::new(n, 0, sr);
            ph.process(&mut ctx, &inp, &mut out);
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
}
