//! Integration tests: direct node chaining via `AudioNode::process` calls.
//!
//! Unlike the graph-based integration tests, these wire nodes by directly
//! invoking `process(&mut ctx, &[input], &mut [output])` — no `Graph`, no
//! scheduler, no `process_cycle`. This isolates the DSP node chain itself.

use audio_core_bsd::{AudioFrame, AudioNode, ProcessContext};
use audio_engine::nodes::{CompressorNode, EqNode, FilterType, LimiterNode, MeterNode};

const NF: usize = 256;
const SR: u32 = 48_000;
const CH: u16 = 1;

fn sine(amp: f32) -> AudioFrame {
    AudioFrame::from_planar(
        1,
        SR,
        (0..NF)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SR as f32).sin() * amp)
            .collect(),
    )
}

fn silence() -> AudioFrame {
    AudioFrame::from_planar(1, SR, vec![0.0; NF])
}

fn peak(frame: &AudioFrame) -> f32 {
    frame
        .samples
        .iter()
        .copied()
        .map(f32::abs)
        .fold(0.0_f32, f32::max)
}

/// sine(0.8) → EqNode(Peaking 440 Hz +12 dB) → output.
///
/// Asserts the EQ transforms the signal: output peak differs from input peak
/// (a +12 dB peaking filter at the sine's exact frequency must change the
/// amplitude).
#[test]
fn eq_chain_changes_amplitude() {
    let input = vec![sine(0.8)];
    let mut eq = EqNode::new(FilterType::Peaking, 440.0, 12.0, 1.0, SR, CH);
    let mut output = vec![silence()];
    let mut ctx = ProcessContext::new(NF, 0, SR);
    eq.process(&mut ctx, &input, &mut output);

    let in_peak = peak(&input[0]);
    let out_peak = peak(&output[0]);
    assert!(
        (out_peak - in_peak).abs() > 0.01,
        "EQ did not change amplitude (in={in_peak:.4} out={out_peak:.4})"
    );
}

/// sine(0.9) → CompressorNode(-6 dB, 10:1) → output.
///
/// 0.9 ≈ -0.9 dB is well above -6 dB threshold, so compression engages.
/// Output peak must be strictly below 0.9.
#[test]
fn compressor_chain_reduces_peak() {
    let input = vec![sine(0.9)];
    let mut comp = CompressorNode::new(-6.0, 10.0, 0.1, 100.0, 0.0, SR, CH);
    let mut output = vec![silence()];
    let mut ctx = ProcessContext::new(NF, 0, SR);
    comp.process(&mut ctx, &input, &mut output);

    let out_peak = peak(&output[0]);
    assert!(
        out_peak < 0.9,
        "compressor did not reduce peak ({out_peak:.4} >= 0.9)"
    );
}

/// sine(0.95) → LimiterNode(-3 dB ≈ 0.708) → output.
///
/// Brick-wall limiter: every output sample must satisfy |value| ≤ threshold.
#[test]
fn limiter_chain_clamps() {
    let input = vec![sine(0.95)];
    let mut lim = LimiterNode::new(-3.0, CH);
    let mut output = vec![silence()];
    let mut ctx = ProcessContext::new(NF, 0, SR);
    lim.process(&mut ctx, &input, &mut output);

    let threshold = 10.0_f32.powf(-3.0 / 20.0);
    for &s in &output[0].samples {
        assert!(
            s.abs() <= threshold + 1e-6,
            "limiter let {s} past threshold {threshold:.4}"
        );
    }
}

/// sine(0.8) → Eq → Compressor → Limiter → Meter → output.
///
/// Full chain. Asserts all output samples are finite and the meter registered
/// a non-zero peak.
#[test]
fn full_chain_produces_finite_output() {
    let input = vec![sine(0.8)];

    let mut eq = EqNode::new(FilterType::Peaking, 440.0, 6.0, 1.0, SR, CH);
    let mut comp = CompressorNode::new(-12.0, 4.0, 1.0, 100.0, 0.0, SR, CH);
    let mut lim = LimiterNode::new(-1.0, CH);
    let mut meter = MeterNode::new(CH);

    let mut buf_a = vec![silence()];
    let mut buf_b = vec![silence()];
    let mut buf_c = vec![silence()];
    let mut buf_d = vec![silence()];

    let mut ctx = ProcessContext::new(NF, 0, SR);

    eq.process(&mut ctx, &input, &mut buf_a);
    comp.process(&mut ctx, &buf_a, &mut buf_b);
    lim.process(&mut ctx, &buf_b, &mut buf_c);
    meter.process(&mut ctx, &buf_c, &mut buf_d);

    for &s in &buf_d[0].samples {
        assert!(s.is_finite(), "non-finite sample in full chain: {s}");
    }

    let levels = meter.snapshot();
    assert!(
        levels.peak > 0.0,
        "meter registered zero peak after full chain"
    );
}
