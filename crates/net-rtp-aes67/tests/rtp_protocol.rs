//! Protocol i5 — RTP header parsing deep-dive.
//!
//! Exercises every fixed-header field and edge case per RFC 3550 §5.1:
//! (a) field-by-field encoding/decoding roundtrip,
//! (b) payload-type whitelist gate,
//! (c) sequence-number wrapping at u16 boundary,
//! (d) CSRC count (CC field) parsing,
//! (e) extension header (X bit) parsing,
//! (f) padding (P bit) parsing.

use net_rtp_aes67::{RtpHeader, RtpPacket, RTP_HEADER_LEN, RTP_VERSION};

// ---------------------------------------------------------------------------
// (a) Field-by-field encoding / decoding roundtrip
// ---------------------------------------------------------------------------

#[test]
fn header_roundtrip_all_fields() {
    let h = RtpHeader {
        version: RTP_VERSION,
        padding: false,
        extension: false,
        csrc_count: 0,
        marker: true,
        payload_type: 96,
        seq: 0x1234,
        timestamp: 0xDEAD_BEEF,
        ssrc: 0x0A0B_0C0D,
    };
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).expect("parse");
    assert_eq!(parsed, h);
}

#[test]
fn header_version_field_is_top_two_bits() {
    let h = RtpHeader::new(0, 0, 0, 0);
    let bytes = h.encode();
    // Version = 2 → top two bits of byte 0 = 0b10.
    assert_eq!(bytes[0] & 0xC0, 0x80);
}

#[test]
fn header_marker_bit_roundtrip() {
    let mut h = RtpHeader::new(10, 1, 100, 42);
    h.marker = true;
    let bytes = h.encode();
    // Marker is bit 7 of byte 1.
    assert_ne!(bytes[1] & 0x80, 0);
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert!(parsed.marker);

    h.marker = false;
    let bytes = h.encode();
    assert_eq!(bytes[1] & 0x80, 0);
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert!(!parsed.marker);
}

#[test]
fn header_payload_type_roundtrip() {
    // PT is 7 bits (0–127). new() masks with 0x7F.
    let h = RtpHeader::new(127, 0, 0, 0);
    assert_eq!(h.payload_type, 127);
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert_eq!(parsed.payload_type, 127);
}

#[test]
fn header_payload_type_masked_by_new() {
    // Bits above 7 are stripped by new(): 0x80 → 0x00.
    let h = RtpHeader::new(0x80, 0, 0, 0);
    assert_eq!(h.payload_type, 0);
}

#[test]
fn header_seq_field_roundtrip() {
    let h = RtpHeader::new(0, 0xABCD, 0, 0);
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert_eq!(parsed.seq, 0xABCD);
}

#[test]
fn header_timestamp_field_roundtrip() {
    let h = RtpHeader::new(0, 0, 0xFFFF_FFFF, 0);
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert_eq!(parsed.timestamp, 0xFFFF_FFFF);
}

#[test]
fn header_ssrc_field_roundtrip() {
    let h = RtpHeader::new(0, 0, 0, 0xCAFEBABE);
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert_eq!(parsed.ssrc, 0xCAFEBABE);
}

#[test]
fn header_zero_fields() {
    let h = RtpHeader::new(0, 0, 0, 0);
    assert_eq!(h.version, RTP_VERSION);
    assert!(!h.padding);
    assert!(!h.extension);
    assert_eq!(h.csrc_count, 0);
    assert!(!h.marker);
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert_eq!(parsed, h);
}

#[test]
fn header_all_flags_set() {
    let h = RtpHeader {
        version: RTP_VERSION,
        padding: true,
        extension: true,
        csrc_count: 3,
        marker: true,
        payload_type: 63,
        seq: 1,
        timestamp: 2,
        ssrc: 3,
    };
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert_eq!(parsed, h);
    assert!(parsed.padding);
    assert!(parsed.extension);
    assert_eq!(parsed.csrc_count, 3);
    assert!(parsed.marker);
}

// ---------------------------------------------------------------------------
// (b) Payload-type whitelist gate
// ---------------------------------------------------------------------------

