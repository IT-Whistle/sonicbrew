//! Dynamic range compressor — reduces the loudness of signals above a
//! threshold by a ratio, with attack/release envelope smoothing.
//!
//! All parameters are fixed at construction; the envelope follower state is
//! pre-allocated per channel. `process` does bounded sample math with a
//! peak-detection envelope and smooth gain reduction — no allocation or
//! locking.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 1-in / 1-out dynamic range compressor.
pub struct CompressorNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    threshold: f32,    // linear amplitude (0.0..1.0)
    ratio: f32,        // compression ratio (1.0 = no compression, inf = brick-wall)
    attack_coef: f32,  // envelope attack coefficient (per sample)
    release_coef: f32, // envelope release coefficient
    makeup: f32,       // output makeup gain (linear)
    /// Per-channel envelope follower state.
    envelope: Vec<f32>,
}

impl CompressorNode {
    /// Create a compressor.
    ///
    /// - `threshold_db`: level above which compression engages (dB).
    /// - `ratio`: gain reduction ratio (e.g. 4.0 means 4:1).
    /// - `attack_ms` / `release_ms`: envelope time constants.
    /// - `makeup_db`: output gain compensation.
    /// - `sample_rate` / `channels`: graph configuration.
    #[must_use]
    pub fn new(
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_db: f32,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        let threshold = 10.0_f32.powf(threshold_db / 20.0);
        let makeup = 10.0_f32.powf(makeup_db / 20.0);
        let attack_coef = time_constant(attack_ms, sample_rate);
        let release_coef = time_constant(release_ms, sample_rate);
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            threshold,
            ratio,
            attack_coef,
            release_coef,
            makeup,
            envelope: vec![0.0; channels as usize],
        }
    }

    /// Process one sample for channel `ch`, updating the envelope.
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

        // Gain reduction: above threshold, compress by ratio.
        let gain = if *env > self.threshold && *env > 1e-9 {
            let over = *env / self.threshold;
            // compressed_level = threshold * over^(1/ratio)
            let compressed = self.threshold * over.powf(1.0 / self.ratio);
            compressed / *env
        } else {
            1.0
        };
        x * gain * self.makeup
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

impl AudioNode for CompressorNode {
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
    fn below_threshold_no_reduction() {
        let mut comp = CompressorNode::new(-12.0, 4.0, 1.0, 100.0, 0.0, 48_000, 1);
        let inp = vec![AudioFrame::from_planar(1, 48000, vec![0.1; 256])];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48000);
        comp.process(&mut ctx, &inp, &mut out);
        // 0.1 ≈ -20dB, well below -12dB threshold → no compression.
        let last = out[0].samples[255].abs();
        assert!((last - 0.1).abs() < 0.01, "below threshold: {last}");
    }

    #[test]
    fn above_threshold_reduces_level() {
        let mut comp = CompressorNode::new(-6.0, 10.0, 0.1, 100.0, 0.0, 48_000, 1);
        // 0.9 ≈ -0.9dB, well above -6dB threshold.
        let inp = vec![AudioFrame::from_planar(1, 48000, vec![0.9; 512])];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 512])];
        let mut ctx = ProcessContext::new(512, 0, 48000);
        comp.process(&mut ctx, &inp, &mut out);
        // After enough samples for the envelope to settle, output < input.
        let last = out[0].samples[511].abs();
        assert!(last < 0.9, "compressed output {last} should be < 0.9");
    }
}
