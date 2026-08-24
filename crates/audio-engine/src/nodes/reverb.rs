//! Schroeder / Freeverb-style reverberation — a parallel bank of feedback
//! comb filters (each with a one-pole damper in the loop) followed by a
//! series of allpass filters, with an independent engine per channel and a
//! wet/dry mix.
//!
//! All filter state is pre-allocated at construction; `process` performs only
//! bounded per-sample reads/writes into the comb/allpass buffers with no
//! allocation, locking, or panicking. The comb feedback factor derived from
//! `room_size` is strictly below unity (max 0.98), so the tank cannot diverge.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// Freeverb reference comb delay lengths at 44.1 kHz.
const COMB_DELAYS_441: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
/// Freeverb reference allpass delay lengths at 44.1 kHz.
const ALLPASS_DELAYS_441: [usize; 4] = [556, 441, 341, 225];
/// Number of parallel comb filters per channel (Freeverb standard).
const COMBS_PER_CHANNEL: usize = 8;
/// Number of series allpass filters per channel (Freeverb standard).
const ALLPASSES_PER_CHANNEL: usize = 4;
/// Allpass feedback factor (fixed in the classic Schroeder/Freeverb design).
const ALLPASS_FEEDBACK: f32 = 0.5;
/// Reference sample rate for the delay-length tables above.
const REFERENCE_RATE: u32 = 44_100;

/// Feedback comb filter with a one-pole lowpass damper in the feedback loop.
struct CombFilter {
    buffer: Vec<f32>,
    idx: usize,
    feedback: f32,
    /// Damping lowpass coefficient applied to the stored state.
    damp1: f32,
    /// Damping lowpass coefficient applied to the read output.
    damp2: f32,
    /// One-pole lowpass state (the damper).
    lowpass: f32,
}

impl CombFilter {
    fn new(delay: usize, feedback: f32, damping: f32) -> Self {
        Self {
            buffer: vec![0.0; delay.max(1)],
            idx: 0,
            feedback,
            damp1: damping,
            damp2: 1.0 - damping,
            lowpass: 0.0,
        }
    }

    /// Process one sample through the comb.
    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let output = self.buffer[self.idx];
        self.lowpass = output * self.damp2 + self.lowpass * self.damp1;
        self.buffer[self.idx] = input + self.lowpass * self.feedback;
        self.idx += 1;
        if self.idx >= self.buffer.len() {
            self.idx = 0;
        }
        output
    }
}

/// Schroeder allpass filter with fixed feedback.
struct AllpassFilter {
    buffer: Vec<f32>,
    idx: usize,
}

impl AllpassFilter {
    fn new(delay: usize) -> Self {
        Self {
            buffer: vec![0.0; delay.max(1)],
            idx: 0,
        }
    }

    /// Process one sample through the allpass.
    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let buffered = self.buffer[self.idx];
        let output = -input + buffered;
        self.buffer[self.idx] = input + buffered * ALLPASS_FEEDBACK;
        self.idx += 1;
        if self.idx >= self.buffer.len() {
            self.idx = 0;
        }
        output
    }
}

/// 1-in / 1-out Schroeder/Freeverb reverb with per-channel engines.
pub struct ReverbNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    wet: f32,
    dry: f32,
    /// Per-channel comb banks, laid out as `combs[ch*8..ch*8+8]`.
    combs: Vec<CombFilter>,
    /// Per-channel allpass banks, laid out as `allpasses[ch*4..ch*4+4]`.
    allpasses: Vec<AllpassFilter>,
}

