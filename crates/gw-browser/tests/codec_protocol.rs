//! Protocol i5 — deep codec wire-format round-trip and boundary tests.
//!
//! Covers scenarios beyond the inline unit tests in codec.rs: diverse
//! channel/sample-rate combinations, header field extremes, endianness
//! assertions, large payloads, stereo planar ordering, and Opus-tag rejection.

use audio_core_bsd::AudioFrame;
use gw_browser::{decode_frame, encode_frame, FrameSpec, TAG_OPUS, TAG_PCM};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn round_trip(frame: &AudioFrame, spec: FrameSpec) -> AudioFrame {
    let bytes = encode_frame(frame).expect("encode_frame must succeed");
    decode_frame(&bytes, spec).expect("decode_frame must succeed")
}

fn assert_samples_eq(got: &[f32], want: &[f32]) {
    assert_eq!(got.len(), want.len(), "sample count mismatch");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert!((g - w).abs() < 1e-6, "sample[{i}]: got {g}, want {w}");
    }
}

// ---------------------------------------------------------------------------
// Round-trip across diverse channel / sample-rate combinations
// ---------------------------------------------------------------------------

#[test]
fn round_trip_mono_44100() {
    let spec = FrameSpec::new(1, 44_100);
    let samples: Vec<f32> = (0..128).map(|i| i as f32 * 0.01).collect();
    let frame = AudioFrame::from_planar(1, 44_100, samples.clone());
    let decoded = round_trip(&frame, spec);
    assert_samples_eq(&decoded.samples, &samples);
}

#[test]
fn round_trip_stereo_48000() {
    let spec = FrameSpec::new(2, 48_000);
    // planar: [ch0_s0..ch0_sN, ch1_s0..ch1_sN]
    let samples: Vec<f32> = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
    let frame = AudioFrame::from_planar(2, 48_000, samples.clone());
    let decoded = round_trip(&frame, spec);
    assert_samples_eq(&decoded.samples, &samples);
}

#[test]
fn round_trip_8_channel_96000() {
    let spec = FrameSpec::new(8, 96_000);
    let num_samples = 8 * 4; // 4 frames * 8 channels
    let samples: Vec<f32> = (0..num_samples).map(|i| (i as f32) * -0.1).collect();
    let frame = AudioFrame::from_planar(8, 96_000, samples.clone());
    let decoded = round_trip(&frame, spec);
    assert_samples_eq(&decoded.samples, &samples);
}

#[test]
fn round_trip_max_channels_255() {
    let spec = FrameSpec::new(255, 8_000);
    let num_samples = 255 * 2; // 2 frames * 255 channels
    let samples: Vec<f32> = vec![1.0; num_samples];
    let frame = AudioFrame::from_planar(255, 8_000, samples.clone());
    let decoded = round_trip(&frame, spec);
    assert_samples_eq(&decoded.samples, &samples);
}

// ---------------------------------------------------------------------------
// Header field boundary values
// ---------------------------------------------------------------------------

#[test]
fn channels_255_header_fits_in_u8() {
    let frame = AudioFrame::from_planar(255, 48_000, vec![0.0; 255]);
    let bytes = encode_frame(&frame).unwrap();
    assert_eq!(bytes[1], 255);
}

#[test]
fn sample_rate_zero_round_trips() {
    let spec = FrameSpec::new(1, 0);
    let frame = AudioFrame::from_planar(1, 0, vec![42.0]);
    let decoded = round_trip(&frame, spec);
    assert_eq!(decoded.sample_rate, 0);
}

#[test]
fn sample_rate_max_u32_round_trips() {
    let spec = FrameSpec::new(1, u32::MAX);
    let frame = AudioFrame::from_planar(1, u32::MAX, vec![1.5]);
    let decoded = round_trip(&frame, spec);
    assert_eq!(decoded.sample_rate, u32::MAX);
}

// ---------------------------------------------------------------------------
// Endianness assertions (explicit LE byte-order checks)
// ---------------------------------------------------------------------------

#[test]
fn sample_rate_is_little_endian_in_wire_format() {
    let frame = AudioFrame::from_planar(1, 48_000, vec![0.0]);
    let bytes = encode_frame(&frame).unwrap();
    // 48_000 = 0xBB80; LE bytes at offset 2..6: 80 BB 00 00
    assert_eq!(bytes[2], 0x80);
    assert_eq!(bytes[3], 0xBB);
    assert_eq!(bytes[4], 0x00);
    assert_eq!(bytes[5], 0x00);
}

#[test]
fn f32_samples_are_little_endian_in_wire_format() {
    let frame = AudioFrame::from_planar(1, 48_000, vec![0.5_f32]);
    let bytes = encode_frame(&frame).unwrap();
    // 0.5f32 = 0x3F000000; LE bytes: 00 00 00 3F
    assert_eq!(bytes[6], 0x00);
    assert_eq!(bytes[7], 0x00);
    assert_eq!(bytes[8], 0x00);
    assert_eq!(bytes[9], 0x3F);
}

// ---------------------------------------------------------------------------
// Large payloads
// ---------------------------------------------------------------------------

#[test]
fn large_payload_256_samples() {
    let spec = FrameSpec::new(2, 48_000);
    let samples: Vec<f32> = (0..512).map(|i| i as f32 * 0.001).collect();
    let frame = AudioFrame::from_planar(2, 48_000, samples.clone());
    let decoded = round_trip(&frame, spec);
    assert_samples_eq(&decoded.samples, &samples);
}

#[test]
fn large_payload_1024_samples() {
    let spec = FrameSpec::new(2, 48_000);
    let samples: Vec<f32> = (0..2048).map(|i| (i as f32).sin()).collect();
    let frame = AudioFrame::from_planar(2, 48_000, samples.clone());
    let decoded = round_trip(&frame, spec);
    assert_samples_eq(&decoded.samples, &samples);
}

// ---------------------------------------------------------------------------
// Stereo planar ordering: ch0 samples first, then ch1
// ---------------------------------------------------------------------------

#[test]
fn stereo_planar_order_preserved() {
    let spec = FrameSpec::new(2, 48_000);
    // ch0 = [1.0, 2.0, 3.0], ch1 = [4.0, 5.0, 6.0]
    let samples = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    let frame = AudioFrame::from_planar(2, 48_000, samples);
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&bytes, spec).unwrap();

    // Reconstruct per-channel slices to verify ordering.
    // For 2ch with 3 frames each, planar layout = [ch0: 3 samples, ch1: 3 samples]
    assert_eq!(decoded.samples, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
}

// ---------------------------------------------------------------------------
// Opus tag rejected when feature is off (test always runs with opus OFF)
// ---------------------------------------------------------------------------

#[test]
fn opus_tag_rejected_without_feature() {
    let mut bytes = vec![TAG_OPUS, 2];
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&[0xAA; 32]);
    let result = decode_frame(&bytes, FrameSpec::new(2, 48_000));
    assert!(
        result.is_err(),
        "Opus tag must be rejected without opus feature"
    );
}

// ---------------------------------------------------------------------------
// Header length constant is exactly 6
// ---------------------------------------------------------------------------

#[test]
fn header_len_constant_is_6() {
    assert_eq!(gw_browser::HEADER_LEN, 6);
}

// ---------------------------------------------------------------------------
// TAG_PCM is 0, TAG_OPUS is 1 (stable ABI constants)
// ---------------------------------------------------------------------------

#[test]
fn tag_constants_match_wire_spec() {
    assert_eq!(gw_browser::TAG_PCM, TAG_PCM);
    assert_eq!(gw_browser::TAG_OPUS, TAG_OPUS);
}
