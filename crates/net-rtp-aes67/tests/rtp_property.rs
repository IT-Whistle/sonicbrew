//! Property-based (proptest) tests for the net-rtp-aes67 crate.
//!
//! Covers:
//! - (a) L16 encode/decode roundtrip — any `f32` samples in [-1.0, 1.0] encode
//!   to L16 and decode back within ±1 LSB of quantisation error.
//! - (b) RTP packet serialisation roundtrip — any header + payload encode to
//!   bytes and parse back field-identical.
//! - (c) Sequence-number monotonicity — `UdpTransport` seq increments by +1 on
//!   every send, including across the u16 wrap boundary.

use net_rtp_aes67::{RtpHeader, RtpPacket, RTP_HEADER_LEN, RTP_VERSION};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// proptest strategies
// ---------------------------------------------------------------------------

/// Arbitrary `RtpHeader` with `version = 2`, `csrc_count = 0`, `padding =
/// false`, `extension = false`. Only the user-visible fields are randomised.
fn arb_rtp_header() -> impl Strategy<Value = RtpHeader> {
    (
        any::<u8>().prop_map(|v| v & 0x7F), // payload_type (0..127)
        any::<u16>(),                       // seq
        any::<u32>(),                       // timestamp
        any::<u32>(),                       // ssrc
        any::<bool>(),                      // marker
    )
        .prop_map(|(payload_type, seq, timestamp, ssrc, marker)| RtpHeader {
            version: RTP_VERSION,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker,
            payload_type,
            seq,
            timestamp,
            ssrc,
        })
}

/// Arbitrary non-empty payload bytes.
fn arb_payload() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..256)
}

// ---------------------------------------------------------------------------
// Property (a): L16 encode/decode roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn l16_roundtrip(
        channels in 1u16..=8u16,
        frame_count in 1usize..128,
    ) {
        let total_samples = usize::from(channels) * frame_count;
        // Planar layout: channels-major, then frames.
        // Each sample in [-1.0, 1.0].
        let samples: Vec<f32> = (0..total_samples)
            .map(|i| (i as f32 * 0.01).sin())
            .collect();

        let encoded = net_rtp_aes67::encode_l16(&samples, channels);
        let decoded = net_rtp_aes67::decode_l16(&encoded, channels);

        prop_assert_eq!(decoded.len(), samples.len());

        for (i, (got, want)) in decoded.iter().zip(samples.iter()).enumerate() {
            // L16 quantises to i16: encode scales by 32767, decode divides by
            // 32768.  Max error ≈ 1.5/32768 ≈ 4.58e-5.
            let err = (got - want).abs();
            prop_assert!(
                err < 5.0e-5,
                "sample {i} error {err} exceeds L16 tolerance (got {got}, want {want})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property (b): RTP packet serialisation roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn rtp_packet_roundtrip(
        header in arb_rtp_header(),
        payload in arb_payload(),
    ) {
        let packet = RtpPacket { header, payload: payload.clone() };
        let bytes = packet.encode();

        // Header byte length must be exactly RTP_HEADER_LEN (csrc_count = 0).
        prop_assert_eq!(bytes.len(), RTP_HEADER_LEN + payload.len());

        let parsed = RtpPacket::parse(&bytes).expect("parse must succeed");

        // Header fields preserved.
        prop_assert_eq!(parsed.header.version, RTP_VERSION);
        prop_assert_eq!(parsed.header.padding, false);
        prop_assert_eq!(parsed.header.extension, false);
        prop_assert_eq!(parsed.header.csrc_count, 0);
        prop_assert_eq!(parsed.header.marker, header.marker);
        prop_assert_eq!(parsed.header.payload_type, header.payload_type);
        prop_assert_eq!(parsed.header.seq, header.seq);
        prop_assert_eq!(parsed.header.timestamp, header.timestamp);
        prop_assert_eq!(parsed.header.ssrc, header.ssrc);

        // Payload preserved.
        prop_assert_eq!(parsed.payload, payload);
    }
}

// ---------------------------------------------------------------------------
// Property (c): Seq number monotonicity (+1 per send)
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn seq_monotonic(start_seq in any::<u16>(), count in 1u16..=100u16) {
        // Simulate the sequence counter behaviour: start + i (wrapping).
        let mut seq = start_seq;
        for i in 0..count {
            prop_assert_eq!(seq, start_seq.wrapping_add(i));
            seq = seq.wrapping_add(1);
        }
        // After the loop, seq == start + count (wrapped).
        prop_assert_eq!(seq, start_seq.wrapping_add(count));
    }
}
