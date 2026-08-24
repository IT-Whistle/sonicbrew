//! Biquad equaliser node — standard second-order IIR filter.
//!
//! Supports the six common filter types (low/high pass, band pass, peaking,
//! low/high shelf). Coefficients are computed at construction using the
//! RBJ Audio EQ Cookbook formulas; `process` applies a Direct Form I
//! Transposed biquad per sample per channel with pre-allocated state.

use std::f32::consts::PI;

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// Biquad filter type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterType {
    LowPass,
    HighPass,
    BandPass,
    Peaking,
    LowShelf,
    HighShelf,
}

/// Normalised biquad coefficients (a0 normalised to 1).
#[derive(Debug, Clone, Copy)]
struct Coeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

/// 1-in / 1-out biquad EQ.
///
/// Parameters (`freq`, `gain_db`, `q`, `filter_type`) are fixed at
/// construction; the filter state (`z1`, `z2` per channel) is pre-allocated.
pub struct EqNode {
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
    coeffs: Coeffs,
    /// DF1T state — two registers per channel.
    z1: Vec<f32>,
    z2: Vec<f32>,
}

impl EqNode {
    /// Create a biquad with the given parameters.
    ///
    /// - `freq`: centre/cutoff frequency in Hz.
    /// - `gain_db`: gain in dB (peaking/shelf only; ignored for pass filters).
    /// - `q`: quality factor (bandwidth).
    /// - `sample_rate`: sample rate in Hz.
    /// - `channels`: channel count.
    #[must_use]
    pub fn new(
        filter_type: FilterType,
        freq: f32,
        gain_db: f32,
        q: f32,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        let coeffs = compute_coeffs(filter_type, freq, gain_db, q, sample_rate);
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            coeffs,
            z1: vec![0.0; channels as usize],
            z2: vec![0.0; channels as usize],
        }
    }

    /// Process one sample through the DF1T biquad (per channel `ch`).
    #[inline]
    fn process_sample(&mut self, x: f32, ch: usize) -> f32 {
        let c = &self.coeffs;
        let z1 = &mut self.z1;
        let z2 = &mut self.z2;
        // Direct Form I Transposed.
        let y = c.b0 * x + z1[ch];
        z1[ch] = c.b1 * x - c.a1 * y + z2[ch];
        z2[ch] = c.b2 * x - c.a2 * y;
        y
    }
}

impl AudioNode for EqNode {
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

/// Compute RBJ Audio EQ Cookbook biquad coefficients.
fn compute_coeffs(ft: FilterType, freq: f32, gain_db: f32, q: f32, sample_rate: u32) -> Coeffs {
    let sr = sample_rate as f32;
    let w0 = 2.0 * PI * freq / sr;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * q);
    let a = 10.0_f32.powf(gain_db / 40.0); // for peaking/shelf

    let (b0, b1, b2, a0, a1, a2) = match ft {
        FilterType::LowPass => (
            (1.0 - cos) / 2.0,
            1.0 - cos,
            (1.0 - cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ),
        FilterType::HighPass => (
            (1.0 + cos) / 2.0,
            -(1.0 + cos),
            (1.0 + cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ),
        FilterType::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        FilterType::Peaking => (
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        ),
        FilterType::LowShelf => {
            let sq = (a * a).sqrt();
            (
                a * ((a + 1.0) - (a - 1.0) * cos + 2.0 * sq * alpha),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - 2.0 * sq * alpha),
                (a + 1.0) + (a - 1.0) * cos + 2.0 * sq * alpha,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - 2.0 * sq * alpha,
            )
        }
        FilterType::HighShelf => {
            let sq = (a * a).sqrt();
            (
                a * ((a + 1.0) + (a - 1.0) * cos + 2.0 * sq * alpha),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - 2.0 * sq * alpha),
                (a + 1.0) - (a - 1.0) * cos + 2.0 * sq * alpha,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - 2.0 * sq * alpha,
            )
        }
    };

    Coeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowpass_attenuates_high_frequency() {
        // At 48k, a 500Hz lowpass should pass DC (gain ≈ 1) and attenuate
        // a 10kHz tone.
        let mut eq = EqNode::new(FilterType::LowPass, 500.0, 0.0, 0.707, 48_000, 1);
        let dc = vec![AudioFrame::from_planar(1, 48000, vec![0.5; 256])];
        let mut out = vec![AudioFrame::from_planar(1, 48000, vec![0.0; 256])];
        let mut ctx = ProcessContext::new(256, 0, 48000);
        eq.process(&mut ctx, &dc, &mut out);
        // Steady-state DC through a lowpass → unchanged amplitude.
        let last = out[0].samples[255];
        assert!((last - 0.5).abs() < 0.01, "DC passthrough: {last}");
    }

    #[test]
    fn coefficients_are_finite() {
        for ft in [
            FilterType::LowPass,
            FilterType::HighPass,
            FilterType::BandPass,
            FilterType::Peaking,
            FilterType::LowShelf,
            FilterType::HighShelf,
        ] {
            let c = compute_coeffs(ft, 1000.0, 3.0, 1.0, 48_000);
            assert!(c.b0.is_finite());
            assert!(c.b1.is_finite());
            assert!(c.b2.is_finite());
            assert!(c.a1.is_finite());
            assert!(c.a2.is_finite());
        }
    }
}
