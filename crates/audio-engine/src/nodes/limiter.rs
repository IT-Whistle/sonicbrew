//! Brick-wall limiter — prevents any sample from exceeding a threshold.
//!
//! A simple, zero-latency limiter: any sample whose absolute value exceeds the
//! threshold is scaled to exactly the threshold. This is the simplest
//! RT-safe limiter (no lookahead, no envelope) suitable for preventing
//! clipping. For transparent limiting use [`CompressorNode`](super::CompressorNode)
//! with a high ratio first, then this as a safety net.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 1-in / 1-out brick-wall limiter.
pub struct LimiterNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    threshold: f32,
}

impl LimiterNode {
    /// Create a limiter at `threshold_db` (negative dB, e.g. -1.0).
    #[must_use]
    pub fn new(threshold_db: f32, channels: u16) -> Self {
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            threshold: 10.0_f32.powf(threshold_db / 20.0),
        }
    }
}

impl AudioNode for LimiterNode {
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
        for i in 0..n {
            let s = inp.samples[i];
            out.samples[i] = if s.abs() > self.threshold {
                self.threshold * s.signum()
            } else {
                s
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_overshoot() {
        let mut lim = LimiterNode::new(-3.0, 1); // ≈ 0.708
        let inp = vec![AudioFrame::from_planar(
            1,
            48000,
            vec![0.9, -0.95, 0.5, -0.3],
        )];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 4])];
        let mut ctx = ProcessContext::new(4, 0, 48000);
        lim.process(&mut ctx, &inp, &mut out);
        let thr = 10.0_f32.powf(-3.0 / 20.0);
        for &s in &out[0].samples {
            assert!(s.abs() <= thr + 1e-6, "{s} exceeds threshold {thr}");
        }
    }

    #[test]
    fn passthrough_below_threshold() {
        let mut lim = LimiterNode::new(-1.0, 1);
        let inp = vec![AudioFrame::from_planar(1, 48000, vec![0.1, -0.2, 0.3])];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 3])];
        let mut ctx = ProcessContext::new(3, 0, 48000);
        lim.process(&mut ctx, &inp, &mut out);
        assert_eq!(out[0].samples, vec![0.1, -0.2, 0.3]);
    }
}
