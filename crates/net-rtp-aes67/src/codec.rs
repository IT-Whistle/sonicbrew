//! RTP packet codec and L16/L24 PCM framing.
//!
//! This module is **pure Rust** (no I/O): it encodes/decodes RTP headers
//! ([`RtpHeader`], [`RtpPacket`]) per RFC 3550 and converts between the planar
//! `f32` layout used by [`audio_core_bsd::AudioFrame`] and the big-endian
//! interleaved PCM payloads carried by RTP audio (RFC 3551 / AES67).
//!
//! # Scope
//!
//! In scope (P1): RFC 3550 fixed-header codec, L16/L24 framing, payload-type
//! mapping. Out of scope: SAP/SDP, FEC, SRTP/DTLS, PTP/M16 alignment.
//!
//! [`audio_core_bsd::AudioFrame`]: crate::AudioFrame

use crate::{Codec, Result, TransportError};

/// RTP protocol version (RFC 3550 §5.1). Only `2` is valid.
pub const RTP_VERSION: u8 = 2;

/// Minimum RTP fixed-header length, in bytes (no CSRC list).
pub const RTP_HEADER_LEN: usize = 12;

/// RFC 3550 fixed RTP header (12 bytes minimum).
///
/// All multi-byte fields are big-endian on the wire. The CSRC *list* itself is
/// not stored here (only [`RtpHeader::csrc_count`]); [`RtpHeader::parse`]
/// consumes the CSRC bytes and [`RtpPacket`] carries everything after the
/// header as the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    /// Always [`RTP_VERSION`] (= 2) for any packet this crate emits.
    pub version: u8,
    /// Padding flag (RFC 3550 §5.1, P bit).
    pub padding: bool,
    /// Header-extension flag (RFC 3550 §5.1, X bit).
    pub extension: bool,
    /// Number of CSRC identifiers that follow the fixed header (0–15).
    pub csrc_count: u8,
    /// Marker bit (RFC 3551: typically the first packet of a talkspurt / frame).
    pub marker: bool,
    /// RTP payload type (0–127).
    pub payload_type: u8,
    /// Sequence number (increments by one per packet sent).
    pub seq: u16,
    /// RTP timestamp (clock units, monotonic per stream).
    pub timestamp: u32,
    /// Synchronisation source identifier.
    pub ssrc: u32,
}

impl RtpHeader {
    /// Creates a well-formed header for a fresh packet: `version = 2`, all flag
    /// bits clear, CSRC count zero.
    #[must_use]
    pub fn new(payload_type: u8, seq: u16, timestamp: u32, ssrc: u32) -> Self {
        Self {
            version: RTP_VERSION,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: false,
            payload_type: payload_type & 0x7F,
            seq,
            timestamp,
            ssrc,
        }
    }

    /// Parses a 12-byte (or longer) RTP fixed header from `buf`.
    ///
    /// Returns [`TransportError::Network`] if the buffer is shorter than
    /// [`RTP_HEADER_LEN`] or the on-wire version is not [`RTP_VERSION`].
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < RTP_HEADER_LEN {
            return Err(TransportError::Network(format!(
                "RTP header truncated: got {} bytes, need at least {RTP_HEADER_LEN}",
                buf.len()
            )));
        }
        let version = (buf[0] >> 6) & 0x03;
        if version != RTP_VERSION {
            return Err(TransportError::Network(format!(
                "bad RTP version: got {version}, expected {RTP_VERSION}"
            )));
        }
        let padding = ((buf[0] >> 5) & 0x01) != 0;
        let extension = ((buf[0] >> 4) & 0x01) != 0;
        let csrc_count = buf[0] & 0x0F;
        let marker = ((buf[1] >> 7) & 0x01) != 0;
        let payload_type = buf[1] & 0x7F;
        let seq = u16::from_be_bytes([buf[2], buf[3]]);
        let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        Ok(Self {
            version,
            padding,
            extension,
            csrc_count,
            marker,
            payload_type,
            seq,
            timestamp,
            ssrc,
        })
    }

    /// Encodes the fixed header to exactly [`RTP_HEADER_LEN`] big-endian bytes.
    ///
    /// The CSRC list is never emitted by this encoder (this crate always sets
    /// [`Self::csrc_count`] to 0); [`Self::encoded_len`] still accounts for a
    /// non-zero count so [`RtpPacket`] framing stays correct for parsed headers.
    #[must_use]
    pub fn encode(&self) -> [u8; RTP_HEADER_LEN] {
        let byte0 = (RTP_VERSION << 6)
            | (u8::from(self.padding) << 5)
            | (u8::from(self.extension) << 4)
            | (self.csrc_count & 0x0F);
        let byte1 = (u8::from(self.marker) << 7) | (self.payload_type & 0x7F);
        let mut buf = [0u8; RTP_HEADER_LEN];
        buf[0] = byte0;
        buf[1] = byte1;
        buf[2..4].copy_from_slice(&self.seq.to_be_bytes());
        buf[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        buf
    }

    /// On-wire length of the header including any CSRC list:
    /// `RTP_HEADER_LEN + 4 * csrc_count`.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        RTP_HEADER_LEN + 4 * usize::from(self.csrc_count)
    }
}