#[test]
fn payload_type_whitelist_accepts_valid() {
    // All 7-bit values are valid (0–127). The parser does not gate by PT;
    // the whitelist is a caller-side concern. Verify the roundtrip covers
    // every whitelisted value used in this crate.
    for pt in [10, 11, 96, 97] {
        let h = RtpHeader::new(pt, 1, 100, 7);
        let bytes = h.encode();
        let parsed = RtpHeader::parse(&bytes).unwrap();
        assert_eq!(parsed.payload_type, pt, "PT {pt} roundtrip");
    }
}

#[test]
fn payload_type_max_value_roundtrip() {
    let h = RtpHeader::new(127, 0, 0, 0);
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert_eq!(parsed.payload_type, 127);
}

#[test]
fn payload_type_min_value_roundtrip() {
    let h = RtpHeader::new(0, 0, 0, 0);
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert_eq!(parsed.payload_type, 0);
}

// ---------------------------------------------------------------------------
// (c) Sequence-number wrapping at u16 boundary
// ---------------------------------------------------------------------------

#[test]
fn seq_wrap_65534_to_0() {
    // 65534 → 65535 → 0 — three consecutive seqs crossing the u16 boundary.
    for seq in [65534u16, 65535, 0] {
        let h = RtpHeader::new(10, seq, 0, 0);
        let bytes = h.encode();
        let parsed = RtpHeader::parse(&bytes).unwrap();
        assert_eq!(parsed.seq, seq, "seq {seq} roundtrip");
    }
}

#[test]
fn seq_max_value_roundtrip() {
    let h = RtpHeader::new(0, u16::MAX, 0, 0);
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert_eq!(parsed.seq, u16::MAX);
}

#[test]
fn seq_min_value_roundtrip() {
    let h = RtpHeader::new(0, 0, 0, 0);
    let bytes = h.encode();
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert_eq!(parsed.seq, 0);
}

#[test]
fn seq_wrapping_independence() {
    // Each seq value is stored as big-endian u16 — verify that 0xFFFE, 0xFFFF,
    // and 0x0000 produce distinct wire bytes.
    let e0 = RtpHeader::new(0, 0xFFFE, 0, 0).encode();
    let e1 = RtpHeader::new(0, 0xFFFF, 0, 0).encode();
    let e2 = RtpHeader::new(0, 0x0000, 0, 0).encode();
    assert_ne!(&e0[2..4], &e1[2..4]);
    assert_ne!(&e1[2..4], &e2[2..4]);
}

// ---------------------------------------------------------------------------
// (d) CSRC count (CC field) handling
// ---------------------------------------------------------------------------

#[test]
fn csrc_count_zero_is_default() {
    let h = RtpHeader::new(10, 0, 0, 0);
    assert_eq!(h.csrc_count, 0);
    assert_eq!(h.encoded_len(), RTP_HEADER_LEN);
}

#[test]
fn csrc_count_stored_but_not_encoded() {
    // csrc_count is stored in the header struct and reflected in encoded_len,
    // but the encoder only writes the 12-byte fixed header. The CSRC *list*
    // itself is not written.
    let mut h = RtpHeader::new(10, 0, 0, 0);
    h.csrc_count = 4;
    let bytes = h.encode();
    assert_eq!(bytes.len(), RTP_HEADER_LEN, "encoder emits only 12 bytes");
    assert_eq!(
        h.encoded_len(),
        RTP_HEADER_LEN + 16,
        "encoded_len accounts for 4 CSRCs"
    );
}

#[test]
fn csrc_count_max_fifteen() {
    let mut h = RtpHeader::new(10, 0, 0, 0);
    h.csrc_count = 15;
    let bytes = h.encode();
    // CC is the low nibble of byte 0.
    assert_eq!(bytes[0] & 0x0F, 15);
}

#[test]
fn csrc_count_parsed_from_wire() {
    // Manually construct a buffer with CC=2 in byte 0.
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = (RTP_VERSION << 6) | 0x02; // version=2, CC=2
    buf[1] = 0x00; // marker=0, PT=0
    let parsed = RtpHeader::parse(&buf).unwrap();
    assert_eq!(parsed.csrc_count, 2);
}

// ---------------------------------------------------------------------------
// (e) Extension header (X bit) handling
// ---------------------------------------------------------------------------