impl ReverbNode {
    /// Create a reverb node.
    ///
    /// - `room_size`: 0.0..1.0 → comb feedback 0.7..0.98 (larger = longer tail).
    /// - `damping`: 0.0..1.0 → high-frequency absorption in the feedback loop.
    /// - `wet` / `dry`: output gains for the reverberated and dry paths.
    /// - `sample_rate` / `channels`: graph configuration; comb/allpass delay
    ///   lengths scale proportionally to `sample_rate` from the 44.1 kHz
    ///   Freeverb reference tables.
    #[must_use]
    pub fn new(
        room_size: f32,
        damping: f32,
        wet: f32,
        dry: f32,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        let room_size = room_size.clamp(0.0, 1.0);
        let damping = damping.clamp(0.0, 1.0);
        let wet = wet.clamp(0.0, 1.0);
        let dry = dry.clamp(0.0, 1.0);
        let ch = channels.max(1) as usize;
        let scale = sample_rate as f32 / REFERENCE_RATE as f32;
        let feedback = room_size * 0.28 + 0.7;

        let mut combs = Vec::with_capacity(COMBS_PER_CHANNEL * ch);
        for _ in 0..ch {
            for &base in &COMB_DELAYS_441 {
                combs.push(CombFilter::new(scale_delay(base, scale), feedback, damping));
            }
        }
        let mut allpasses = Vec::with_capacity(ALLPASSES_PER_CHANNEL * ch);
        for _ in 0..ch {
            for &base in &ALLPASS_DELAYS_441 {
                allpasses.push(AllpassFilter::new(scale_delay(base, scale)));
            }
        }

        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            wet,
            dry,
            combs,
            allpasses,
        }
    }

    /// Process one sample for channel `ch`.
    #[inline]
    fn process_sample(&mut self, x: f32, ch: usize) -> f32 {
        let comb_base = ch * COMBS_PER_CHANNEL;
        let mut comb_out = 0.0_f32;
        for i in 0..COMBS_PER_CHANNEL {
            comb_out += self.combs[comb_base + i].process(x);
        }
        let ap_base = ch * ALLPASSES_PER_CHANNEL;
        let mut wet_out = comb_out;
        for i in 0..ALLPASSES_PER_CHANNEL {
            wet_out = self.allpasses[ap_base + i].process(wet_out);
        }
        x * self.dry + wet_out * self.wet
    }
}

/// Scale a 44.1 kHz reference delay length to the current sample rate.
fn scale_delay(base: usize, scale: f32) -> usize {
    ((base as f32) * scale).round().max(1.0) as usize
}

impl AudioNode for ReverbNode {
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

    /// Run `node` over a mono planar buffer and return the output samples.
    fn run_mono(node: &mut ReverbNode, input: &[f32], sr: u32) -> Vec<f32> {
        let n = input.len();
        let inp = vec![AudioFrame::from_planar(1, sr, input.to_vec())];
        let mut out = vec![AudioFrame::from_planar(1, sr, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, sr);
        node.process(&mut ctx, &inp, &mut out);
        out[0].samples.clone()
    }

    #[test]
    fn dry_signal_preserved() {
        // wet=0.0 → output equals the dry input regardless of reverb state.
        let mut rev = ReverbNode::new(0.8, 0.5, 0.0, 1.0, 48_000, 1);
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.01).sin()).collect();
        let out = run_mono(&mut rev, &input, 48_000);
        for (got, want) in out.iter().zip(input.iter()) {
            assert!(
                (got - want).abs() < 1e-6,
                "dry passthrough mismatch: {got} vs {want}"
            );
        }
    }

    #[test]
    fn wet_adds_tail() {
        // dry=0.0, wet=1.0, large room. An impulse at t=0 should produce a
        // non-zero reverberation tail once the comb delay lines ring out.
        let mut rev = ReverbNode::new(0.8, 0.5, 1.0, 0.0, 48_000, 1);
        let mut input = vec![0.0_f32; 4096];
        input[0] = 1.0;
        let out = run_mono(&mut rev, &input, 48_000);
        // The initial output is zero (tanks are empty); the tail that follows
        // must contain non-zero energy.
        let tail_energy: f32 = out[1500..].iter().map(|s| s.abs()).sum();
        assert!(
            tail_energy > 0.0,
            "reverb tail must be non-zero after impulse, got {tail_energy}"
        );
    }

    #[test]
    fn impulse_response_decays() {
        // A single impulse then silence. The response must peak and then
        // decay: the final sample's magnitude is below the peak magnitude.
        let mut rev = ReverbNode::new(0.5, 0.5, 1.0, 1.0, 48_000, 1);
        let mut input = vec![0.0_f32; 16_384];
        input[0] = 1.0;
        let out = run_mono(&mut rev, &input, 48_000);
        let peak = out.iter().fold(0.0_f32, |a, &s| a.max(s.abs()));
        assert!(peak > 0.0, "impulse must produce a response: peak {peak}");
        let last = out[16_383].abs();
        assert!(
            last < peak,
            "tail ({last}) should be below the peak ({peak}) — no decay"
        );
    }

    #[test]
    fn clamps_params() {
        // Out-of-range room_size/damping must clamp, not panic, and stay finite.
        let mut rev = ReverbNode::new(2.0, -1.0, 1.0, 1.0, 48_000, 1);
        let input = vec![0.5_f32; 1024];
        let out = run_mono(&mut rev, &input, 48_000);
        for &s in &out {
            assert!(s.is_finite(), "output must be finite after clamping: {s}");
        }
    }
}
