//! Sanitizer i4 — malformed / random-input fuzzing for PulseAudio codec.
//!
//! Guarantees no-panic on arbitrary byte sequences and validates that all
//! error paths produce `PulseError` variants (never a panic or abort).
//! Uses deterministic malformed cases plus proptest for property-based tests.

use gw_pulse::codec::{
    parse_pa_string, PacketHeader, PulseError, SampleFormat, SampleSpec, HEADER_LEN, MAX_LEN,
};

// ---------------------------------------------------------------------------
// Deterministic malformed inputs — no-panic + PulseError
// ---------------------------------------------------------------------------

#[test]
fn empty_bytes_no_panic() {
    let r = PacketHeader::parse(&[]);
    assert!(r.is_err());
}

#[test]
fn single_byte_no_panic() {
    let r = PacketHeader::parse(&[0x00]);
    assert!(r.is_err());
}

#[test]
fn nineteen_bytes_no_panic() {
    let r = PacketHeader::parse(&[0x00; 19]);
    assert!(r.is_err());
}

#[test]
fn exactly_header_len_no_body() {
    let r = PacketHeader::parse(&[0x00; HEADER_LEN]);
    assert!(r.is_ok());
    assert_eq!(r.unwrap().opcode, 0);
}

#[test]
fn header_with_zero_length_body() {
    let buf = [0x00u8; HEADER_LEN];
    let r = PacketHeader::parse(&buf).unwrap();
    assert!(!r.is_valid()); // length=0 => is_valid() = false
}

#[test]
fn header_with_max_plus_one_length() {
    let mut buf = [0x00u8; HEADER_LEN];
    let len = MAX_LEN + 1;
    buf[..4].copy_from_slice(&len.to_be_bytes());
    let h = PacketHeader::parse(&buf).unwrap();
    assert!(!h.is_valid()); // exceeds MAX_LEN
}

#[test]
fn sample_spec_empty_no_panic() {
    let r = SampleSpec::parse(&[]);
    assert!(r.is_err());
}

#[test]
fn sample_spec_one_byte_no_panic() {
    let r = SampleSpec::parse(&[0x00]);
    assert!(r.is_err());
}

#[test]
fn sample_spec_five_bytes_no_panic() {
    let r = SampleSpec::parse(&[0x00; 5]);
    assert!(r.is_err());
}

#[test]
fn sample_spec_all_format_bytes_no_panic() {
    for b in 0u8..=255 {
        let mut buf = vec![0x00; 5];
        buf.push(b);
        let r = SampleSpec::parse(&buf);
        match SampleFormat::from_pa_byte(b) {
            Some(fmt) => {
                let spec = r.expect("known format byte should parse");
                assert_eq!(spec.format, fmt);
            }
            None => {
                assert!(matches!(r, Err(PulseError::InvalidSampleFormat(_))));
            }
        }
    }
}

#[test]
fn pa_string_empty_buffer_no_panic() {
    let r = parse_pa_string(&[]);
    assert!(r.is_err());
}

#[test]
fn pa_string_only_nul_no_panic() {
    let r = parse_pa_string(&[0x00]);
    assert!(r.is_err());
}

#[test]
fn pa_string_three_bytes_no_panic() {
    let r = parse_pa_string(&[0x00, 0x01, 0x02]);
    assert!(r.is_err());
}

#[test]
fn pa_string_huge_length_no_panic() {
    // MAX_LEN + 100 declared but only 1 byte present
    let len = MAX_LEN + 100;
    let mut buf = Vec::new();
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(0x00);
    let r = parse_pa_string(&buf);
    assert!(matches!(r, Err(PulseError::Oversized)));
}

// ---------------------------------------------------------------------------
// Large garbage payloads — no-panic
// ---------------------------------------------------------------------------

#[test]
fn large_header_garbage_no_panic() {
    let mut buf = vec![0xAB; 1024];
    // Overwrite first 4 bytes to set a valid-ish length
    buf[..4].copy_from_slice(&512u32.to_be_bytes());
    let r = PacketHeader::parse(&buf);
    let _ = r; // must not panic
}

#[test]
fn large_string_garbage_no_panic() {
    let mut buf = vec![0xCD; 4096];
    // Set length to 4092 (the rest)
    let len = 4092u32;
    buf[..4].copy_from_slice(&len.to_be_bytes());
    let r = parse_pa_string(&buf);
    let _ = r; // must not panic
}

#[test]
fn one_megabyte_garbage_no_panic() {
    let mut buf = vec![0x55; 1_048_576];
    buf[..4].copy_from_slice(&(1_048_572u32).to_be_bytes());
    let r = parse_pa_string(&buf);
    let _ = r; // must not panic
}

// ---------------------------------------------------------------------------
// Error variant validation
// ---------------------------------------------------------------------------

