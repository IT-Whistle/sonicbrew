//! Protocol i5 — PulseAudio native-protocol deep parse tests.
//!
//! Covers scenarios beyond the inline unit tests in codec.rs: header field
//! extremes, SampleSpec round-trips, string encoding edge cases, auth-cookie
//! handshake message structure, command opcode diversity, and combined
//! parse sequences that simulate a real session init.

use gw_pulse::codec::{
    parse_pa_string, PacketHeader, PulseError, SampleFormat, SampleSpec, HEADER_LEN, MAX_LEN,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_header(length: u32, channel: u32, offset: u64, flags: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN);
    buf.extend_from_slice(&length.to_be_bytes());
    buf.extend_from_slice(&channel.to_be_bytes());
    buf.extend_from_slice(&offset.to_be_bytes());
    buf.extend_from_slice(&flags.to_be_bytes());
    buf
}

fn build_header_with_opcode(
    length: u32,
    channel: u32,
    offset: u64,
    flags: u32,
    opcode: u32,
) -> Vec<u8> {
    let mut buf = build_header(length, channel, offset, flags);
    buf.extend_from_slice(&opcode.to_be_bytes());
    buf
}

fn build_sample_spec(rate: u32, channels: u8, format: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(6);
    buf.extend_from_slice(&rate.to_be_bytes());
    buf.push(channels);
    buf.push(format);
    buf
}

fn build_pa_string(s: &[u8]) -> Vec<u8> {
    let len = s.len() as u32 + 1; // +1 for NUL
    let mut buf = Vec::with_capacity(4 + s.len() + 1);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(s);
    buf.push(0x00);
    buf
}

// ---------------------------------------------------------------------------
// (a) PacketHeader — field-by-field encoding/decoding
// ---------------------------------------------------------------------------

#[test]
fn header_all_fields_max_u32() {
    let buf = build_header(u32::MAX, u32::MAX, u64::MAX, u32::MAX);
    let h = PacketHeader::parse(&buf).expect("parse ok");
    assert_eq!(h.length, u32::MAX);
    assert_eq!(h.channel, u32::MAX);
    assert_eq!(h.offset, u64::MAX);
    assert_eq!(h.flags, u32::MAX);
    assert_eq!(h.opcode, 0); // no body word
}

#[test]
fn header_all_fields_zero() {
    let buf = build_header(0, 0, 0, 0);
    let h = PacketHeader::parse(&buf).expect("parse ok");
    assert_eq!(h.length, 0);
    assert_eq!(h.channel, 0);
    assert_eq!(h.offset, 0);
    assert_eq!(h.flags, 0);
}

#[test]
fn header_offset_boundary_2pow32() {
    let buf = build_header(100, 0, 1u64 << 32, 0);
    let h = PacketHeader::parse(&buf).expect("parse ok");
    assert_eq!(h.offset, 1u64 << 32);
}

#[test]
fn header_channel_stream_ids() {
    // channel=0 (control), channel=1 (playback), channel=2 (record)
    for ch in [0u32, 1, 2, 0xDEAD_BEEF] {
        let buf = build_header(10, ch, 0, 0);
        let h = PacketHeader::parse(&buf).expect("parse ok");
        assert_eq!(h.channel, ch);
    }
}

#[test]
fn header_flags_bitmask_preserved() {
    let flags = 0b1010_0101_1111_0000_0000_0000_0000_0000u32;
    let buf = build_header(1, 0, 0, flags);
    let h = PacketHeader::parse(&buf).expect("parse ok");
    assert_eq!(h.flags, flags);
}

#[test]
fn header_opcode_only_when_body_present() {
    // Exactly HEADER_LEN: opcode defaults to 0
    let buf = build_header(0, 0, 0, 0);
    assert_eq!(PacketHeader::parse(&buf).unwrap().opcode, 0);
    // HEADER_LEN + 4: opcode decoded
    let buf = build_header_with_opcode(16, 0, 0, 0, 42);
    assert_eq!(PacketHeader::parse(&buf).unwrap().opcode, 42);
}

// ---------------------------------------------------------------------------
// (b) SampleSpec — round-trip / boundary
// ---------------------------------------------------------------------------

#[test]
fn sample_spec_round_trip_stereo_44100() {
    let raw = build_sample_spec(44_100, 2, 0x05);
    let spec = SampleSpec::parse(&raw).expect("parse ok");
    assert_eq!(spec.sample_rate, 44_100);
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.format, SampleFormat::Float32Le);
}