/// A complete RTP packet: a fixed header plus its (owned) payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpPacket {
    /// The parsed/constructed fixed header.
    pub header: RtpHeader,
    /// Everything after the header and CSRC list.
    pub payload: Vec<u8>,
}

impl RtpPacket {
    /// Encodes header + payload into a fresh `Vec<u8>`.
    ///
    /// This is **not** a real-time path: the allocation is acceptable for the
    /// worker-thread transport. The CSRC list is never written (this crate sets
    /// [`RtpHeader::csrc_count`] to 0 on emitted headers).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let header_bytes = self.header.encode();
        let mut out = Vec::with_capacity(header_bytes.len() + self.payload.len());
        out.extend_from_slice(&header_bytes);
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parses an RTP packet (header + trailing payload) from `buf`.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let header = RtpHeader::parse(buf)?;
        let header_len = header.encoded_len();
        if buf.len() < header_len {
            return Err(TransportError::Network(format!(
                "RTP packet shorter than its header: got {} bytes, header claims {header_len}",
                buf.len()
            )));
        }
        let payload = buf[header_len..].to_vec();
        Ok(Self { header, payload })
    }
}

/// Maps a [`Codec`] to its RTP payload type (RFC 3551 §4.5.14 / AES67).
///
/// - `PcmL16` stereo → **10** (L16/48k/2), mono → **11** (L16/48k/1)
/// - `PcmL24` → **96** (dynamic PT; would be advertised via SDP, out of scope)
/// - `Opus` → **97** (dynamic PT; SDP-negotiated, out of scope)
/// - `Custom(n)` → `n` verbatim (caller owns the assignment)
#[must_use]
pub fn codec_to_payload_type(codec: Codec, channels: u16) -> u8 {
    match codec {
        Codec::PcmL16 => {
            if channels >= 2 {
                10
            } else {
                11
            }
        }
        // Dynamic payload types (96–127). Real assignment happens via SDP,
        // which is explicitly out of scope (no SAP/SDP in P1). These stable
        // defaults let a self-contained loopback pick a sensible PT.
        Codec::PcmL24 => 96,
        Codec::Opus => 97,
        Codec::Custom(n) => n,
    }
}

/// Bytes per sample for the given codec, used to advance the RTP timestamp
/// clock by the per-channel frame count. Compressed codecs (`Opus`) have no
/// fixed bytes-per-sample, so a nominal `1` is used.
const fn bytes_per_sample(codec: Codec) -> usize {
    match codec {
        Codec::PcmL16 => 2,
        Codec::PcmL24 => 3,
        Codec::Opus | Codec::Custom(_) => 1,
    }
}

/// Advance the RTP timestamp clock should make for a payload of `len` bytes
/// at the given channel count (per-channel frame count). Public for callers
/// that drive the clock themselves.
#[must_use]
pub fn timestamp_advance(len: usize, codec: Codec, channels: u16) -> u32 {
    let bps = bytes_per_sample(codec);
    let ch = usize::from(channels).max(1);
    (len / (bps * ch)) as u32
}

// ---------------------------------------------------------------------------
// L16 / L24 PCM framing
// ---------------------------------------------------------------------------
//
// Sample-encoding convention (mirrors audio-io-bsd's sample_conv):
//   * encode:  multiply by (2^N - 1)   — symmetric, full-scale maps to +max
//   * decode:  divide   by  2^N        — power-of-two, cheap and bit-exact
// This deliberately asymmetry (32767 up / 32768 down) is the standard audio
// convention and keeps the round-trip error under one LSB.

const L16_FULL_SCALE: f32 = 32_767.0; // 2^15 - 1
const L16_DECODE_DIV: f32 = 32_768.0; // 2^15
const L24_FULL_SCALE: f32 = 8_388_607.0; // 2^23 - 1
const L24_DECODE_DIV: f32 = 8_388_608.0; // 2^23