#[test]
fn error_paths_are_pulse_error_variants() {
    // Truncated
    let r = PacketHeader::parse(&[0u8; 3]);
    assert!(matches!(r, Err(PulseError::Truncated)));

    let r = SampleSpec::parse(&[0u8; 3]);
    assert!(matches!(r, Err(PulseError::Truncated)));

    let r = parse_pa_string(&[0u8; 3]);
    assert!(matches!(r, Err(PulseError::Truncated)));

    // InvalidSampleFormat
    let r = SampleSpec::parse(&[0, 0, 0, 0x01, 0x01, 0xFF]);
    assert!(matches!(r, Err(PulseError::InvalidSampleFormat(0xFF))));

    // Oversized — length claims 200 bytes but only 5 follow
    let mut buf = Vec::new();
    buf.extend_from_slice(&200u32.to_be_bytes());
    buf.extend_from_slice(&[0xFF; 5]);
    let r = parse_pa_string(&buf);
    assert!(matches!(r, Err(PulseError::Oversized)));
}

// ---------------------------------------------------------------------------
// proptest property-based fuzz tests
// ---------------------------------------------------------------------------

use proptest::prelude::*;

proptest! {
    #[test]
    fn arbitrary_bytes_header_never_panic(data in prop::collection::vec(any::<u8>(), 0..2048)) {
        let r = PacketHeader::parse(&data);
        let _ = r; // must not panic
    }

    #[test]
    fn arbitrary_bytes_sample_spec_never_panic(data in prop::collection::vec(any::<u8>(), 0..2048)) {
        let r = SampleSpec::parse(&data);
        let _ = r; // must not panic
    }

    #[test]
    fn arbitrary_bytes_pa_string_never_panic(data in prop::collection::vec(any::<u8>(), 0..2048)) {
        let r = parse_pa_string(&data);
        let _ = r; // must not panic
    }

    #[test]
    fn arbitrary_header_fields_never_panic(
        length in any::<u32>(),
        channel in any::<u32>(),
        offset in any::<u64>(),
        flags in any::<u32>(),
    ) {
        let mut buf = Vec::with_capacity(HEADER_LEN);
        buf.extend_from_slice(&length.to_be_bytes());
        buf.extend_from_slice(&channel.to_be_bytes());
        buf.extend_from_slice(&offset.to_be_bytes());
        buf.extend_from_slice(&flags.to_be_bytes());
        let h = PacketHeader::parse(&buf).unwrap();
        assert_eq!(h.length, length);
        assert_eq!(h.channel, channel);
        assert_eq!(h.offset, offset);
        assert_eq!(h.flags, flags);
    }

    #[test]
    fn arbitrary_sample_spec_fields_never_panic(
        rate in any::<u32>(),
        channels in any::<u8>(),
        format in any::<u8>(),
    ) {
        let mut buf = Vec::with_capacity(6);
        buf.extend_from_slice(&rate.to_be_bytes());
        buf.push(channels);
        buf.push(format);
        let r = SampleSpec::parse(&buf);
        match SampleFormat::from_pa_byte(format) {
            Some(fmt) => {
                let spec = r.unwrap();
                assert_eq!(spec.sample_rate, rate);
                assert_eq!(spec.channels, channels);
                assert_eq!(spec.format, fmt);
            }
            None => {
                assert!(matches!(r, Err(PulseError::InvalidSampleFormat(_))));
            }
        }
    }

    #[test]
    fn arbitrary_pa_string_length_never_panic(
        len in any::<u32>(),
        payload in prop::collection::vec(any::<u8>(), 0..2048),
    ) {
        let mut buf = Vec::new();
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&payload);
        let r = parse_pa_string(&buf);
        // Just ensure no panic; Ok or Err are both acceptable.
        let _ = r;
    }

    #[test]
    fn header_with_random_opcode_never_panic(
        length in any::<u32>(),
        opcode in any::<u32>(),
    ) {
        let mut buf = Vec::with_capacity(HEADER_LEN + 4);
        buf.extend_from_slice(&length.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u64.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&opcode.to_be_bytes());
        let h = PacketHeader::parse(&buf).unwrap();
        assert_eq!(h.opcode, opcode);
    }

    #[test]
    fn is_valid_consistency(
        length in any::<u32>(),
        channel in any::<u32>(),
    ) {
        let mut buf = Vec::with_capacity(HEADER_LEN);
        buf.extend_from_slice(&length.to_be_bytes());
        buf.extend_from_slice(&channel.to_be_bytes());
        buf.extend_from_slice(&0u64.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        let h = PacketHeader::parse(&buf).unwrap();
        assert_eq!(h.is_valid(), length > 0 && length <= MAX_LEN);
    }
}