#[test]
fn extension_bit_roundtrip() {
    let mut h = RtpHeader::new(10, 0, 0, 0);
    h.extension = true;
    let bytes = h.encode();
    // X bit is bit 4 of byte 0.
    assert_ne!(bytes[0] & 0x10, 0);
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert!(parsed.extension);
}

#[test]
fn extension_bit_not_parsing_extension_data() {
    // The parser reads the X bit but does NOT consume extension header bytes.
    // Append 4 bytes of fake extension data after the fixed header; the parser
    // should still succeed and leave them as payload.
    let mut h = RtpHeader::new(10, 1, 100, 42);
    h.extension = true;
    let mut bytes = h.encode().to_vec();
    bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // fake extension
    let packet = RtpPacket::parse(&bytes).unwrap();
    assert!(packet.header.extension);
    // The 4 extension bytes are part of the payload (parser doesn't know the
    // extension length without reading the extension header format).
    assert_eq!(packet.payload, vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

// ---------------------------------------------------------------------------
// (f) Padding (P bit) handling
// ---------------------------------------------------------------------------

#[test]
fn padding_bit_roundtrip() {
    let mut h = RtpHeader::new(10, 0, 0, 0);
    h.padding = true;
    let bytes = h.encode();
    // P bit is bit 5 of byte 0.
    assert_ne!(bytes[0] & 0x20, 0);
    let parsed = RtpHeader::parse(&bytes).unwrap();
    assert!(parsed.padding);
}

#[test]
fn padding_bit_with_payload() {
    // P=1 with payload bytes. The parser reads the P bit but does NOT strip
    // padding from the payload (that's a caller responsibility per RFC 3550).
    let mut h = RtpHeader::new(10, 1, 100, 42);
    h.padding = true;
    let mut bytes = h.encode().to_vec();
    bytes.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
    let packet = RtpPacket::parse(&bytes).unwrap();
    assert!(packet.header.padding);
    assert_eq!(packet.payload, vec![0x01, 0x02, 0x03, 0x04]);
}

// ---------------------------------------------------------------------------
// Edge cases: truncated buffers, version rejection
// ---------------------------------------------------------------------------

#[test]
fn truncated_header_too_short() {
    // 11 bytes — one byte short of the 12-byte minimum.
    assert!(RtpHeader::parse(&[0u8; 11]).is_err());
}

#[test]
fn truncated_header_empty() {
    assert!(RtpHeader::parse(&[]).is_err());
}

#[test]
fn truncated_header_one_byte() {
    assert!(RtpHeader::parse(&[0u8; 1]).is_err());
}

#[test]
fn rejects_version_zero() {
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = 0x00; // version=0
    assert!(RtpHeader::parse(&buf).is_err());
}

#[test]
fn rejects_version_one() {
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = 0x40; // version=1
    assert!(RtpHeader::parse(&buf).is_err());
}

#[test]
fn rejects_version_three() {
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = 0xC0; // version=3
    assert!(RtpHeader::parse(&buf).is_err());
}

#[test]
fn packet_truncated_after_header() {
    // Valid 12-byte header but packet claims CC=1 (encoded_len=16), so buffer
    // is too short.
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = (RTP_VERSION << 6) | 0x01; // CC=1
    assert!(RtpPacket::parse(&buf).is_err());
}

#[test]
fn packet_exact_header_no_payload() {
    // Exactly 12 bytes with CC=0 — valid, payload is empty.
    let mut buf = [0u8; RTP_HEADER_LEN];
    buf[0] = RTP_VERSION << 6;
    let packet = RtpPacket::parse(&buf).unwrap();
    assert!(packet.payload.is_empty());
}

#[test]
fn new_header_always_sets_version_2() {
    let h = RtpHeader::new(10, 0, 0, 0);
    assert_eq!(h.version, RTP_VERSION);
}

#[test]
fn new_header_masks_payload_type() {
    // new() ensures PT fits in 7 bits: 0x80 → 0.
    let h = RtpHeader::new(0x80, 0, 0, 0);
    assert_eq!(h.payload_type, 0);
}

#[test]
fn encode_len_accounts_for_csrc() {
    let mut h = RtpHeader::new(10, 0, 0, 0);
    h.csrc_count = 7;
    assert_eq!(h.encoded_len(), RTP_HEADER_LEN + 7 * 4);
}