/// Encodes planar `f32` samples (the [`AudioFrame`] layout: `[ch0.., ch1..]`)
/// to an L16 RTP payload: **big-endian, interleaved** `[L0, R0, L1, R1, …]`.
///
/// [`AudioFrame`]: crate::AudioFrame
#[must_use]
pub fn encode_l16(frames: &[f32], channels: u16) -> Vec<u8> {
    encode_pcm(frames, channels, L16_FULL_SCALE, 2, |s| {
        // Truncate to i16 (the shared encoder works in i32 for L24).
        let v = s as i16;
        v.to_be_bytes().to_vec()
    })
}

/// Decodes an L16 RTP payload (big-endian, interleaved) back to planar `f32`.
#[must_use]
pub fn decode_l16(bytes: &[u8], channels: u16) -> Vec<f32> {
    decode_pcm(bytes, channels, L16_DECODE_DIV, 2, |chunk| {
        i32::from(i16::from_be_bytes([chunk[0], chunk[1]]))
    })
}

/// Encodes planar `f32` samples to an L24 RTP payload: 3-byte big-endian,
/// interleaved, left-justified samples.
#[must_use]
pub fn encode_l24(frames: &[f32], channels: u16) -> Vec<u8> {
    encode_pcm(frames, channels, L24_FULL_SCALE, 3, |s| {
        vec![
            ((s >> 16) & 0xFF) as u8,
            ((s >> 8) & 0xFF) as u8,
            (s & 0xFF) as u8,
        ]
    })
}

/// Decodes an L24 RTP payload (3-byte big-endian, interleaved) to planar `f32`.
#[must_use]
pub fn decode_l24(bytes: &[u8], channels: u16) -> Vec<f32> {
    decode_pcm(bytes, channels, L24_DECODE_DIV, 3, |chunk| {
        // Assemble the unsigned 24-bit value then sign-extend to i32 when
        // bit 23 (the 24-bit sign bit) is set.
        let raw = (i32::from(chunk[0]) << 16) | (i32::from(chunk[1]) << 8) | i32::from(chunk[2]);
        if (raw & 0x0080_0000) != 0 {
            raw | (0xFF00_0000u32 as i32)
        } else {
            raw
        }
    })
}

/// Shared interleaving encoder: writes `width` bytes per sample produced by
/// `pack`, deinterleaving the planar input on the fly.
fn encode_pcm(
    frames: &[f32],
    channels: u16,
    full_scale: f32,
    width: usize,
    pack: impl Fn(i32) -> Vec<u8>,
) -> Vec<u8> {
    let ch = usize::from(channels);
    if ch == 0 {
        return Vec::new();
    }
    let num_frames = frames.len() / ch;
    let mut out = Vec::with_capacity(num_frames * ch * width);
    for i in 0..num_frames {
        for c in 0..ch {
            let sample = (frames[c * num_frames + i].clamp(-1.0, 1.0) * full_scale).round() as i32;
            out.extend_from_slice(&pack(sample));
        }
    }
    out
}