#[test]
fn sample_spec_round_trip_mono_8000() {
    let raw = build_sample_spec(8_000, 1, 0x00);
    let spec = SampleSpec::parse(&raw).expect("parse ok");
    assert_eq!(spec.sample_rate, 8_000);
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.format, SampleFormat::S16Le);
}

#[test]
fn sample_spec_max_channels_255() {
    let raw = build_sample_spec(48_000, 255, 0x05);
    let spec = SampleSpec::parse(&raw).expect("parse ok");
    assert_eq!(spec.channels, 255);
}

#[test]
fn sample_spec_rate_zero() {
    let raw = build_sample_spec(0, 1, 0x00);
    let spec = SampleSpec::parse(&raw).expect("parse ok");
    assert_eq!(spec.sample_rate, 0);
}

#[test]
fn sample_spec_rate_max_u32() {
    let raw = build_sample_spec(u32::MAX, 1, 0x05);
    let spec = SampleSpec::parse(&raw).expect("parse ok");
    assert_eq!(spec.sample_rate, u32::MAX);
}

#[test]
fn sample_spec_unknown_format_returns_none() {
    assert_eq!(SampleFormat::from_pa_byte(0x01), None);
    assert_eq!(SampleFormat::from_pa_byte(0x02), None);
    assert_eq!(SampleFormat::from_pa_byte(0x03), None);
    assert_eq!(SampleFormat::from_pa_byte(0x04), None);
    assert_eq!(SampleFormat::from_pa_byte(0x06), None);
    assert_eq!(SampleFormat::from_pa_byte(0xFF), None);
}

#[test]
fn sample_spec_to_audio_frame_meta() {
    let raw = build_sample_spec(96_000, 6, 0x00);
    let spec = SampleSpec::parse(&raw).expect("parse ok");
    let (ch, rate) = spec.to_audio_frame_meta();
    assert_eq!(ch, 6);
    assert_eq!(rate, 96_000);
}

#[test]
fn sample_spec_truncated_5_bytes() {
    let buf = [0u8; 5];
    assert!(matches!(
        SampleSpec::parse(&buf),
        Err(PulseError::Truncated)
    ));
}

#[test]
fn sample_spec_truncated_1_byte() {
    let buf = [0xAA];
    assert!(matches!(
        SampleSpec::parse(&buf),
        Err(PulseError::Truncated)
    ));
}

// ---------------------------------------------------------------------------
// (c) String encoding — edge cases
// ---------------------------------------------------------------------------

#[test]
fn pa_string_empty() {
    // length=1 (just NUL), payload is "\0"
    let buf = vec![0x00, 0x00, 0x00, 0x01, 0x00];
    let (s, rest) = parse_pa_string(&buf).expect("parse ok");
    assert_eq!(s, b"");
    assert!(rest.is_empty());
}

#[test]
fn pa_string_long_payload() {
    let payload = vec![b'A'; 1000];
    let mut buf = Vec::new();
    buf.extend_from_slice(&(1001u32).to_be_bytes()); // length = 1000 + NUL
    buf.extend_from_slice(&payload);
    buf.push(0x00);
    let (s, rest) = parse_pa_string(&buf).expect("parse ok");
    assert_eq!(s.len(), 1000);
    assert_eq!(s, &payload[..]);
    assert!(rest.is_empty());
}

#[test]
fn pa_string_no_nul_terminator() {
    // Length says 3, bytes are "abc" with no NUL — should still work.
    let mut buf = Vec::new();
    buf.extend_from_slice(&3u32.to_be_bytes());
    buf.extend_from_slice(b"abc");
    let (s, rest) = parse_pa_string(&buf).expect("parse ok");
    assert_eq!(s, b"abc");
    assert!(rest.is_empty());
}

#[test]
fn pa_string_length_equals_buffer() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&5u32.to_be_bytes());
    buf.extend_from_slice(b"test");
    buf.push(0x00);
    let (s, rest) = parse_pa_string(&buf).expect("parse ok");
    assert_eq!(s, b"test");
    assert!(rest.is_empty());
}

#[test]
fn pa_string_multiple_strings_concatenated() {
    let mut buf = build_pa_string(b"hello");
    buf.extend_from_slice(&build_pa_string(b"world"));
    let (s1, rest) = parse_pa_string(&buf).expect("parse ok");
    assert_eq!(s1, b"hello");
    let (s2, rest) = parse_pa_string(rest).expect("parse ok");
    assert_eq!(s2, b"world");
    assert!(rest.is_empty());
}

