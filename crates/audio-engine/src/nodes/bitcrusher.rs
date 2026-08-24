//! Bitcrusher — lo-fi digital degradation via bit-depth reduction and
//! sample-rate decimation (sample-and-hold).
//!
//! All parameters are fixed at construction; per-channel held-sample and
//! counter state is pre-allocated. `process` does only bounded arithmetic
//! with no allocation, locking, or panicking.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 1-in / 1-out bitcrusher (bit-depth + sample-rate reduction).
///
/// Applies two independent degradation stages:
/// 1. **Sample-rate reduction** — a sample-and-hold that updates the held
///    value every `hold_factor` samples, producing the characteristic
///    "staircase" decimation.
/// 2. **Bit-depth reduction** — quantizes the held sample to `2^bits`
///    discrete levels, adding quantisation noise.
pub struct BitcrusherNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    bits: u32,
    hold_factor: usize,
    /// Per-channel sample-and-hold value.
    held_samples: Vec<f32>,
    /// Per-channel decimation counter.
    counters: Vec<usize>,
}

impl BitcrusherNode {
    /// Create a bitcrusher.
    ///
    /// - `bits`: target bit depth (clamped 1..=16). Lower = coarser quantization.
    /// - `hold_factor`: sample-and-hold period in samples (clamped 1..=256).
    ///   1 = no decimation (original sample rate).
    /// - `channels`: channel count.
    #[must_use]
    pub fn new(bits: u32, hold_factor: usize, channels: u16) -> Self {
        let ch = channels.max(1) as usize;
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            bits: bits.clamp(1, 16),
            hold_factor: hold_factor.clamp(1, 256),
            held_samples: vec![0.0; ch],
            counters: vec![0; ch],
        }
    }

    /// Process one sample for channel `ch`.
    #[inline]
    fn process_sample(&mut self, x: f32, ch: usize) -> f32 {
        // 1. Sample-rate reduction (sample-and-hold).
        if self.counters[ch] % self.hold_factor == 0 {
            self.held_samples[ch] = x;
        }
        self.counters[ch] += 1;
        let held = self.held_samples[ch];

        // 2. Bit-depth reduction — quantise to `levels` discrete steps.
        //    Normalise to [0,1], round to the nearest of `levels` steps,
        //    then map back to [-1,1].  This yields exactly `2^bits` output
        //    levels symmetric about zero (e.g. bits=2 → {-1,-⅓,⅓,1}).
        let levels = 1u32 << self.bits;
        let denom = (levels - 1) as f32;
        let norm = (held.clamp(-1.0, 1.0) + 1.0) * 0.5;
        let quantized = (norm * denom).round() / denom;
        quantized * 2.0 - 1.0
    }
}

impl AudioNode for BitcrusherNode {
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
        // Planar layout: [ch0..., ch1...].
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
    fn high_bits_passthrough() {
        let mut bc = BitcrusherNode::new(16, 1, 1);
        let sig: Vec<f32> = (0..256).map(|i| -0.9 + 1.8 * (i as f32) / 255.0).collect();
        let inp = vec![AudioFrame::from_planar(1, 48000, sig.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48000);
        bc.process(&mut ctx, &inp, &mut out);
        for (i, (&x, &y)) in sig.iter().zip(&out[0].samples).enumerate() {
            assert!(
                (y - x).abs() < 1e-3,
                "16-bit / hold=1 should be near-passthrough at sample {i}: in={x}, out={y}"
            );
        }
    }

    #[test]
    fn low_bits_quantizes() {
        let mut bc = BitcrusherNode::new(2, 1, 1);
        // Ramped signal from -1 to +1.
        let sig: Vec<f32> = (0..256).map(|i| -1.0 + 2.0 * (i as f32) / 255.0).collect();
        let inp = vec![AudioFrame::from_planar(1, 48000, sig)];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48000);
        bc.process(&mut ctx, &inp, &mut out);
        let allowed = [-1.0_f32, -1.0 / 3.0, 1.0 / 3.0, 1.0];
        for (i, &y) in out[0].samples.iter().enumerate() {
            assert!(
                allowed.iter().any(|&a| (y - a).abs() < 1e-5),
                "2-bit output at sample {i} ({y}) is not a valid quantisation level"
            );
        }
    }

    #[test]
    fn hold_factor_repeats() {
        let mut bc = BitcrusherNode::new(16, 4, 1);
        // Distinct values per sample so hold is detectable.
        let sig: Vec<f32> = (0..16).map(|i| i as f32 * 0.01).collect();
        let inp = vec![AudioFrame::from_planar(1, 48000, sig)];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 16])];
        let mut ctx = ProcessContext::new(16, 0, 48000);
        bc.process(&mut ctx, &inp, &mut out);
        // Samples 0..4 all hold sig[0]=0.0; samples 4..8 all hold sig[4]=0.04.
        let block0 = &out[0].samples[0..4];
        let block1 = &out[0].samples[4..8];
        for &v in &block0[1..] {
            assert!(
                (v - block0[0]).abs() < 1e-6,
                "hold block 0 should repeat 0.0: got {v}"
            );
        }
        for &v in &block1[1..] {
            assert!(
                (v - block1[0]).abs() < 1e-6,
                "hold block 1 should repeat ~0.04: got {v}"
            );
        }
        assert!(
            (block0[0] - block1[0]).abs() > 1e-3,
            "blocks should differ: block0={}, block1={}",
            block0[0],
            block1[0]
        );
    }

    #[test]
    fn stereo_independent() {
        let mut bc = BitcrusherNode::new(16, 2, 2);
        // 7 samples per channel (odd count — detects shared vs per-channel counter).
        let ch0 = [0.3_f32, 0.0, 0.5, 0.0, 0.7, 0.0, 0.9];
        let ch1 = [0.8_f32, 0.0, 0.6, 0.0, 0.4, 0.0, 0.2];
        let mut planar = ch0.to_vec();
        planar.extend_from_slice(&ch1);
        let inp = vec![AudioFrame::from_planar(2, 48000, planar)];
        let mut out = vec![AudioFrame::from_planar(2, 48000, vec![0.0; 14])];
        let mut ctx = ProcessContext::new(7, 0, 48000);
        bc.process(&mut ctx, &inp, &mut out);
        let out0 = &out[0].samples[0..7];
        let out1 = &out[0].samples[7..14];

        // Per-channel counter: ch1 sample 0 has counter=0 (even) → updates → holds 0.8.
        // A shared global counter would leave ch1 starting at counter=7 (odd) →
        // no update → holds the initial 0.0.
        assert!(
            (out1[0] - 0.8).abs() < 1e-3,
            "ch1[0] should hold its own first sample 0.8 (per-channel counter): got {}",
            out1[0]
        );
        assert!(
            (out0[0] - 0.3).abs() < 1e-3,
            "ch0[0] should hold 0.3: got {}",
            out0[0]
        );

        // Both channels show hold behaviour: pairs [0,1] repeat.
        assert!(
            (out0[0] - out0[1]).abs() < 1e-6,
            "ch0 hold pair should match: {} vs {}",
            out0[0],
            out0[1]
        );
        assert!(
            (out1[0] - out1[1]).abs() < 1e-6,
            "ch1 hold pair should match: {} vs {}",
            out1[0],
            out1[1]
        );
    }
}
