//! Stereo width control via mid/side processing.
//!
//! Decomposes a stereo signal into mid (L+R)/2 and side (L-R)/2 components,
//! scales the side (difference) component by a width factor, then reconstructs
//! L/R. A width of 0.0 collapses to mono, 1.0 is passthrough, >1.0 widens the
//! stereo image. Mono input is passed through unchanged.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 1-in / 1-out stereo width controller via mid/side processing.
///
/// `width` of 0.0 collapses to mono, 1.0 is passthrough, 2.0 doubles the
/// side component. Non-stereo input is passed through unchanged.
pub struct StereoWidenerNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    width: f32,
}

impl StereoWidenerNode {
    /// Create a stereo widener.
    ///
    /// - `width`: 0.0 = mono, 1.0 = original, 2.0 = doubled side (clamped to 0.0..=2.0).
    /// - `channels`: must be 2 to have an effect; 1 (or any non-stereo) is passthrough.
    #[must_use]
    pub fn new(width: f32, channels: u16) -> Self {
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            width: width.clamp(0.0, 2.0),
        }
    }
}

impl AudioNode for StereoWidenerNode {
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
        let n = inp.samples.len().min(out.samples.len());
        // Default: copy input to output.
        out.samples[..n].copy_from_slice(&inp.samples[..n]);

        let ch = inp.channels as usize;
        if ch != 2 || n == 0 {
            // Mono or degenerate: width is meaningless — passthrough.
            return;
        }
        let per_ch = n / ch;
        for i in 0..per_ch {
            let left = inp.samples[i];
            let right = inp.samples[per_ch + i];
            let mid = (left + right) * 0.5;
            let side = (left - right) * 0.5 * self.width;
            out.samples[i] = mid + side;
            out.samples[per_ch + i] = mid - side;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_one_is_passthrough() {
        let mut node = StereoWidenerNode::new(1.0, 2);
        let inp = vec![AudioFrame::from_planar(
            2,
            48000,
            vec![0.8, -0.3, 0.5, 0.2, 0.4, -0.1],
        )];
        let mut out = vec![AudioFrame::from_planar(2, 48000, vec![0.0; 6])];
        let mut ctx = ProcessContext::new(6, 0, 48000);
        node.process(&mut ctx, &inp, &mut out);
        let s = &out[0].samples;
        // Planar: [L0,L1,L2, R0,R1,R2]
        for i in 0..3 {
            assert!((s[i] - inp[0].samples[i]).abs() < 1e-6, "left[{i}]");
            assert!(
                (s[3 + i] - inp[0].samples[3 + i]).abs() < 1e-6,
                "right[{i}]"
            );
        }
    }

    #[test]
    fn width_zero_is_mono() {
        let mut node = StereoWidenerNode::new(0.0, 2);
        // Planar: L=[0.8, -0.2], R=[0.2, 0.4] → mid=[0.5, 0.1], side=0
        let inp = vec![AudioFrame::from_planar(2, 48000, vec![0.8, -0.2, 0.2, 0.4])];
        let mut out = vec![AudioFrame::from_planar(2, 48000, vec![0.0; 4])];
        let mut ctx = ProcessContext::new(4, 0, 48000);
        node.process(&mut ctx, &inp, &mut out);
        let s = &out[0].samples;
        assert!((s[0] - 0.5).abs() < 1e-6);
        assert!((s[1] - 0.1).abs() < 1e-6);
        assert!((s[2] - 0.5).abs() < 1e-6);
        assert!((s[3] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn width_two_doubles_side() {
        let mut node = StereoWidenerNode::new(2.0, 2);
        // Planar: L=[0.7], R=[0.3] → mid=0.5, side=(0.7-0.3)/2*2=0.4
        // newL = 0.5 + 0.4 = 0.9, newR = 0.5 - 0.4 = 0.1
        let inp = vec![AudioFrame::from_planar(2, 48000, vec![0.7, 0.3])];
        let mut out = vec![AudioFrame::from_planar(2, 48000, vec![0.0; 2])];
        let mut ctx = ProcessContext::new(2, 0, 48000);
        node.process(&mut ctx, &inp, &mut out);
        let s = &out[0].samples;
        assert!((s[0] - 0.9).abs() < 1e-6, "newLeft = {}", s[0]);
        assert!((s[1] - 0.1).abs() < 1e-6, "newRight = {}", s[1]);
        // L-R diff: original 0.4, output 0.8 → doubled.
        assert!(((s[0] - s[1]) - 2.0 * (0.7 - 0.3)).abs() < 1e-6);
    }

    #[test]
    fn mono_passthrough() {
        let mut node = StereoWidenerNode::new(1.5, 1);
        let inp = vec![AudioFrame::from_planar(1, 48000, vec![0.3, -0.5, 0.8])];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 3])];
        let mut ctx = ProcessContext::new(3, 0, 48000);
        node.process(&mut ctx, &inp, &mut out);
        assert_eq!(out[0].samples, vec![0.3, -0.5, 0.8]);
    }
}