#[test]
fn pa_string_exact_max_len_boundary() {
    // Length = MAX_LEN (just at the limit)
    let mut buf = Vec::new();
    buf.extend_from_slice(&MAX_LEN.to_be_bytes());
    buf.extend_from_slice(&vec![0xAB; MAX_LEN as usize - 1]);
    buf.push(0x00);
    let (s, rest) = parse_pa_string(&buf).expect("parse ok");
    assert_eq!(s.len(), MAX_LEN as usize - 1);
    assert!(rest.is_empty());
}

#[test]
fn pa_string_length_exceeds_max() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(MAX_LEN + 1).to_be_bytes());
    buf.extend_from_slice(&[0x00; 64]);
    assert!(matches!(parse_pa_string(&buf), Err(PulseError::Oversized)));
}

#[test]
fn pa_string_truncated_in_payload() {
    // Length=100, but only 2 bytes follow
    let mut buf = Vec::new();
    buf.extend_from_slice(&100u32.to_be_bytes());
    buf.extend_from_slice(&[0x00; 2]);
    assert!(matches!(parse_pa_string(&buf), Err(PulseError::Oversized)));
}

#[test]
fn pa_string_only_length_prefix_no_payload() {
    let buf = vec![0x00, 0x00, 0x00, 0x05];
    assert!(matches!(parse_pa_string(&buf), Err(PulseError::Oversized)));
}

// ---------------------------------------------------------------------------
// (d) Auth-cookie handshake message structure
// ---------------------------------------------------------------------------

#[test]
fn auth_handshake_setup_header_with_setup_command() {
    // PA command SETUP = opcode 1 (typically first message from client)
    let buf = build_header_with_opcode(64, 0, 0, 0, 1);
    let h = PacketHeader::parse(&buf).expect("parse ok");
    assert_eq!(h.length, 64);
    assert_eq!(h.opcode, 1);
    assert_eq!(h.channel, 0); // control connection
}

#[test]
fn auth_handshake_setup_body_contains_version_and_string() {
    // Simulated SETUP body: version (u32) + PA string (credential path).
    // Build the full message: 20-byte header + body.
    let mut body = Vec::new();
    body.extend_from_slice(&35u32.to_be_bytes()); // protocol version 35
    body.extend_from_slice(&build_pa_string(b"/home/user/.pulse-cookie"));

    let header = build_header_with_opcode(body.len() as u32, 0, 0, 0, 1);
    let mut msg = header;
    msg.extend_from_slice(&body);

    let h = PacketHeader::parse(&msg).expect("parse ok");
    assert_eq!(h.length, body.len() as u32);
    assert_eq!(h.opcode, 1); // SETUP command

    // Parse the version from the body (first 4 bytes after header+opcode).
    let body_start = HEADER_LEN + 4;
    let version = u32::from_be_bytes([
        msg[body_start],
        msg[body_start + 1],
        msg[body_start + 2],
        msg[body_start + 3],
    ]);
    assert_eq!(version, 35);

    // Parse the PA string from the body after the version.
    let (path, _) = parse_pa_string(&msg[body_start + 4..]).expect("string parse ok");
    assert_eq!(path, b"/home/user/.pulse-cookie");
}

#[test]
fn auth_handshake_authenticate_command() {
    // PA command AUTHENTICATE = opcode 2
    let buf = build_header_with_opcode(128, 0, 0, 0, 2);
    let h = PacketHeader::parse(&buf).expect("parse ok");
    assert_eq!(h.opcode, 2);
}

#[test]
fn auth_handshake_set_name_command() {
    // PA command SET_NAME = opcode 27
    let buf = build_header_with_opcode(64, 0, 0, 0, 27);
    let h = PacketHeader::parse(&buf).expect("parse ok");
    assert_eq!(h.opcode, 27);
}

// ---------------------------------------------------------------------------
// (e) Command opcode diversity
// ---------------------------------------------------------------------------

#[test]
fn opcodes_various_values() {
    // Each PA command has a distinct opcode; verify parsing for a few.
    let opcodes = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 20, 27, 32, 33, 34];
    for opcode in opcodes {
        let buf = build_header_with_opcode(100, 0, 0, 0, opcode);
        let h = PacketHeader::parse(&buf).expect("parse ok");
        assert_eq!(h.opcode, opcode, "opcode {opcode} round-trip failed");
    }
}

