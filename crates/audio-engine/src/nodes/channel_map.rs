//! Channel routing node — swap, mute, pan, mono↔stereo within a frame.
//!
//! Works within the graph's fixed channel count (set by `GraphConfig`).
//! For stereo (2-ch) frames the node can: swap L/R, mute one channel, apply
//! equal-power panning, or duplicate mono content to both channels.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// Channel-routing mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChannelMode {
    /// Pass through unchanged.
    Passthrough,
    /// Swap left and right (stereo only).
    Swap,
    /// Mute the left channel.
    MuteLeft,
    /// Mute the right channel.
    MuteRight,
    /// Equal-power pan: `pan` in [-1, +1] (left ↔ right).
    /// Uses constant-power law: `l = cos((pan+1)*π/4)`, `r = sin((pan+1)*π/4)`.
    Pan(f32),
    /// Upmix mono→stereo: copy ch0 to both channels.
    MonoToStereo,
    /// Downmix stereo→mono: average L and R into ch0, mute ch1.
    StereoToMono,
}

/// 1-in / 1-out node that routes channels within the frame according to a
/// [`ChannelMode`]. The mode can be changed on the control thread between
/// cycles via [`ChannelMapNode::set_mode`].
pub struct ChannelMapNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    mode: ChannelMode,
}

impl ChannelMapNode {
    #[must_use]
    pub fn new(channels: u16, mode: ChannelMode) -> Self {
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            mode,
        }
    }

    /// Change the routing mode (control thread).
    pub fn set_mode(&mut self, mode: ChannelMode) {
        self.mode = mode;
    }
}

impl AudioNode for ChannelMapNode {
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
        if ch == 0 || n == 0 {
            return;
        }
        let frames = n / ch;

        match self.mode {
            ChannelMode::Passthrough => {}
            ChannelMode::Swap if ch >= 2 => {
                for f in 0..frames {
                    out.samples.swap(f * ch, f * ch + 1);
                }
            }
            ChannelMode::MuteLeft if ch >= 2 => {
                for f in 0..frames {
                    out.samples[f * ch] = 0.0;
                }
            }
            ChannelMode::MuteRight if ch >= 2 => {
                for f in 0..frames {
                    out.samples[f * ch + 1] = 0.0;
                }
            }
            ChannelMode::Pan(pan) if ch >= 2 => {
                let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
                let (s, c) = angle.sin_cos();
                for f in 0..frames {
                    let l = out.samples[f * ch];
                    let r = out.samples[f * ch + 1];
                    out.samples[f * ch] = l * c;
                    out.samples[f * ch + 1] = r * s;
                }
            }
            ChannelMode::MonoToStereo if ch >= 2 => {
                for f in 0..frames {
                    let m = out.samples[f * ch];
                    out.samples[f * ch + 1] = m;
                }
            }
            ChannelMode::StereoToMono if ch >= 2 => {
                for f in 0..frames {
                    let mid = (out.samples[f * ch] + out.samples[f * ch + 1]) * 0.5;
                    out.samples[f * ch] = mid;
                    out.samples[f * ch + 1] = mid;
                }
            }
            // Fallback for modes that don't match the channel count.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_exchanges_channels() {
        let mut node = ChannelMapNode::new(2, ChannelMode::Swap);
        let inp = vec![AudioFrame::from_planar(2, 48000, vec![0.1, 0.2, 0.3, 0.4])];
        let mut out = vec![AudioFrame::from_planar(2, 48000, vec![0.0; 4])];
        let mut ctx = ProcessContext::new(2, 0, 48000);
        node.process(&mut ctx, &inp, &mut out);
        assert_eq!(out[0].samples, vec![0.2, 0.1, 0.4, 0.3]);
    }

    #[test]
    fn stereo_to_mono_averages() {
        let mut node = ChannelMapNode::new(2, ChannelMode::StereoToMono);
        let inp = vec![AudioFrame::from_planar(2, 48000, vec![0.8, 0.2, 0.6, 0.4])];
        let mut out = vec![AudioFrame::from_planar(2, 48000, vec![0.0; 4])];
        let mut ctx = ProcessContext::new(2, 0, 48000);
        node.process(&mut ctx, &inp, &mut out);
        assert!((out[0].samples[0] - 0.5).abs() < 1e-6);
        assert!((out[0].samples[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn passthrough_preserves_samples() {
        let mut node = ChannelMapNode::new(1, ChannelMode::Passthrough);
        let inp = vec![AudioFrame::from_planar(1, 48000, vec![0.1, -0.2, 0.3])];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 3])];
        let mut ctx = ProcessContext::new(3, 0, 48000);
        node.process(&mut ctx, &inp, &mut out);
        assert_eq!(out[0].samples, vec![0.1, -0.2, 0.3]);
    }
}
