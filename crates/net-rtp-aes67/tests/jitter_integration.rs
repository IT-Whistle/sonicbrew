//! Integration i4 — JitterBuffer + codec combined pipeline.
//!
//! Verifies that shuffled (and wrap-around) RTP packets, pushed through the
//! `JitterBuffer` and drained via `drain_jitter`, produce **in-order decoded**
//! frames and that loss is correctly skipped.

use net_rtp_aes67::{drain_jitter, encode_l16, AudioFrame, JitterBuffer, PushOutcome};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a mono L16 RTP payload whose first sample encodes `tag` (a small
/// integer) so we can verify emission order without comparing full buffers.
fn tagged_payload(tag: f32) -> Vec<u8> {
    // 4 mono samples at the given value — small enough to be cheap, large
    // enough to be a realistic L16 frame fragment.
    encode_l16(&[tag; 4], 1)
}

/// Decode the first sample of a frame to verify its tag.
fn first_sample(frame: &AudioFrame) -> f32 {
    frame.samples[0]
}

// ---------------------------------------------------------------------------
// Test: in-order packets → in-order decoded frames
// ---------------------------------------------------------------------------

#[test]
fn in_order_packets_decode_in_order() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(16);
    let channels = 1u16;
    let sample_rate = 48_000u32;

    // Push seq 0, 1, 2 in order.
    jitter.push(0, tagged_payload(0.1));
    jitter.push(1, tagged_payload(0.2));
    jitter.push(2, tagged_payload(0.3));

    let frames = drain_jitter(&mut jitter, channels, sample_rate);
    assert_eq!(frames.len(), 3);
    assert!((first_sample(&frames[0]) - 0.1).abs() < 1e-3);
    assert!((first_sample(&frames[1]) - 0.2).abs() < 1e-3);
    assert!((first_sample(&frames[2]) - 0.3).abs() < 1e-3);
    assert!(jitter.is_empty());
}

// ---------------------------------------------------------------------------
// Test: out-of-order → reordered decoded frames
// ---------------------------------------------------------------------------

#[test]
fn out_of_order_packets_decode_in_order() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(16);
    let channels = 1u16;
    let sample_rate = 48_000u32;

    // Push earliest first (establishes baseline), then later ones out of order.
    // The JitterBuffer reorders buffered packets; the first push must be the
    // lowest seq so that later out-of-order arrivals are buffered (not rejected
    // as duplicates behind the baseline).
    jitter.push(0, tagged_payload(0.1)); // baseline
    jitter.push(2, tagged_payload(0.3)); // ahead, buffered
    jitter.push(1, tagged_payload(0.2)); // ahead (between 0 and 2), buffered

    let frames = drain_jitter(&mut jitter, channels, sample_rate);
    assert_eq!(frames.len(), 3);
    assert!((first_sample(&frames[0]) - 0.1).abs() < 1e-3, "seq 0 first");
    assert!(
        (first_sample(&frames[1]) - 0.2).abs() < 1e-3,
        "seq 1 second"
    );
    assert!((first_sample(&frames[2]) - 0.3).abs() < 1e-3, "seq 2 third");
    assert!(jitter.is_empty());
}

// ---------------------------------------------------------------------------
// Test: loss gap → skipped (no frame produced for lost seq)
// ---------------------------------------------------------------------------

#[test]
fn loss_gap_skipped_no_phantom_frame() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(16);
    let channels = 1u16;
    let sample_rate = 48_000u32;

    // Push seq 0 and 2 (seq 1 is lost).
    jitter.push(0, tagged_payload(0.1));
    jitter.push(2, tagged_payload(0.3));

    let frames = drain_jitter(&mut jitter, channels, sample_rate);
    assert_eq!(frames.len(), 2, "only seq 0 and seq 2 emitted");
    assert!(
        (first_sample(&frames[0]) - 0.1).abs() < 1e-3,
        "frame 0 is seq 0"
    );
    assert!(
        (first_sample(&frames[1]) - 0.3).abs() < 1e-3,
        "frame 1 is seq 2"
    );

    // The lost seq 1 (which would decode to 0.2) must NOT appear.
    for f in &frames {
        assert!((first_sample(f) - 0.2).abs() >= 1e-3, "lost seq 1 absent");
    }
    assert!(jitter.is_empty());
}

