//! Noise generator — 0-in / 1-out source producing white or pink noise.
//!
//! All state (RNG + per-channel pink filter coefficients) is pre-allocated at
//! construction; `process` does only bounded arithmetic — no allocation,
//! locking, or panicking.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// Noise colour selection.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum NoiseColor {
    /// Flat-spectrum white noise.
    White,
    /// 1/f pink noise (Paul Kellet filter).
    Pink,
}

/// 0-in / 1-out noise source (seedable, RT-safe).
pub struct NoiseSource {
    out_port: [PortDescriptor; 1],
    color: NoiseColor,
    amp: f32,
    rng_state: u64,
    channels: u16,
    /// Paul Kellet pink filter state: 7 coefficients (b0–b6) per channel.
    pink: Vec<f32>,
}

impl NoiseSource {
    /// Create a noise source.
    ///
    /// - `color`: white or pink.
    /// - `amp`: output amplitude (0.0..=1.0 typical).
    /// - `seed`: RNG seed (0 → uses a fixed non-zero default to avoid the
    ///   xorshift degenerate case).
    /// - `channels`: number of output channels.
    #[must_use]
    pub fn new(color: NoiseColor, amp: f32, seed: u64, channels: u16) -> Self {
        Self {
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            color,
            amp,
            rng_state: if seed == 0 { 1 } else { seed },
            channels,
            pink: vec![0.0; 7 * channels as usize],
        }
    }

    /// xorshift64 step → roughly uniform [-1.0, 1.0).
    #[inline]
    fn next_rand(&mut self) -> f32 {
        self.rng_state ^= self.rng_state << 13;
        self.rng_state ^= self.rng_state >> 7;
        self.rng_state ^= self.rng_state << 17;
        let lo = self.rng_state as u32;
        (lo as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// Generate one sample for channel `ch`, updating filter state.
    #[inline]
    fn gen_sample(&mut self, ch: usize) -> f32 {
        let white = self.next_rand();
        match self.color {
            NoiseColor::White => white * self.amp,
            NoiseColor::Pink => {
                let base = ch * 7;
                let s = &mut self.pink;
                s[base] = 0.998_86 * s[base] + white * 0.055_517_9;
                s[base + 1] = 0.993_32 * s[base + 1] + white * 0.075_075_9;
                s[base + 2] = 0.969_00 * s[base + 2] + white * 0.153_852;
                s[base + 3] = 0.866_50 * s[base + 3] + white * 0.310_485_6;
                s[base + 4] = 0.550_00 * s[base + 4] + white * 0.532_952_2;
                s[base + 5] = -0.761_6 * s[base + 5] - white * 0.016_898_0;
                let pink = (s[base]
                    + s[base + 1]
                    + s[base + 2]
                    + s[base + 3]
                    + s[base + 4]
                    + s[base + 5]
                    + s[base + 6]
                    + white * 0.5362)
                    * 0.11;
                s[base + 6] = white * 0.115_926;
                pink * self.amp
            }
        }
    }
}

impl AudioNode for NoiseSource {
    fn inputs(&self) -> &[PortDescriptor] {
        &[]
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_port
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        _in_frames: &[AudioFrame],
        out_frames: &mut [AudioFrame],
    ) {
        let Some(out) = out_frames.get_mut(0) else {
            return;
        };
        let ch = self.channels as usize;
        let n = out.samples.len();
        if ch == 0 || n == 0 {
            return;
        }
        let per_ch = n / ch;
        for c in 0..ch {
            let offset = c * per_ch;
            for i in 0..per_ch {
                out.samples[offset + i] = self.gen_sample(c);
            }
        }
        out.channels = self.channels;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(node: &mut NoiseSource, n: usize) -> Vec<f32> {
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, 48_000);
        node.process(&mut ctx, &[], &mut out);
        out[0].samples.clone()
    }

    #[test]
    fn white_noise_is_not_silent() {
        let mut node = NoiseSource::new(NoiseColor::White, 0.5, 42, 1);
        let s = run(&mut node, 256);
        let peak = s.iter().fold(0.0_f32, |a, &v| a.max(v.abs()));
        assert!(peak > 0.1, "white noise peak {peak} too low");
        // Randomness: most consecutive samples must differ.
        let diffs = s.windows(2).filter(|w| w[0] != w[1]).count();
        assert!(diffs > 200, "only {diffs} transitions in 255 pairs");
    }

    #[test]
    fn pink_noise_is_not_silent() {
        let mut node = NoiseSource::new(NoiseColor::Pink, 0.5, 42, 1);
        let s = run(&mut node, 256);
        let peak = s.iter().fold(0.0_f32, |a, &v| a.max(v.abs()));
        assert!(peak > 0.01, "pink noise peak {peak} too low");
    }

    #[test]
    fn seed_reproducibility() {
        let mut a = NoiseSource::new(NoiseColor::White, 1.0, 12345, 1);
        let mut b = NoiseSource::new(NoiseColor::White, 1.0, 12345, 1);
        let sa = run(&mut a, 64);
        let sb = run(&mut b, 64);
        assert_eq!(sa, sb, "same seed must produce identical output");
    }

    #[test]
    fn zero_seed_uses_default() {
        // Must not panic and must produce non-silent output.
        let mut node = NoiseSource::new(NoiseColor::White, 0.5, 0, 1);
        let s = run(&mut node, 64);
        let peak = s.iter().fold(0.0_f32, |a, &v| a.max(v.abs()));
        assert!(peak > 0.0, "zero seed produced silence");
    }
}
