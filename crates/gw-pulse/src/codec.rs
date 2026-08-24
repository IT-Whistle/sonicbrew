//! M10 — PulseAudio native-protocol wire codec (P1 subset).
//!
//! Pure-Rust parser for the subset of the PulseAudio native protocol needed to
//! demonstrate parsing a playback stream's [`SampleSpec`] and a
//! length-prefixed string. The live daemon connection (auth cookie, socket
//! handshake, libpulse FFI) is **deferred** behind a default-off feature; this
//! module links nothing but the standard library and `thiserror`.
//!
//! # Wire facts
//!
//! The PulseAudio native protocol frames each message with a fixed
//! **20-byte descriptor** (all integers big-endian):
//!
//! ```text
//!  offset  field     type    meaning
//!  ------  --------  ------  ---------------------------------------------
//!  0       length    u32 BE  body length (bytes following this descriptor)
//!  4       channel   u32 BE  stream channel id (0 = control connection)
//!  8       offset    u64 BE  seek offset (memblock positioning)
//!  16      flags     u32 BE  frame flags
//! ```
//!
//! The command **opcode** is *not* part of the 20-byte descriptor: it is the
//! first big-endian `u32` of the body (the PA command tag). [`PacketHeader`]
//! folds it in for convenience — [`PacketHeader::parse`] reads the 20-byte
//! descriptor, and when at least four further bytes are present it also decodes
//! the leading opcode; otherwise `opcode` is `0`.
//!
//! A [`SampleSpec`] is **6 bytes**: `sample_rate: u32 BE`, `channels: u8`,
//! `format: u8` (the real `pa_sample_spec` is also 6 bytes). (The brief labels
//! this "5 bytes"; the documented field set `u32 + u8 + u8` is 6, so 6 is what
//! is implemented — same shape of discrepancy as the browser-gateway header.)
//!
//! Strings are `u32 BE` length-prefixed and NUL-terminated; the length counts
//! the NUL.
//!
//! (The brief lists five "packet-header fields" including `opcode` while also
//! stating the header is 20 bytes; the real PA descriptor is the four fields
//! above = 20 bytes, with `opcode` opening the body. This module implements the
//! 20-byte descriptor and treats `opcode` as the body word, so both the
//! `HEADER_LEN == 20` constant and the five-field [`PacketHeader`] struct hold.)

/// Length of the fixed PulseAudio frame descriptor, in bytes (4 + 4 + 8 + 4).
pub const HEADER_LEN: usize = 20;

/// Sane upper bound on a declared body/string length, guarding against a
/// hostile or truncated length prefix forcing a giant allocation. PulseAudio
/// itself caps native packets near 64 MiB; 4 MiB is ample for the P1 subset.
pub const MAX_LEN: u32 = 4 * 1024 * 1024;

/// Errors returned by the PulseAudio wire codec.
#[derive(Debug, thiserror::Error)]
pub enum PulseError {
    /// Buffer did not contain enough bytes for the field being parsed.
    #[error("buffer truncated")]
    Truncated,
    /// Encountered an unknown tag byte (tagstruct fields). Reserved for the
    /// fuller parser; the P1 subset does not raise it yet.
    #[error("invalid tag byte {0:#04x}")]
    InvalidTag(u8),
    /// Unknown PA sample-format byte.
    #[error("invalid sample format byte {0}")]
    InvalidSampleFormat(u8),
    /// A length prefix exceeded [`MAX_LEN`] or the remaining buffer.
    #[error("length prefix oversized or exceeds buffer")]
    Oversized,
    /// A tagstruct string was not valid UTF-8.
    #[error("string not valid UTF-8")]
    InvalidUtf8,
}