// ---------------------------------------------------------------------------
// Test: sequence wrap-around (65534 → 65535 → 0)
// ---------------------------------------------------------------------------

#[test]
fn seq_wrap_around_decodes_in_order() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(16);
    let channels = 1u16;
    let sample_rate = 48_000u32;

    // Push across the u16 boundary: 65534, 65535, 0, 1.
    jitter.push(65534, tagged_payload(0.1));
    jitter.push(65535, tagged_payload(0.2));
    jitter.push(0, tagged_payload(0.3));
    jitter.push(1, tagged_payload(0.4));

    let frames = drain_jitter(&mut jitter, channels, sample_rate);
    assert_eq!(frames.len(), 4);
    assert!((first_sample(&frames[0]) - 0.1).abs() < 1e-3, "seq 65534");
    assert!((first_sample(&frames[1]) - 0.2).abs() < 1e-3, "seq 65535");
    assert!((first_sample(&frames[2]) - 0.3).abs() < 1e-3, "seq 0");
    assert!((first_sample(&frames[3]) - 0.4).abs() < 1e-3, "seq 1");
    assert!(jitter.is_empty());
}

// ---------------------------------------------------------------------------
// Test: wrap-around with loss at the boundary
// ---------------------------------------------------------------------------

#[test]
fn seq_wrap_with_loss_at_boundary() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(16);
    let channels = 1u16;
    let sample_rate = 48_000u32;

    // 65534 (baseline), 65535, then 1 (seq 0 lost).
    // Seq 0 would be behind the baseline if pushed after 65534 (forward_distance
    // 65534→0 = 2 > 0, so it's ahead). But we deliberately omit seq 0 to test loss.
    jitter.push(65534, tagged_payload(0.1)); // baseline
    jitter.push(65535, tagged_payload(0.2)); // +1 ahead, buffered
    jitter.push(1, tagged_payload(0.4)); // +3 ahead, buffered

    let frames = drain_jitter(&mut jitter, channels, sample_rate);
    // drain_jitter pops 65534, then 65535, then hits gap (no seq 0), skip_gap
    // jumps to seq 1. So all 3 buffered packets are emitted.
    assert_eq!(frames.len(), 3, "seqs 65534, 65535, 1 emitted");
    assert!((first_sample(&frames[0]) - 0.1).abs() < 1e-3, "seq 65534");
    assert!((first_sample(&frames[1]) - 0.2).abs() < 1e-3, "seq 65535");
    assert!((first_sample(&frames[2]) - 0.4).abs() < 1e-3, "seq 1");
    assert!(jitter.is_empty());
}

// ---------------------------------------------------------------------------
// Test: multiple gaps in one drain
// ---------------------------------------------------------------------------

#[test]
fn multiple_gaps_all_skipped() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(16);
    let channels = 1u16;
    let sample_rate = 48_000u32;

    // Push 0, 3, 5 (seqs 1, 2, 4 lost).
    jitter.push(0, tagged_payload(0.1));
    jitter.push(3, tagged_payload(0.4));
    jitter.push(5, tagged_payload(0.6));

    let frames = drain_jitter(&mut jitter, channels, sample_rate);
    assert_eq!(frames.len(), 3, "seqs 0, 3, 5 emitted");
    assert!((first_sample(&frames[0]) - 0.1).abs() < 1e-3);
    assert!((first_sample(&frames[1]) - 0.4).abs() < 1e-3);
    assert!((first_sample(&frames[2]) - 0.6).abs() < 1e-3);
    assert!(jitter.is_empty());
}

// ---------------------------------------------------------------------------
// Test: duplicates don't produce extra frames
// ---------------------------------------------------------------------------