#[test]
fn opcode_max_value() {
    let buf = build_header_with_opcode(1, 0, 0, 0, u32::MAX);
    let h = PacketHeader::parse(&buf).expect("parse ok");
    assert_eq!(h.opcode, u32::MAX);
}

#[test]
fn opcode_zero_is_valid() {
    let buf = build_header_with_opcode(1, 0, 0, 0, 0);
    let h = PacketHeader::parse(&buf).expect("parse ok");
    assert_eq!(h.opcode, 0);
}

// ---------------------------------------------------------------------------
// Combined parse sequences — simulate session init
// ---------------------------------------------------------------------------

#[test]
fn session_init_sequence() {
    // 1. SETUP request
    let setup = build_header_with_opcode(64, 0, 0, 0, 1);
    let h1 = PacketHeader::parse(&setup).expect("setup parse ok");
    assert_eq!(h1.opcode, 1);

    // 2. AUTHENTICATE request with cookie path
    let mut auth_body = Vec::new();
    auth_body.extend_from_slice(&build_pa_string(b"cookie-data"));
    let auth_header = build_header_with_opcode(auth_body.len() as u32, 0, 0, 0, 2);
    let h2 = PacketHeader::parse(&auth_header).expect("auth parse ok");
    assert_eq!(h2.opcode, 2);
    // Cookie string can be parsed from the body
    let (cookie, _) = parse_pa_string(&auth_body).expect("cookie parse ok");
    assert_eq!(cookie, b"cookie-data");

    // 3. SET_NAME request
    let mut name_body = Vec::new();
    name_body.extend_from_slice(&build_pa_string(b"sonicbrew"));
    let name_header = build_header_with_opcode(name_body.len() as u32, 0, 0, 0, 27);
    let h3 = PacketHeader::parse(&name_header).expect("name parse ok");
    assert_eq!(h3.opcode, 27);

    // 4. Channel for audio data is non-zero
    let audio_buf = build_header_with_opcode(1024, 1, 0, 0, 10);
    let h4 = PacketHeader::parse(&audio_buf).expect("audio parse ok");
    assert_eq!(h4.channel, 1);
}

#[test]
fn header_plus_sample_spec_body() {
    // A stream-style message: 20-byte header describing body that starts
    // with a SampleSpec.
    let ss = build_sample_spec(48_000, 2, 0x05);
    let mut buf = build_header_with_opcode(6 + ss.len() as u32, 1, 0, 0, 9);
    buf.extend_from_slice(&ss);

    let h = PacketHeader::parse(&buf).expect("header parse ok");
    assert_eq!(h.length, 6 + ss.len() as u32);
    assert_eq!(h.opcode, 9);

    // Parse the SampleSpec from the bytes *after* the header+opcode.
    let body = &buf[HEADER_LEN + 4..];
    let spec = SampleSpec::parse(body).expect("spec parse ok");
    assert_eq!(spec.sample_rate, 48_000);
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.format, SampleFormat::Float32Le);
}

#[test]
fn repeated_header_parse_no_state_leak() {
    // Parse the same buffer twice; results must be identical.
    let buf = build_header_with_opcode(100, 5, 0x1234_5678_9ABC_DEF0, 0xFF, 42);
    let h1 = PacketHeader::parse(&buf).expect("first parse");
    let h2 = PacketHeader::parse(&buf).expect("second parse");
    assert_eq!(h1, h2);
}

// ---------------------------------------------------------------------------
// Edge: parse from a larger buffer with trailing bytes
// ---------------------------------------------------------------------------

#[test]
fn header_parse_ignores_trailing_bytes() {
    let mut buf = build_header(10, 1, 0, 0);
    buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // trailing garbage
    let h = PacketHeader::parse(&buf).expect("parse ok");
    assert_eq!(h.length, 10);
    assert_eq!(h.opcode, 0xDEAD_BEEF); // decoded from trailing bytes
}

#[test]
fn sample_spec_parse_ignores_trailing_bytes() {
    let mut raw = build_sample_spec(44_100, 2, 0x00);
    raw.extend_from_slice(&[0xFF; 32]); // trailing garbage
    let spec = SampleSpec::parse(&raw).expect("parse ok");
    assert_eq!(spec.sample_rate, 44_100);
}

#[test]
fn pa_string_parse_ignores_trailing_bytes() {
    let mut buf = build_pa_string(b"test");
    buf.extend_from_slice(&[0xFF; 64]); // trailing garbage
    let (s, rest) = parse_pa_string(&buf).expect("parse ok");
    assert_eq!(s, b"test");
    assert_eq!(rest.len(), 64);
}
