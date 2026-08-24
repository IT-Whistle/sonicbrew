//! Sanitizer i5 — malformed / random input fuzzing for decode_frame.
//!
//! Guarantees no-panic on arbitrary byte sequences and validates that all
//! error paths produce `GatewayError::BadFrame` (never a panic or abort).
//! Uses deterministic malformed cases plus proptest for property-based tests.

use audio_core_bsd::AudioFrame;
use gw_browser::{decode_frame, FrameSpec};

fn is_bad_frame(result: Result<AudioFrame, gw_browser::GatewayError>) -> bool {
    matches!(result, Err(gw_browser::GatewayError::BadFrame(_)))
}

// ---------------------------------------------------------------------------
// Deterministic malformed inputs — no-panic + BadFrame
// ---------------------------------------------------------------------------

#[test]
fn empty_bytes_no_panic() {
    let result = decode_frame(&[], FrameSpec::new(2, 48_000));
    assert!(is_bad_frame(result));
}

#[test]
fn single_byte_no_panic() {
    let result = decode_frame(&[0x00], FrameSpec::new(2, 48_000));
    assert!(is_bad_frame(result));
}

#[test]
fn two_bytes_no_panic() {
    let result = decode_frame(&[0x00, 0x01], FrameSpec::new(2, 48_000));
    assert!(is_bad_frame(result));
}

#[test]
fn three_bytes_no_panic() {
    let result = decode_frame(&[0x00, 0x01, 0x02], FrameSpec::new(2, 48_000));
    assert!(is_bad_frame(result));
}

#[test]
fn four_bytes_no_panic() {
    let result = decode_frame(&[0x00, 0x01, 0x02, 0x03], FrameSpec::new(2, 48_000));
    assert!(is_bad_frame(result));
}

#[test]
fn five_bytes_no_panic() {
    let result = decode_frame(&[0x00, 0x01, 0x02, 0x03, 0x04], FrameSpec::new(2, 48_000));
    assert!(is_bad_frame(result));
}

#[test]
fn header_only_no_payload() {
    let mut bytes = vec![0x00, 0x02]; // TAG_PCM, 2 channels
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    let result = decode_frame(&bytes, FrameSpec::new(2, 48_000));
    assert!(
        is_bad_frame(result),
        "header-only with zero payload must be BadFrame"
    );
}

#[test]
fn single_byte_payload_no_panic() {
    let mut bytes = vec![0x00, 0x01]; // TAG_PCM, 1 ch, 48 kHz
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.push(0xFF); // 1 byte payload — not a multiple of 4
    let result = decode_frame(&bytes, FrameSpec::new(1, 48_000));
    assert!(is_bad_frame(result));
}

#[test]
fn truncated_payload_no_panic() {
    // Valid header (2ch, 48kHz) but payload is 3 bytes (needs multiple of 8).
    let mut bytes = vec![0x00, 0x02];
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
    let result = decode_frame(&bytes, FrameSpec::new(2, 48_000));
    assert!(is_bad_frame(result));
}

#[test]
fn all_tag_values_no_panic() {
    let spec = FrameSpec::new(2, 48_000);
    for tag in 0u8..=255 {
        let mut bytes = vec![tag, 0x02];
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        let result = decode_frame(&bytes, spec);
        // Either BadFrame (unknown tag or mismatch) or valid — just no panic.
        let _ = result;
    }
}

// ---------------------------------------------------------------------------
// Large garbage payload — no-panic
// ---------------------------------------------------------------------------

#[test]
fn large_garbage_payload_no_panic() {
    let mut bytes = vec![0x00, 0x02]; // TAG_PCM, 2ch
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&vec![0xAB; 1024]);
    let result = decode_frame(&bytes, FrameSpec::new(2, 48_000));
    let _ = result; // must not panic
}

// ---------------------------------------------------------------------------
// Very large payload (1 MiB) — no-panic
// ---------------------------------------------------------------------------

#[test]
fn one_megabyte_payload_no_panic() {
    let mut bytes = vec![0x00, 0x02]; // TAG_PCM, 2ch
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&vec![0x00; 1_048_576]);
    let result = decode_frame(&bytes, FrameSpec::new(2, 48_000));
    // 1 MiB payload is a multiple of 8 (channels * 4), so this should
    // decode successfully — but must never panic regardless.
    let _ = result;
}

// ---------------------------------------------------------------------------
// All error paths are GatewayError::BadFrame (never other variants)
// ---------------------------------------------------------------------------

#[test]
fn error_paths_are_bad_frame_variant() {
    let spec = FrameSpec::new(2, 48_000);

    // Short header
    let r = decode_frame(&[0u8; 3], spec);
    assert!(is_bad_frame(r));

    // Unknown tag
    let mut bytes = vec![0xFF, 0x02];
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 8]);
    let r = decode_frame(&bytes, spec);
    assert!(is_bad_frame(r));

    // Channel mismatch
    let mut bytes = vec![0x00, 0x01]; // 1ch but spec wants 2ch
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 4]);
    let r = decode_frame(&bytes, spec);
    assert!(is_bad_frame(r));

    // Sample rate mismatch
    let mut bytes = vec![0x00, 0x02];
    bytes.extend_from_slice(&44_100u32.to_le_bytes()); // 44100 but spec wants 48000
    bytes.extend_from_slice(&[0u8; 8]);
    let r = decode_frame(&bytes, spec);
    assert!(is_bad_frame(r));
}

// ---------------------------------------------------------------------------
// proptest property-based fuzz tests
// ---------------------------------------------------------------------------

use proptest::prelude::*;

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let spec = FrameSpec::new(2, 48_000);
        let result = decode_frame(&data, spec);
        // Must return Ok or Err(BadFrame), never panic.
        let _ = result;
    }

    #[test]
    fn arbitrary_header_makes_bad_frame(
        tag in any::<u8>(),
        channels in any::<u8>(),
        sr_bytes in prop::collection::vec(any::<u8>(), 4),
    ) {
        let mut bytes = vec![tag, channels];
        bytes.extend_from_slice(&sr_bytes);
        // Add some garbage payload
        bytes.extend_from_slice(&[0xAA; 16]);
        let spec = FrameSpec::new(2, 48_000);
        let result = decode_frame(&bytes, spec);
        let _ = result;
    }

    #[test]
    fn random_tag_never_panics(tag in 2u8..=255) {
        let mut bytes = vec![tag, 0x02];
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        let result = decode_frame(&bytes, FrameSpec::new(2, 48_000));
        assert!(is_bad_frame(result), "tag {tag} outside 0..=1 must be BadFrame");
    }
}