/// PulseAudio native-protocol command opcodes (subset).
///
/// The opcode is the first big-endian `u32` of a frame body (exposed as
/// [`PacketHeader::opcode`] when a body is present). Numbering follows the
/// server's dispatch table (`pulsecore/native-common.h`); only the commands
/// the daemon handshake needs are listed here. Note the protocol has **no
/// per-command replies**: success responses to `AUTH`, `SET_CLIENT_NAME` etc.
/// all come back as [`REPLY`]; only the introspection replies (e.g.
/// [`SERVER_INFO`] for `QUERY_INFO`) have their own opcodes.
pub mod command {
    /// `PA_COMMAND_ERROR` — error response (tagstruct: `u32` code + string).
    pub const ERROR: u32 = 0;
    /// `PA_COMMAND_TIMEOUT` — server-side timeout response.
    pub const TIMEOUT: u32 = 1;
    /// `PA_COMMAND_REPLY` — generic success response.
    pub const REPLY: u32 = 2;
    /// `PA_COMMAND_CREATE_PLAYBACK_STREAM` — open a playback stream. The
    /// v35 request carries sample spec, channel map, sink selection, buffer
    /// metrics and the version-gated flag blocks (see
    /// [`crate::daemon::PulseDaemon::create_playback_stream`]); the reply's
    /// first two `u32`s are the stream channel and the sink-input index.
    pub const CREATE_PLAYBACK_STREAM: u32 = 3;
    /// `PA_COMMAND_DELETE_PLAYBACK_STREAM` — teardown; the payload is the
    /// stream channel as a single `u32` (the server requires EOF after it).
    pub const DELETE_PLAYBACK_STREAM: u32 = 4;
    /// `PA_COMMAND_AUTH` — handshake step 1 (protocol version + cookie).
    pub const AUTH: u32 = 8;
    /// `PA_COMMAND_SET_CLIENT_NAME` — handshake step 2 (client proplist).
    pub const SET_CLIENT_NAME: u32 = 9;
    /// `PA_SAMPLE_FLOAT32LE` format byte (pulse/sample.h).
    pub const SAMPLE_FLOAT32LE: u8 = 5;
    /// `PA_VOLUME_NORM` (0x10000) — unity channel volume in cvolume.
    pub const VOLUME_NORM: u32 = 0x10000;
    /// `PA_COMMAND_GET_SERVER_INFO` — no-arg server introspection request
    /// (native-common.h enum value 20).
    pub const QUERY_INFO: u32 = 20;
    /// `PA_COMMAND_REPLY` carries the server-info body for `QUERY_INFO`.
    pub const SERVER_INFO: u32 = 2;
}

/// Convenience `Result` alias for codec operations.
pub type Result<T> = std::result::Result<T, PulseError>;

/// Parsed PulseAudio frame descriptor plus the leading command opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    /// Body length in bytes (follows the descriptor).
    pub length: u32,
    /// Stream channel id; `0` is the control connection.
    pub channel: u32,
    /// Seek offset for memblock positioning.
    pub offset: u64,
    /// Frame flags.
    pub flags: u32,
    /// Command opcode — the first big-endian `u32` of the body. `0` when the
    /// buffer only carries the 20-byte descriptor.
    pub opcode: u32,
}

impl PacketHeader {
    /// Parses a frame descriptor from `buf`.
    ///
    /// Requires at least [`HEADER_LEN`] bytes. The 20-byte descriptor fields
    /// (`length`, `channel`, `offset`, `flags`) are always populated. If four
    /// more bytes are present they are decoded as the leading command
    /// `opcode`; otherwise `opcode` stays `0`.
    ///
    /// # Errors
    ///
    /// Returns [`PulseError::Truncated`] if `buf` is shorter than
    /// [`HEADER_LEN`].
    pub fn parse(buf: &[u8]) -> Result<PacketHeader> {
        if buf.len() < HEADER_LEN {
            return Err(PulseError::Truncated);
        }
        let length = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let channel = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let offset = u64::from_be_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        let flags = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let opcode = if buf.len() >= HEADER_LEN + 4 {
            u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]])
        } else {
            0
        };
        Ok(PacketHeader {
            length,
            channel,
            offset,
            flags,
            opcode,
        })
    }

    /// Cheap structural validity check: body length is non-zero and under
    /// [`MAX_LEN`].
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.length > 0 && self.length <= MAX_LEN
    }
}

/// Minimal PulseAudio sample-format subset (real `PA_SAMPLE_*` values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// `PA_SAMPLE_S16LE` = 0.
    S16Le,
    /// `PA_SAMPLE_FLOAT32LE` = 5.
    Float32Le,
}

impl SampleFormat {
    /// Maps a raw PA sample-format byte to the enum.
    #[must_use]
    pub fn from_pa_byte(b: u8) -> Option<SampleFormat> {
        match b {
            0 => Some(SampleFormat::S16Le),
            5 => Some(SampleFormat::Float32Le),
            _ => None,
        }
    }
}

/// 6-byte PulseAudio sample specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleSpec {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u8,
    /// Sample format.
    pub format: SampleFormat,
}

