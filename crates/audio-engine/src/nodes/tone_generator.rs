//! Tone generator — 0-in / 1-out multi-waveform oscillator source.
//!
//! Generates sine, square, saw, or triangle waves via a per-channel phase
//! accumulator. All state is pre-allocated at construction; `process` does
//! only bounded arithmetic — no allocation, locking, or panicking.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};
use std::f32::consts::PI;

/// Waveform selection.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Waveform {
    /// Sine wave.
    Sine,
    /// Square wave (±1 transitions at zero crossings).
    Square,
    /// Sawtooth wave (linear ramp −1 → +1).
    Saw,
    /// Triangle wave.
    Triangle,
}

/// 0-in / 1-out multi-waveform tone generator (RT-safe).
///
/// Uses a per-channel phase accumulator incremented by `phase_inc` each sample.
/// The accumulator wraps to `[0, 2π)` to prevent overflow over long runs.
pub struct ToneGenerator {
    out_port: [PortDescriptor; 1],
    waveform: Waveform,
    amp: f32,
    channels: u16,
    /// Per-channel phase accumulator (radians, kept in `[0, 2π)`).
    phase: Vec<f32>,
    /// Per-sample phase increment = `2π × freq / sample_rate`.
    phase_inc: f32,
}

impl ToneGenerator {
    /// Create a tone generator.
    ///
    /// - `waveform`: sine, square, saw, or triangle.
    /// - `freq`: frequency in Hz (must be positive; `≤ 0` clamps to `1.0`).
    /// - `amp`: output amplitude (clamped to `0.0..=1.0`).
    /// - `sample_rate`: sample rate in Hz.
    /// - `channels`: number of output channels.
    #[must_use]
    pub fn new(waveform: Waveform, freq: f32, amp: f32, sample_rate: u32, channels: u16) -> Self {
        let freq = if freq <= 0.0 { 1.0 } else { freq };
        let amp = amp.clamp(0.0, 1.0);
        let phase_inc = 2.0 * PI * freq / sample_rate as f32;
        Self {
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            waveform,
            amp,
            channels,
            phase: vec![0.0; channels as usize],
            phase_inc,
        }
    }

    /// Compute one sample for the given waveform and phase.
    #[inline]
    fn sample(waveform: Waveform, phase: f32) -> f32 {
        let two_pi = 2.0 * PI;
        match waveform {
            Waveform::Sine => phase.sin(),
            Waveform::Square => {
                if phase.sin() >= 0.0 {
                    1.0
                } else {
                    -1.0
                }
            }
            Waveform::Saw => {
                let t = (phase % two_pi) / two_pi;
                2.0 * t - 1.0
            }
            Waveform::Triangle => {
                let t = (phase % two_pi) / two_pi;
                4.0 * (t - 0.5).abs() - 1.0
            }
        }
    }
}

impl AudioNode for ToneGenerator {
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
        let two_pi = 2.0 * PI;
        for c in 0..ch {
            let offset = c * per_ch;
            for i in 0..per_ch {
                out.samples[offset + i] = Self::sample(self.waveform, self.phase[c]) * self.amp;
                self.phase[c] += self.phase_inc;
                if self.phase[c] >= two_pi {
                    self.phase[c] -= two_pi;
                }
            }
        }
        out.channels = self.channels;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(node: &mut ToneGenerator, n: usize) -> Vec<f32> {
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, 48_000);
        node.process(&mut ctx, &[], &mut out);
        out[0].samples.clone()
    }

    #[test]
    fn sine_matches_reference() {
        let freq = 440.0_f32;
        let amp = 0.8_f32;
        let sr = 48_000_u32;
        let mut node = ToneGenerator::new(Waveform::Sine, freq, amp, sr, 1);
        let s = run(&mut node, 256);
        let phase_inc = 2.0 * PI * freq / sr as f32;
        for (i, &got) in s.iter().enumerate() {
            let expected = (phase_inc * i as f32).sin() * amp;
            assert!(
                (got - expected).abs() < 1e-5,
                "sample {i}: got {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn square_is_bipolar() {
        let amp = 0.5_f32;
        let mut node = ToneGenerator::new(Waveform::Square, 440.0, amp, 48_000, 1);
        let s = run(&mut node, 256);
        for (i, &v) in s.iter().enumerate() {
            assert!(
                (v - amp).abs() < 1e-6 || (v + amp).abs() < 1e-6,
                "sample {i}: {v} not ±{amp}"
            );
        }
    }

    #[test]
    fn triangle_is_bounded() {
        let amp = 0.7_f32;
        let mut node = ToneGenerator::new(Waveform::Triangle, 220.0, amp, 48_000, 1);
        let s = run(&mut node, 1024);
        let peak = s.iter().fold(0.0_f32, |a, &v| a.max(v.abs()));
        assert!(
            peak > amp * 0.9,
            "triangle peak {peak} too far from amp {amp}"
        );
        for (i, &v) in s.iter().enumerate() {
            assert!(v.abs() <= amp + 1e-6, "sample {i}: {v} exceeds ±{amp}");
        }
    }

    #[test]
    fn saw_is_monotonic_rising_in_period() {
        let freq = 100.0_f32;
        let sr = 48_000_u32;
        let mut node = ToneGenerator::new(Waveform::Saw, freq, 1.0, sr, 1);
        let period = (sr as f32 / freq) as usize;
        let s = run(&mut node, period);
        // Within one period the saw wave should be monotonically rising.
        for i in 1..period {
            assert!(
                s[i] >= s[i - 1] - 1e-6,
                "sample {i}: {} < {} (not monotonic)",
                s[i],
                s[i - 1]
            );
        }
    }

    #[test]
    fn phase_wraps_without_overflow() {
        let mut node = ToneGenerator::new(Waveform::Sine, 20_000.0, 1.0, 48_000, 1);
        let s = run(&mut node, 100_000);
        assert!(
            s.iter().all(|&v| v.is_finite()),
            "NaN/Inf detected in output"
        );
        assert!(
            node.phase[0] < 2.0 * PI,
            "phase {} not wrapped below 2π",
            node.phase[0]
        );
    }
}