#[test]
fn duplicates_do_not_produce_extra_frames() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(16);
    let channels = 1u16;
    let sample_rate = 48_000u32;

    jitter.push(0, tagged_payload(0.1));
    jitter.push(1, tagged_payload(0.2));
    assert_eq!(jitter.push(0, tagged_payload(0.99)), PushOutcome::Duplicate);
    assert_eq!(jitter.push(1, tagged_payload(0.99)), PushOutcome::Duplicate);

    let frames = drain_jitter(&mut jitter, channels, sample_rate);
    assert_eq!(frames.len(), 2);
    assert!((first_sample(&frames[0]) - 0.1).abs() < 1e-3);
    assert!((first_sample(&frames[1]) - 0.2).abs() < 1e-3);
}

// ---------------------------------------------------------------------------
// Test: capacity overflow rejects newest
// ---------------------------------------------------------------------------

#[test]
fn capacity_overflow_rejects_newest() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(2); // capacity=2
    let channels = 1u16;
    let sample_rate = 48_000u32;

    jitter.push(0, tagged_payload(0.1));
    jitter.push(1, tagged_payload(0.2));
    // Buffer full (2/2). Third packet is rejected.
    assert_eq!(
        jitter.push(2, tagged_payload(0.3)),
        PushOutcome::Rejected(tagged_payload(0.3))
    );

    let frames = drain_jitter(&mut jitter, channels, sample_rate);
    assert_eq!(frames.len(), 2);
    assert!((first_sample(&frames[0]) - 0.1).abs() < 1e-3);
    assert!((first_sample(&frames[1]) - 0.2).abs() < 1e-3);
}

// ---------------------------------------------------------------------------
// Test: larger stereo L16 payload roundtrip through the pipeline
// ---------------------------------------------------------------------------

#[test]
fn stereo_l16_pipeline_roundtrip() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(16);
    let channels = 2u16;
    let sample_rate = 48_000u32;

    // Two stereo frames: [L0,R0] and [L1,R1].
    let p0 = encode_l16(&[0.5, -0.5, 0.25, -0.25], 2);
    let p1 = encode_l16(&[1.0, -1.0, 0.0, 0.0], 2);

    jitter.push(0, p0);
    jitter.push(1, p1);

    let frames = drain_jitter(&mut jitter, channels, sample_rate);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].channels, 2);
    assert_eq!(frames[0].samples.len(), 4);
    // Frame 0: [0.5, -0.5, 0.25, -0.25] (within L16 tolerance).
    let want0 = [0.5, -0.5, 0.25, -0.25];
    for (i, (got, want)) in frames[0].samples.iter().zip(want0.iter()).enumerate() {
        assert!(
            (got - want).abs() < 1e-3,
            "frame0 sample {i}: got {got}, want {want}"
        );
    }
    // Frame 1: [1.0, -1.0, 0.0, 0.0].
    let want1 = [1.0, -1.0, 0.0, 0.0];
    for (i, (got, want)) in frames[1].samples.iter().zip(want1.iter()).enumerate() {
        assert!(
            (got - want).abs() < 1e-3,
            "frame1 sample {i}: got {got}, want {want}"
        );
    }
    assert!(jitter.is_empty());
}

// ---------------------------------------------------------------------------
// Test: empty jitter buffer drain produces nothing
// ---------------------------------------------------------------------------

#[test]
fn empty_drain_produces_nothing() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(16);
    let frames = drain_jitter(&mut jitter, 1, 48_000);
    assert!(frames.is_empty());
}

// ---------------------------------------------------------------------------
// Test: single packet drain
// ---------------------------------------------------------------------------

#[test]
fn single_packet_drains_correctly() {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(16);
    let channels = 1u16;
    let sample_rate = 48_000u32;

    jitter.push(42, tagged_payload(0.7));
    let frames = drain_jitter(&mut jitter, channels, sample_rate);
    assert_eq!(frames.len(), 1);
    assert!((first_sample(&frames[0]) - 0.7).abs() < 1e-3);
    assert!(jitter.is_empty());
}
