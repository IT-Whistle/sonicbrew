//! Sanitizer i5 — Fuzz / property-based tests for the RTP parser.
//!
//! Feeds arbitrary byte sequences into `RtpHeader::parse` and
//! `RtpPacket::parse` to guarantee **no panics** on any input. Also tests
//! specific pathological inputs: truncated packets, invalid version fields,
//! oversized CSRC counts, and padding length exceeding the payload.

use net_rtp_aes67::{RtpHeader, RtpPacket, RTP_HEADER_LEN, RTP_VERSION};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// proptest: arbitrary bytes → no panic
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn header_parse_never_panics_on_arbitrary_bytes(
        buf in proptest::collection::vec(any::<u8>(), 0..1500)
    ) {
        // Must never panic — errors are acceptable, panics are not.
        let _ = RtpHeader::parse(&buf);
    }

    #[test]
    fn packet_parse_never_panics_on_arbitrary_bytes(
        buf in proptest::collection::vec(any::<u8>(), 0..1500)
    ) {
        let _ = RtpPacket::parse(&buf);
    }

    #[test]
    fn valid_header_always_encodes_to_12_bytes(
        pt in 0u8..128,
        seq in any::<u16>(),
        ts in any::<u32>(),
        ssrc in any::<u32>(),
    ) {
        let h = RtpHeader::new(pt, seq, ts, ssrc);
        let bytes = h.encode();
        prop_assert_eq!(bytes.len(), 12);
    }

    #[test]
    fn header_roundtrip_is_idempotent(
        pt in 0u8..128,
        seq in any::<u16>(),
        ts in any::<u32>(),
        ssrc in any::<u32>(),
    ) {
        let h = RtpHeader::new(pt, seq, ts, ssrc);
        let bytes = h.encode();
        let parsed = RtpHeader::parse(&bytes).unwrap();
        prop_assert_eq!(parsed, h);
    }

    #[test]
    fn packet_roundtrip_preserves_payload(
        pt in 0u8..128,
        seq in any::<u16>(),
        ts in any::<u32>(),
        ssrc in any::<u32>(),
        payload in proptest::collection::vec(any::<u8>(), 0..500),
    ) {
        let h = RtpHeader::new(pt, seq, ts, ssrc);
        let pkt = net_rtp_aes67::RtpPacket { header: h, payload: payload.clone() };
        let bytes = pkt.encode();
        let parsed = RtpPacket::parse(&bytes).unwrap();
        prop_assert_eq!(parsed.header, h);
        prop_assert_eq!(parsed.payload, payload);
    }
}

// ---------------------------------------------------------------------------
// Targeted adversarial inputs (no proptest needed)
// ---------------------------------------------------------------------------

#[test]
fn empty_buffer_is_err() {
    assert!(RtpHeader::parse(&[]).is_err());
}

#[test]
fn one_byte_is_err() {
    assert!(RtpHeader::parse(&[0x80]).is_err());
}

#[test]
fn eleven_bytes_is_err() {
    assert!(RtpHeader::parse(&[0u8; 11]).is_err());
}

#[test]
fn version_zero_is_err() {
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = 0x00; // version 0
    assert!(RtpHeader::parse(&buf).is_err());
}

#[test]
fn version_one_is_err() {
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = 0x40; // version 1
    assert!(RtpHeader::parse(&buf).is_err());
}

#[test]
fn version_three_is_err() {
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = 0xC0; // version 3
    assert!(RtpHeader::parse(&buf).is_err());
}

#[test]
fn all_ones_buffer_with_valid_version() {
    // 0xFF bytes: version=3 → rejected (correct).
    let buf = [0xFF; RTP_HEADER_LEN];
    assert!(RtpHeader::parse(&buf).is_err());
}

#[test]
fn max_csrc_count_fifteen_is_parsed() {
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = (2u8 << 6) | 0x0F; // version=2, CC=15
    let h = RtpHeader::parse(&buf).unwrap();
    assert_eq!(h.csrc_count, 15);
}

#[test]
fn csrc_count_does_not_cause_oob_read() {
    // CC=15 but buffer is exactly 12 bytes (no CSRC list). parse() reads only
    // the fixed header — the CSRC list bytes are left to the caller.
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = (2u8 << 6) | 0x0F;
    let h = RtpHeader::parse(&buf).unwrap();
    assert_eq!(h.csrc_count, 15);
}

#[test]
fn packet_with_zero_length_payload() {
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = RTP_VERSION << 6;
    let pkt = RtpPacket::parse(&buf).unwrap();
    assert!(pkt.payload.is_empty());
}

#[test]
fn packet_with_huge_csrc_count_short_buffer() {
    // CC=15 → encoded_len=72, but buffer is only 12 bytes.
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = (2u8 << 6) | 0x0F;
    assert!(RtpPacket::parse(&buf).is_err());
}

#[test]
fn padding_bit_with_zero_length_payload() {
    // P=1, no payload. Parser sets padding=true but payload is empty.
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = (2u8 << 6) | 0x20; // version=2, P=1
    let pkt = RtpPacket::parse(&buf).unwrap();
    assert!(pkt.header.padding);
    assert!(pkt.payload.is_empty());
}

#[test]
fn extension_bit_with_no_extension_data() {
    // X=1, but no extension header bytes. Parser reads the bit, payload is empty.
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = (2u8 << 6) | 0x10; // version=2, X=1
    let pkt = RtpPacket::parse(&buf).unwrap();
    assert!(pkt.header.extension);
    assert!(pkt.payload.is_empty());
}

#[test]
fn mixed_flags_combination() {
    // P=1, X=1, CC=3, M=1, PT=127 — all flags set except version (must be 2).
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = (2u8 << 6) | 0x20 | 0x10 | 0x03; // V=2, P, X, CC=3
    buf[1] = 0x80 | 0x7F; // M=1, PT=127
    buf[2..4].copy_from_slice(&0xABCDu16.to_be_bytes());
    buf[4..8].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
    buf[8..12].copy_from_slice(&0xCAFEBABEu32.to_be_bytes());
    let h = RtpHeader::parse(&buf).unwrap();
    assert!(h.padding);
    assert!(h.extension);
    assert_eq!(h.csrc_count, 3);
    assert!(h.marker);
    assert_eq!(h.payload_type, 127);
    assert_eq!(h.seq, 0xABCD);
    assert_eq!(h.timestamp, 0xDEAD_BEEF);
    assert_eq!(h.ssrc, 0xCAFEBABE);
}

#[test]
fn boundary_seq_values() {
    for seq in [0u16, 1, 0x7FFF, 0x8000, 0xFFFE, 0xFFFF] {
        let h = RtpHeader::new(10, seq, 0, 0);
        let bytes = h.encode();
        let parsed = RtpHeader::parse(&bytes).unwrap();
        assert_eq!(parsed.seq, seq);
    }
}

#[test]
fn boundary_timestamp_values() {
    for ts in [0u32, 1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFE, 0xFFFF_FFFF] {
        let h = RtpHeader::new(10, 0, ts, 0);
        let bytes = h.encode();
        let parsed = RtpHeader::parse(&bytes).unwrap();
        assert_eq!(parsed.timestamp, ts);
    }
}

#[test]
fn boundary_ssrc_values() {
    for ssrc in [0u32, 1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFE, 0xFFFF_FFFF] {
        let h = RtpHeader::new(10, 0, 0, ssrc);
        let bytes = h.encode();
        let parsed = RtpHeader::parse(&bytes).unwrap();
        assert_eq!(parsed.ssrc, ssrc);
    }
}
