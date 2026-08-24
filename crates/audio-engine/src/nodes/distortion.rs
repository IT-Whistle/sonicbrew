//! Waveshaper distortion — applies a non-linear transfer function to the
//! input signal for harmonic saturation / clipping.
//!
//! The node is fully stateless: `drive`, `mode`, `threshold`, and
//! `output_level` are fixed at construction; `process` does only bounded
//! per-sample arithmetic with no allocation, locking, or panicking.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// Distortion transfer function.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum DistortionMode {
    /// Soft saturation via `tanh` — tube-amp character.
    SoftClip,
    /// Hard clipping — transistor-style brick-wall.
    HardClip,
    /// Foldback distortion — folds the waveform back in on itself (harsh).
    Foldback,
    /// Asymmetric overdrive via `1 - exp(-x)` (fuzz-like).
    Overdrive,
}

/// 1-in / 1-out waveshaper distortion.
///
/// All parameters are fixed at construction; the node carries no per-sample
/// state, so `process_sample` is a pure function of its inputs.
pub struct DistortionNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    drive: f32, // input gain (1.0 = unity, typically 2.0–10.0)
    mode: DistortionMode,
    threshold: f32,    // hardclip/foldback limit (0.0..1.0)
    output_level: f32, // output normalisation gain
}

impl DistortionNode {
    /// Create a distortion node.
    ///
    /// - `mode`: transfer function selection.
    /// - `drive`: input amplification (clamped 0.1..=20.0).
    /// - `threshold`: hardclip/foldback ceiling (clamped 0.01..=1.0).
    /// - `output_level`: output normalisation (clamped 0.0..=2.0).
    /// - `channels`: channel count.
    #[must_use]
    pub fn new(
        mode: DistortionMode,
        drive: f32,
        threshold: f32,
        output_level: f32,
        channels: u16,
    ) -> Self {
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            drive: drive.clamp(0.1, 20.0),
            mode,
            threshold: threshold.clamp(0.01, 1.0),
            output_level: output_level.clamp(0.0, 2.0),
        }
    }

    /// Apply the waveshaper to one sample (pure, stateless).
    #[inline]
    fn process_sample(&self, x: f32) -> f32 {
        let driven = self.drive * x;
        let shaped = match self.mode {
            DistortionMode::SoftClip => driven.tanh(),
            DistortionMode::HardClip => {
                if driven.abs() > self.threshold {
                    self.threshold * driven.signum()
                } else {
                    driven
                }
            }
            DistortionMode::Foldback => {
                let mut s = driven;
                // Fold the signal back into range (bounded iterations for safety).
                for _ in 0..8 {
                    if s.abs() <= self.threshold {
                        break;
                    }
                    s = self.threshold * s.signum() * (2.0 - (s.abs() / self.threshold));
                }
                s.clamp(-self.threshold, self.threshold)
            }
            DistortionMode::Overdrive => {
                if driven >= 0.0 {
                    1.0 - (-driven).exp()
                } else {
                    -(1.0 - driven.exp())
                }
            }
        };
        shaped / self.output_level.max(1e-9)
    }
}

impl AudioNode for DistortionNode {
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
                out.samples[idx] = self.process_sample(inp.samples[idx]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clip_saturates() {
        let mut d = DistortionNode::new(DistortionMode::SoftClip, 5.0, 0.7, 1.0, 1);
        let inp = vec![AudioFrame::from_planar(1, 48000, vec![1.0; 64])];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 64])];
        let mut ctx = ProcessContext::new(64, 0, 48000);
        d.process(&mut ctx, &inp, &mut out);
        // tanh(5.0) ≈ 0.9999 — saturated below 1.0.
        let y = out[0].samples[0].abs();
        assert!(y < 1.0, "soft clip should saturate below 1.0, got {y}");
    }

    #[test]
    fn hard_clip_limits() {
        let mut d = DistortionNode::new(DistortionMode::HardClip, 5.0, 0.5, 1.0, 1);
        // Ramped input from -2.0 to +2.0 — driven = 5*x far exceeds threshold.
        let sig: Vec<f32> = (0..256).map(|i| -2.0 + 4.0 * (i as f32) / 255.0).collect();
        let inp = vec![AudioFrame::from_planar(1, 48000, sig.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48000);
        d.process(&mut ctx, &inp, &mut out);
        for &y in &out[0].samples {
            assert!(y.abs() <= 0.5, "hard clip output {y} exceeds threshold 0.5");
        }
    }

    #[test]
    fn low_drive_passthrough() {
        let mut d = DistortionNode::new(DistortionMode::SoftClip, 1.0, 0.7, 1.0, 1);
        let inp = vec![AudioFrame::from_planar(1, 48000, vec![0.1; 64])];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 64])];
        let mut ctx = ProcessContext::new(64, 0, 48000);
        d.process(&mut ctx, &inp, &mut out);
        // tanh(0.1) ≈ 0.0997 — near unity at low levels.
        let y = out[0].samples[0].abs();
        assert!(
            (y - 0.1).abs() < 0.01,
            "low drive should be near passthrough: {y}"
        );
    }

    #[test]
    fn foldback_stays_bounded() {
        let mut d = DistortionNode::new(DistortionMode::Foldback, 10.0, 0.5, 1.0, 1);
        // Ramped input from -2.0 to +2.0.
        let sig: Vec<f32> = (0..256).map(|i| -2.0 + 4.0 * (i as f32) / 255.0).collect();
        let inp = vec![AudioFrame::from_planar(1, 48000, sig.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48000);
        d.process(&mut ctx, &inp, &mut out);
        for &y in &out[0].samples {
            assert!(y.abs() <= 0.5, "foldback output {y} exceeds bound 0.5");
        }
    }

    #[test]
    fn overdrive_is_finite() {
        let mut d = DistortionNode::new(DistortionMode::Overdrive, 10.0, 0.7, 1.0, 1);
        let sig: Vec<f32> = (0..256).map(|i| -2.0 + 4.0 * (i as f32) / 255.0).collect();
        let inp = vec![AudioFrame::from_planar(1, 48000, sig.clone())];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48000);
        d.process(&mut ctx, &inp, &mut out);
        for &y in &out[0].samples {
            assert!(y.is_finite(), "overdrive produced non-finite output: {y}");
        }
    }

    #[test]
    fn all_modes_finite() {
        for mode in [
            DistortionMode::SoftClip,
            DistortionMode::HardClip,
            DistortionMode::Foldback,
            DistortionMode::Overdrive,
        ] {
            let mut d = DistortionNode::new(mode, 10.0, 0.5, 1.0, 1);
            // Large inputs that could cause overflow in naive implementations.
            let inp = vec![AudioFrame::from_planar(1, 48000, vec![100.0; 64])];
            let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 64])];
            let mut ctx = ProcessContext::new(64, 0, 48000);
            d.process(&mut ctx, &inp, &mut out);
            for &y in &out[0].samples {
                assert!(
                    y.is_finite(),
                    "{mode:?} produced non-finite output for large input: {y}"
                );
            }
        }
    }
}