/// Shared interleaving decoder: reads `width` bytes per sample via `unpack`
/// (sign-extending as needed), then reinterleaves into the planar output.
fn decode_pcm(
    bytes: &[u8],
    channels: u16,
    decode_div: f32,
    width: usize,
    unpack: impl Fn(&[u8]) -> i32,
) -> Vec<f32> {
    let ch = usize::from(channels);
    if ch == 0 {
        return Vec::new();
    }
    let num_samples = bytes.len() / width;
    let num_frames = num_samples / ch;
    let mut out = vec![0.0f32; num_frames * ch];
    for i in 0..num_frames {
        for c in 0..ch {
            let idx = (i * ch + c) * width;
            let s = unpack(&bytes[idx..idx + width]);
            out[c * num_frames + i] = (s as f32) / decode_div;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_header_roundtrip() {
        let h = RtpHeader::new(96, 0x1234, 0xDEAD_BEEF, 0x0A0B_0C0D);
        let bytes = h.encode();
        let parsed = RtpHeader::parse(&bytes).expect("parse");
        assert_eq!(parsed, h);
        assert_eq!(parsed.version, RTP_VERSION);
        assert_eq!(parsed.payload_type, 96);
        assert_eq!(parsed.seq, 0x1234);
        assert_eq!(parsed.timestamp, 0xDEAD_BEEF);
        assert_eq!(parsed.ssrc, 0x0A0B_0C0D);
        assert_eq!(parsed.encoded_len(), RTP_HEADER_LEN);
    }

    #[test]
    fn rtp_header_rejects_bad_version() {
        // Build a buffer with version bits = 0 instead of 2.
        let h = RtpHeader::new(96, 1, 2, 3);
        let mut bytes = h.encode();
        bytes[0] = 0x00; // version=0, everything else zeroed
        assert!(RtpHeader::parse(&bytes).is_err());
    }

    #[test]
    fn rtp_header_truncated() {
        let short = [0u8; 8];
        assert!(RtpHeader::parse(&short).is_err());
    }

    #[test]
    fn rtp_packet_roundtrip_strips_and_restores_payload() {
        let header = RtpHeader::new(10, 42, 1000, 7);
        let payload = vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let packet = RtpPacket {
            header,
            payload: payload.clone(),
        };
        let bytes = packet.encode();
        assert!(bytes.len() > RTP_HEADER_LEN);
        let parsed = RtpPacket::parse(&bytes).expect("parse");
        assert_eq!(parsed.header, header);
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn l16_roundtrip_property() {
        // Two channels, four frames — planar layout [ch0 x4, ch1 x4].
        let planar = vec![
            0.0, 0.25, -0.5, 1.0, // ch0
            -1.0, 0.333, 0.75, 0.0, // ch1
        ];
        let bytes = encode_l16(&planar, 2);
        let back = decode_l16(&bytes, 2);
        assert_eq!(back.len(), planar.len());
        for (got, want) in back.iter().zip(planar.iter()) {
            assert!(
                (got - want).abs() < 1e-3,
                "sample mismatch: got {got}, want {want}"
            );
        }
    }

    #[test]
    fn l24_roundtrip() {
        let planar = vec![
            0.0, 0.123, -0.7, 1.0, // ch0
            -1.0, 0.5, 0.001, 0.9, // ch1
        ];
        let bytes = encode_l24(&planar, 2);
        let back = decode_l24(&bytes, 2);
        assert_eq!(back.len(), planar.len());
        for (got, want) in back.iter().zip(planar.iter()) {
            assert!(
                (got - want).abs() < 1e-6,
                "L24 sample mismatch: got {got}, want {want}"
            );
        }
    }

    #[test]
    fn l16_byte_order_known_vector() {
        // Full-scale +1.0 is an unambiguous known vector: it maps exactly to
        // i16 +32767 (0x7FFF) with the symmetric *32767 encoder, and its
        // big-endian repr proves high-byte-first ordering. (The task's suggested
        // 0.5 → 16383 vector was dropped: 0.5*32767 = 16383.5 rounds to 16384
        // under the mandated round() encoder, making 0.5 ambiguous.)
        let bytes = encode_l16(&[1.0], 1);
        assert_eq!(bytes, [0x7F, 0xFF]);

        // Negative full scale → -32767 → 0x8001 (two's complement, BE).
        let bytes = encode_l16(&[-1.0], 1);
        assert_eq!(bytes, [0x80, 0x01]);

        // Zero → 0x0000.
        assert_eq!(encode_l16(&[0.0], 1), [0x00, 0x00]);
    }

    #[test]
    fn l16_interleaves_planar_input() {
        // ch0=[L0,L1], ch1=[R0,R1] (planar). Interleaved wire order must be
        // [L0, R0, L1, R1].
        let planar = vec![0.0, 1.0, 0.0, 1.0]; // ch0=[0,0]? no: indices 0,1=ch0, 2,3=ch1
        let _ = planar; // keep layout explicit below instead.
        let p = vec![
            1.0, 0.0, // ch0: frame0=L0=1.0, frame1=L1=0.0
            0.0, 1.0, // ch1: frame0=R0=0.0, frame1=R1=1.0
        ];
        let bytes = encode_l16(&p, 2);
        // Expected interleaved i16 BE: L0=32767, R0=0, L1=0, R1=32767.
        let want: Vec<u8> = [0x7F, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x7F, 0xFF].to_vec();
        assert_eq!(bytes, want);
    }

    #[test]
    fn codec_payload_type_mapping() {
        assert_eq!(codec_to_payload_type(Codec::PcmL16, 2), 10);
        assert_eq!(codec_to_payload_type(Codec::PcmL16, 1), 11);
        assert_eq!(codec_to_payload_type(Codec::PcmL24, 2), 96);
        assert_eq!(codec_to_payload_type(Codec::Opus, 2), 97);
        assert_eq!(codec_to_payload_type(Codec::Custom(42), 2), 42);
    }

    #[test]
    fn bytes_per_sample_and_timestamp_advance() {
        assert_eq!(bytes_per_sample(Codec::PcmL16), 2);
        assert_eq!(bytes_per_sample(Codec::PcmL24), 3);
        // 4 stereo L16 samples = 16 bytes / (2 bytes * 2 ch) = 4 frames.
        assert_eq!(timestamp_advance(16, Codec::PcmL16, 2), 4);
    }
}