impl SampleSpec {
    /// Parses a sample spec: `sample_rate: u32 BE`, `channels: u8`,
    /// `format: u8` (6 bytes total).
    ///
    /// # Errors
    ///
    /// Returns [`PulseError::Truncated`] if `buf` is shorter than 6 bytes, or
    /// [`PulseError::InvalidSampleFormat`] for an unknown format byte.
    pub fn parse(buf: &[u8]) -> Result<SampleSpec> {
        const SPEC_LEN: usize = 6;
        if buf.len() < SPEC_LEN {
            return Err(PulseError::Truncated);
        }
        let sample_rate = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let channels = buf[4];
        let format =
            SampleFormat::from_pa_byte(buf[5]).ok_or(PulseError::InvalidSampleFormat(buf[5]))?;
        Ok(SampleSpec {
            sample_rate,
            channels,
            format,
        })
    }

    /// Maps to the `(channels, sample_rate)` pair used by
    /// [`audio_core_bsd::AudioFrame`].
    #[must_use]
    pub fn to_audio_frame_meta(&self) -> (u16, u32) {
        (u16::from(self.channels), self.sample_rate)
    }
}

/// Parses a length-prefixed, NUL-terminated PulseAudio string.
///
/// Reads a big-endian `u32` length, then that many bytes (the length includes
/// the trailing NUL). Returns the string bytes *without* the NUL and the
/// remainder of the buffer.
///
/// # Errors
///
/// Returns [`PulseError::Truncated`] if fewer than 4 bytes remain for the
/// length prefix, or [`PulseError::Oversized`] if the declared length exceeds
/// [`MAX_LEN`] or the remaining buffer.
pub fn parse_pa_string(buf: &[u8]) -> Result<(&[u8], &[u8])> {
    const PREFIX: usize = 4;
    if buf.len() < PREFIX {
        return Err(PulseError::Truncated);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_LEN {
        return Err(PulseError::Oversized);
    }
    let end = PREFIX + usize::try_from(len).expect("len <= MAX_LEN fits usize");
    if buf.len() < end {
        return Err(PulseError::Oversized);
    }
    let str_bytes = &buf[PREFIX..end];
    // Drop the trailing NUL (length includes it) when present.
    let str_bytes = str_bytes.strip_suffix(b"\0").unwrap_or(str_bytes);
    Ok((str_bytes, &buf[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- PacketHeader ----

    #[test]
    fn packet_header_parse_reads_known_descriptor() {
        // 20-byte BE descriptor: length=0xBB80, channel=1, offset=0, flags=0.
        let mut buf = Vec::with_capacity(HEADER_LEN);
        buf.extend_from_slice(&0x0000BB80u32.to_be_bytes()); // length
        buf.extend_from_slice(&1u32.to_be_bytes()); // channel
        buf.extend_from_slice(&0u64.to_be_bytes()); // offset
        buf.extend_from_slice(&0u32.to_be_bytes()); // flags
        assert_eq!(buf.len(), HEADER_LEN);

        let h = PacketHeader::parse(&buf).expect("parse ok");
        assert_eq!(h.length, 0xBB80);
        assert_eq!(h.channel, 1);
        assert_eq!(h.offset, 0);
        assert_eq!(h.flags, 0);
        // opcode requires a body word; with only the descriptor it is 0.
        assert_eq!(h.opcode, 0);
        assert!(h.is_valid());
    }

    #[test]
    fn packet_header_parse_decodes_opcode_from_body() {
        // 24-byte buffer: 20-byte descriptor + leading opcode word.
        let mut buf = Vec::with_capacity(HEADER_LEN + 4);
        buf.extend_from_slice(&0x00000010u32.to_be_bytes()); // length = 16
        buf.extend_from_slice(&0u32.to_be_bytes()); // channel
        buf.extend_from_slice(&0u64.to_be_bytes()); // offset
        buf.extend_from_slice(&0u32.to_be_bytes()); // flags
        buf.extend_from_slice(&9u32.to_be_bytes()); // opcode = 9
        let h = PacketHeader::parse(&buf).expect("parse ok");
        assert_eq!(h.length, 16);
        assert_eq!(h.opcode, 9);
        assert!(h.is_valid());
    }

    #[test]
    fn packet_header_parse_truncated() {
        let short = [0u8; HEADER_LEN - 1];
        assert!(matches!(
            PacketHeader::parse(&short),
            Err(PulseError::Truncated)
        ));
        // Even a single missing byte trips Truncated.
        let ten = [0u8; 10];
        assert!(matches!(
            PacketHeader::parse(&ten),
            Err(PulseError::Truncated)
        ));
    }

    #[test]
    fn packet_header_is_valid_rejects_zero_and_oversized_length() {
        let mut h = PacketHeader {
            length: 0,
            channel: 0,
            offset: 0,
            flags: 0,
            opcode: 0,
        };
        assert!(!h.is_valid());
        h.length = MAX_LEN + 1;
        assert!(!h.is_valid());
        h.length = 1;
        assert!(h.is_valid());
    }

    // ---- SampleSpec ----

    #[test]
    fn sample_spec_parse_stereo_float32_48k() {
        // 6 bytes: 48000 BE, 2ch, FLOAT32LE=5.
        let buf: &[u8] = &[
            0x00, 0x00, 0xBB, 0x80, // sample_rate = 48000
            0x02, // channels = 2
            0x05, // format = FLOAT32LE
        ];
        let spec = SampleSpec::parse(buf).expect("parse ok");
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.format, SampleFormat::Float32Le);
        assert_eq!(spec.to_audio_frame_meta(), (2, 48_000));
    }

    #[test]
    fn sample_spec_parse_s16le_mono() {
        let buf: &[u8] = &[
            0x00, 0x00, 0xAC, 0x44, // 44100 BE
            0x01, // 1 channel
            0x00, // S16LE
        ];
        let spec = SampleSpec::parse(buf).expect("parse ok");
        assert_eq!(spec.sample_rate, 44_100);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.format, SampleFormat::S16Le);
    }

    #[test]
    fn sample_spec_parse_invalid_format_byte() {
        let buf: &[u8] = &[
            0x00, 0x00, 0xBB, 0x80, // 48000
            0x02, 0xFF, // unknown format
        ];
        assert!(matches!(
            SampleSpec::parse(buf),
            Err(PulseError::InvalidSampleFormat(0xFF))
        ));
    }

    #[test]
    fn sample_spec_parse_truncated() {
        let buf = [0u8; 5];
        assert!(matches!(
            SampleSpec::parse(&buf),
            Err(PulseError::Truncated)
        ));
    }

    #[test]
    fn sample_format_from_pa_byte_known_values() {
        assert_eq!(SampleFormat::from_pa_byte(0), Some(SampleFormat::S16Le));
        assert_eq!(SampleFormat::from_pa_byte(5), Some(SampleFormat::Float32Le));
        assert_eq!(SampleFormat::from_pa_byte(1), None);
        assert_eq!(SampleFormat::from_pa_byte(255), None);
    }

    // ---- parse_pa_string ----

    #[test]
    fn parse_pa_string_round_trip_hello() {
        // length=6 (includes NUL), payload "hello\0".
        let mut buf = Vec::new();
        buf.extend_from_slice(&6u32.to_be_bytes());
        buf.extend_from_slice(b"hello");
        buf.push(0x00);
        let (s, rest) = parse_pa_string(&buf).expect("parse ok");
        assert_eq!(s, b"hello");
        assert!(rest.is_empty());
    }

    #[test]
    fn parse_pa_string_returns_remainder() {
        // "ab\0" then 3 trailing bytes that are NOT part of the string.
        let mut buf = Vec::new();
        buf.extend_from_slice(&3u32.to_be_bytes());
        buf.extend_from_slice(b"ab");
        buf.push(0x00);
        buf.extend_from_slice(&[0x11, 0x22, 0x33]);
        let (s, rest) = parse_pa_string(&buf).expect("parse ok");
        assert_eq!(s, b"ab");
        assert_eq!(rest, &[0x11, 0x22, 0x33]);
    }

    #[test]
    fn parse_pa_string_truncated_prefix() {
        let buf = [0u8; 3];
        assert!(matches!(parse_pa_string(&buf), Err(PulseError::Truncated)));
    }

    #[test]
    fn parse_pa_string_oversized_length() {
        // Declares 100 bytes but only 1 follows.
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_be_bytes());
        buf.push(b'x');
        assert!(matches!(parse_pa_string(&buf), Err(PulseError::Oversized)));
    }

    #[test]
    fn header_len_constant_is_twenty() {
        assert_eq!(HEADER_LEN, 20);
    }
}
