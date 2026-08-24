//! 1-in / 2-out aux send splitter — mimics a mixing console's aux send.
//!
//! The input is distributed to two outputs:
//! - **Output 0 (main):** dry passthrough (input copied unchanged).
//! - **Output 1 (aux):** input scaled by `send_level` (0.0..1.0).
//!
//! This enables parallel routing chains in the graph — e.g. the main output
//! feeds a dry mixer while the aux output feeds a reverb that returns on a
//! separate mixer input. The node is fully stateless: `process` is a pure
//! function of `send_level` and the input samples.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 1-in / 2-out aux send splitter.
///
/// Output 0 = main (dry passthrough), Output 1 = aux (scaled by `send_level`).
pub struct AuxSendNode {
    in_port: [PortDescriptor; 1],
    out_ports: [PortDescriptor; 2],
    send_level: f32,
}

impl AuxSendNode {
    /// Create an aux send node.
    ///
    /// `send_level` is clamped to `0.0..=1.0` and determines the aux output
    /// gain (0.0 = muted aux, 1.0 = aux identical to main). `channels` sets
    /// the port channel count (1 = mono, 2 = stereo).
    #[must_use]
    pub fn new(send_level: f32, channels: u16) -> Self {
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_ports: [
                PortDescriptor::output(channels, SampleFormat::F32),
                PortDescriptor::output(channels, SampleFormat::F32),
            ],
            send_level: send_level.clamp(0.0, 1.0),
        }
    }
}

impl AudioNode for AuxSendNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.in_port
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_ports
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        in_frames: &[AudioFrame],
        out_frames: &mut [AudioFrame],
    ) {
        let Some(inp) = in_frames.first() else {
            return;
        };
        // Output 0: dry passthrough.
        if let Some(main) = out_frames.get_mut(0) {
            let m = inp.samples.len().min(main.samples.len());
            main.samples[..m].copy_from_slice(&inp.samples[..m]);
            main.channels = inp.channels;
            main.sample_rate = inp.sample_rate;
        }
        // Output 1: scaled aux.
        if let Some(aux) = out_frames.get_mut(1) {
            let m = inp.samples.len().min(aux.samples.len());
            for (dst, &src) in aux.samples[..m].iter_mut().zip(inp.samples[..m].iter()) {
                *dst = src * self.send_level;
            }
            aux.channels = inp.channels;
            aux.sample_rate = inp.sample_rate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_zeros(n: usize, ch: u16) -> Vec<AudioFrame> {
        vec![
            AudioFrame::from_planar(ch, 48_000, vec![0.0; n]),
            AudioFrame::from_planar(ch, 48_000, vec![0.0; n]),
        ]
    }

    #[test]
    fn main_is_passthrough() {
        let mut node = AuxSendNode::new(0.5, 1);
        let inp = vec![AudioFrame::from_planar(1, 48_000, vec![0.5, -0.3, 0.8])];
        let mut out = two_zeros(3, 1);
        let mut ctx = ProcessContext::new(3, 0, 48_000);
        node.process(&mut ctx, &inp, &mut out);
        assert_eq!(out[0].samples, vec![0.5, -0.3, 0.8]);
    }

    #[test]
    fn aux_is_scaled() {
        let mut node = AuxSendNode::new(0.5, 1);
        let inp = vec![AudioFrame::from_planar(1, 48_000, vec![0.8, 0.4])];
        let mut out = two_zeros(2, 1);
        let mut ctx = ProcessContext::new(2, 0, 48_000);
        node.process(&mut ctx, &inp, &mut out);
        assert!((out[1].samples[0] - 0.4).abs() < 1e-6);
        assert!((out[1].samples[1] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn zero_send_mutes_aux() {
        let mut node = AuxSendNode::new(0.0, 1);
        let inp = vec![AudioFrame::from_planar(1, 48_000, vec![0.7, -0.5])];
        let mut out = two_zeros(2, 1);
        let mut ctx = ProcessContext::new(2, 0, 48_000);
        node.process(&mut ctx, &inp, &mut out);
        // Aux is fully muted.
        assert_eq!(out[1].samples, vec![0.0, 0.0]);
        // Main is unaffected.
        assert_eq!(out[0].samples, vec![0.7, -0.5]);
    }

    #[test]
    fn full_send_equals_main() {
        let mut node = AuxSendNode::new(1.0, 1);
        let inp = vec![AudioFrame::from_planar(1, 48_000, vec![0.6, -0.2, 0.9])];
        let mut out = two_zeros(3, 1);
        let mut ctx = ProcessContext::new(3, 0, 48_000);
        node.process(&mut ctx, &inp, &mut out);
        assert_eq!(out[0].samples, out[1].samples);
    }

    #[test]
    fn two_output_ports() {
        let node = AuxSendNode::new(0.5, 2);
        assert_eq!(node.outputs().len(), 2);
        assert_eq!(node.inputs().len(), 1);
    }
}
