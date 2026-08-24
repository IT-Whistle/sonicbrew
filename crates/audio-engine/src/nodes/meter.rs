//! Metering node — passthrough that measures peak and RMS via RT-safe atomics.
//!
//! The node passes audio through unchanged while updating `AtomicU32` peak/RMS
//! values (f32 bit-cast through `to_bits`/`from_bits`). A control-thread
//! reader calls [`MeterNode::snapshot`] to read the current levels and reset
//! the peak hold.

use std::sync::atomic::{AtomicU32, Ordering};

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// A point-in-time level reading.
#[derive(Debug, Clone, Copy, Default)]
pub struct Levels {
    /// Peak absolute sample value since the last snapshot.
    pub peak: f32,
    /// RMS (root-mean-square) of the last cycle.
    pub rms: f32,
}

/// 1-in / 1-out passthrough meter. Updates atomics during `process`; readers
/// call `snapshot` from the control thread.
pub struct MeterNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    /// Bit-cast f32 peak — stored as AtomicU32 for wait-free RT updates.
    peak: AtomicU32,
    /// Bit-cast f32 rms.
    rms: AtomicU32,
}

impl MeterNode {
    #[must_use]
    pub fn new(channels: u16) -> Self {
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            peak: AtomicU32::new(0.0_f32.to_bits()),
            rms: AtomicU32::new(0.0_f32.to_bits()),
        }
    }

    /// Read the current peak/RMS and reset peak to zero (peak-hold reset).
    /// Safe to call from the control thread.
    pub fn snapshot(&self) -> Levels {
        let peak = f32::from_bits(self.peak.swap(0.0_f32.to_bits(), Ordering::Relaxed));
        let rms = f32::from_bits(self.rms.load(Ordering::Relaxed));
        Levels { peak, rms }
    }
}

impl AudioNode for MeterNode {
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
        if n == 0 {
            return;
        }
        // Passthrough.
        out.samples[..n].copy_from_slice(&inp.samples[..n]);

        // Measure peak + RMS.
        let mut peak = 0.0_f32;
        let mut sum_sq = 0.0_f32;
        for i in 0..n {
            let abs = inp.samples[i].abs();
            if abs > peak {
                peak = abs;
            }
            sum_sq += inp.samples[i] * inp.samples[i];
        }
        let rms = (sum_sq / n as f32).sqrt();

        // Update atomics (wait-free).
        // Peak is max(previous, new) — reload, compare, store.
        loop {
            let old = self.peak.load(Ordering::Relaxed);
            let old_f = f32::from_bits(old);
            if peak <= old_f {
                break;
            }
            if self
                .peak
                .compare_exchange_weak(old, peak.to_bits(), Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        self.rms.store(rms.to_bits(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_and_measure() {
        let mut meter = MeterNode::new(1);
        let inp = vec![AudioFrame::from_planar(
            1,
            48000,
            vec![0.3, -0.6, 0.9, -0.2],
        )];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 4])];
        let mut ctx = ProcessContext::new(4, 0, 48000);
        meter.process(&mut ctx, &inp, &mut out);
        // Passthrough.
        assert_eq!(out[0].samples, vec![0.3, -0.6, 0.9, -0.2]);
        // Peak = 0.9.
        let levels = meter.snapshot();
        assert!((levels.peak - 0.9).abs() < 1e-6, "peak {}", levels.peak);
        // RMS ≈ sqrt((0.09+0.36+0.81+0.04)/4) = sqrt(0.325) ≈ 0.570.
        assert!((levels.rms - 0.5701).abs() < 0.01, "rms {}", levels.rms);
        // Snapshot resets peak.
        let after = meter.snapshot();
        assert!(after.peak < 1e-9, "peak reset {}", after.peak);
    }
}
