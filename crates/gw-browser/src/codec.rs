//! Binary wire format for the browser gateway WebSocket transport.
//!
//! Every message exchanged with a browser client is a single binary frame
//! prefixed by a small fixed header. The format is intentionally minimal and
//! self-describing so that a browser can emit/consume it from a small
//! `DataView` without out-of-band negotiation.
//!
//! # Header layout (6 bytes, little-endian)
//!
//! ```text
//!  offset  field         type     meaning
//!  ------  -----------   -------  -------------------------------------
//!  0       codec_tag     u8       0 = PCM f32 planar, 1 = Opus
//!  1       channels      u8       channel count (1 = mono, 2 = stereo)
//!  2..6    sample_rate   u32 LE   sample rate in Hz (e.g. 48_000)
//!  6..     payload       bytes    codec-specific sample bytes
//! ```
//!
//! (BUILD-PLAN §3.2 calls this a "5-byte header"; the documented field set
//! `u8 + u8 + u32` is in fact 6 bytes, so 6 is what is implemented.)
//!
//! ## PCM payload (`codec_tag == 0`)
//!
//! Raw little-endian `f32` samples in **planar** order
//! (`[ch0…, ch1…]`), matching [`audio_core_bsd::AudioFrame`]. The payload
//! length must be a non-zero multiple of `channels * 4`.
//!
//! ## Opus payload (`codec_tag == 1`)
//!
//! One RFC 6716 Opus packet. Decoded via [`audio_opus_bsd`] **only** when this
//! crate is built with the `opus` feature; otherwise an Opus-tagged frame is
//! rejected as [`crate::GatewayError::BadFrame`] (libopus is absent on the dev
//! host, so the feature is off by default).

use audio_core_bsd::AudioFrame;

use crate::{GatewayError, Result};

/// Length of the fixed binary header, in bytes.
pub const HEADER_LEN: usize = 6;
/// Codec tag for raw little-endian planar `f32` PCM.
pub const TAG_PCM: u8 = 0;
/// Codec tag for an Opus packet payload.
pub const TAG_OPUS: u8 = 1;

/// The transport's audio shape. Frames whose header disagrees with this are
/// rejected so a client cannot feed a stereo frame into a mono graph (or vice
/// versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSpec {
    /// Expected channel count of every frame.
    pub channels: u16,
    /// Expected sample rate, in Hz, of every frame.
    pub sample_rate: u32,
}

impl FrameSpec {
    /// Creates a new spec from `channels` and `sample_rate`.
    #[must_use]
    pub const fn new(channels: u16, sample_rate: u32) -> Self {
        Self {
            channels,
            sample_rate,
        }
    }
}

/// Encodes an [`AudioFrame`] as a PCM binary message ready to send to a client.
///
/// The frame's `channels` must fit in a `u8` (i.e. `<= 255`); otherwise the
/// encode is rejected as [`GatewayError::BadFrame`].
///
/// # Errors
///
/// Returns [`GatewayError::BadFrame`] only when the channel count overflows a
/// single header byte.
pub fn encode_frame(frame: &AudioFrame) -> Result<Vec<u8>> {
    let channels = u8::try_from(frame.channels)
        .map_err(|_| GatewayError::BadFrame("channel count overloads header byte".to_string()))?;
    let mut out = Vec::with_capacity(HEADER_LEN + frame.samples.len() * 4);
    out.push(TAG_PCM);
    out.push(channels);
    out.extend_from_slice(&frame.sample_rate.to_le_bytes());
    for &sample in &frame.samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(out)
}

/// Decodes a binary message into an [`AudioFrame`], validating it against
/// `spec`.
///
/// See the [module docs](self) for the wire layout. Unknown codec tags,
/// short headers, channel/sample-rate mismatches, and malformed payloads all
/// become [`GatewayError::BadFrame`].
///
/// # Errors
///
/// Returns [`GatewayError::BadFrame`] for any structurally invalid message.
pub fn decode_frame(bytes: &[u8], spec: FrameSpec) -> Result<AudioFrame> {
    let (bytes_len, header) = (bytes.len(), bytes);
    if bytes_len < HEADER_LEN {
        return Err(GatewayError::BadFrame(format!(
            "header shorter than {HEADER_LEN} bytes"
        )));
    }
    let tag = header[0];
    let channels = header[1];
    let sample_rate = u32::from_le_bytes([header[2], header[3], header[4], header[5]]);
    let payload = &bytes[HEADER_LEN..];
    match tag {
        TAG_PCM => decode_pcm(payload, channels, sample_rate, spec),
        TAG_OPUS => decode_opus(payload, channels, sample_rate, spec),
        other => Err(GatewayError::BadFrame(format!("unknown codec tag {other}"))),
    }
}

/// Parses a PCM (`codec_tag == 0`) payload.
fn decode_pcm(
    payload: &[u8],
    channels: u8,
    sample_rate: u32,
    spec: FrameSpec,
) -> Result<AudioFrame> {
    if channels == 0 {
        return Err(GatewayError::BadFrame(
            "pcm frame has zero channels".to_string(),
        ));
    }
    if u16::from(channels) != spec.channels {
        return Err(GatewayError::BadFrame(format!(
            "channel mismatch: header={channels} expected={}",
            spec.channels
        )));
    }
    if sample_rate != spec.sample_rate {
        return Err(GatewayError::BadFrame(format!(
            "sample-rate mismatch: header={sample_rate} expected={}",
            spec.sample_rate
        )));
    }
    if payload.is_empty() {
        return Err(GatewayError::BadFrame("pcm payload is empty".to_string()));
    }
    let stride = usize::from(channels) * 4;
    if payload.len() % stride != 0 {
        return Err(GatewayError::BadFrame(format!(
            "pcm payload length {} is not a multiple of {stride}",
            payload.len()
        )));
    }

    let mut samples = Vec::with_capacity(payload.len() / 4);
    for chunk in payload.chunks_exact(4) {
        let arr: [u8; 4] = chunk.try_into().expect("chunks_exact(4) yields 4 bytes");
        samples.push(f32::from_le_bytes(arr));
    }
    Ok(AudioFrame::from_planar(
        spec.channels,
        spec.sample_rate,
        samples,
    ))
}

/// Parses an Opus (`codec_tag == 1`) payload. Only present when the `opus`
/// feature is enabled; otherwise an Opus-tagged frame is a `BadFrame`.
#[cfg(feature = "opus")]
fn decode_opus(
    payload: &[u8],
    channels: u8,
    sample_rate: u32,
    spec: FrameSpec,
) -> Result<AudioFrame> {
    if u16::from(channels) != spec.channels {
        return Err(GatewayError::BadFrame(format!(
            "channel mismatch: header={channels} expected={}",
            spec.channels
        )));
    }
    if sample_rate != spec.sample_rate {
        return Err(GatewayError::BadFrame(format!(
            "sample-rate mismatch: header={sample_rate} expected={}",
            spec.sample_rate
        )));
    }
    let mut decoder = audio_opus_bsd::OpusDecoder::new(sample_rate, spec.channels)
        .map_err(|e| GatewayError::BadFrame(format!("opus decoder init failed: {e}")))?;
    decoder
        .decode_frame(payload)
        .map_err(|e| GatewayError::BadFrame(format!("opus decode failed: {e}")))
}

#[cfg(not(feature = "opus"))]
#[allow(clippy::needless_pass_by_value)] // signature mirrors the `opus` variant for symmetry
fn decode_opus(
    _payload: &[u8],
    _channels: u8,
    _sample_rate: u32,
    _spec: FrameSpec,
) -> Result<AudioFrame> {
    // The Opus codec links libopus (a C dependency). It is intentionally
    // gated behind the `opus` feature, which is off by default.
    Err(GatewayError::BadFrame(
        "opus codec not enabled (build with the `opus` feature)".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GatewayError;
    use audio_core_bsd::AudioFrame;

    const SPEC: FrameSpec = FrameSpec::new(2, 48_000);

    fn is_bad(err: Result<AudioFrame>) -> bool {
        matches!(err, Err(GatewayError::BadFrame(_)))
    }

    #[test]
    fn round_trip_pcm_preserves_samples_and_metadata() {
        let original = AudioFrame::from_planar(2, 48_000, vec![0.1, -0.2, 0.3, -0.4]);
        let bytes = encode_frame(&original).unwrap();
        let decoded = decode_frame(&bytes, SPEC).unwrap();
        assert_eq!(decoded.channels, original.channels);
        assert_eq!(decoded.sample_rate, original.sample_rate);
        assert_eq!(decoded.samples.len(), original.samples.len());
        for (got, want) in decoded.samples.iter().zip(&original.samples) {
            assert!(((got - want).abs()) < 1e-6);
        }
    }

    #[test]
    fn encode_emits_documented_header_and_little_endian_payload() {
        // 1 channel, 48 kHz, one sample of 0.5.
        let frame = AudioFrame::from_planar(1, 48_000, vec![0.5]);
        let bytes = encode_frame(&frame).unwrap();
        // tag=0, ch=1, sr=48000 LE = 80 BB 00 00, 0.5f32 LE = 00 00 00 3F
        assert_eq!(
            bytes,
            vec![0x00, 0x01, 0x80, 0xBB, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3F]
        );
    }

    #[test]
    fn decode_mono_frame_on_mono_spec_round_trips() {
        let mono_spec = FrameSpec::new(1, 44_100);
        let frame = AudioFrame::from_planar(1, 44_100, vec![0.0, 1.0, -1.0]);
        let bytes = encode_frame(&frame).unwrap();
        let decoded = decode_frame(&bytes, mono_spec).unwrap();
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.sample_rate, 44_100);
        assert_eq!(decoded.samples, vec![0.0, 1.0, -1.0]);
    }

    #[test]
    fn decode_short_header_is_bad_frame() {
        assert!(is_bad(decode_frame(&[0u8, 1, 2, 3, 4], SPEC)));
    }

    #[test]
    fn decode_unknown_tag_is_bad_frame() {
        let bytes = encode_frame(&AudioFrame::silence(2, 8, 48_000)).unwrap();
        let mut bad = bytes;
        bad[0] = 0x42; // unknown tag
        assert!(is_bad(decode_frame(&bad, SPEC)));
    }

    #[test]
    fn decode_pcm_channel_mismatch_is_bad_frame() {
        // Header says 1 channel but spec wants 2.
        let bytes = encode_frame(&AudioFrame::silence(1, 8, 48_000)).unwrap();
        assert!(is_bad(decode_frame(&bytes, SPEC)));
    }

    #[test]
    fn decode_pcm_sample_rate_mismatch_is_bad_frame() {
        let bytes = encode_frame(&AudioFrame::silence(2, 8, 44_100)).unwrap();
        assert!(is_bad(decode_frame(&bytes, SPEC)));
    }

    #[test]
    fn decode_pcm_unaligned_payload_is_bad_frame() {
        // Build a valid header (2 ch, 48 kHz) then append a payload whose
        // length is not a multiple of channels*4 (= 8).
        let mut bytes = vec![TAG_PCM, 2];
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 5]); // 5 bytes: not a multiple of 8
        assert!(is_bad(decode_frame(&bytes, SPEC)));
    }

    #[test]
    fn decode_pcm_empty_payload_is_bad_frame() {
        let mut bytes = vec![TAG_PCM, 2];
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        // no payload
        assert!(is_bad(decode_frame(&bytes, SPEC)));
    }

    #[test]
    fn decode_pcm_zero_channels_is_bad_frame() {
        let mut bytes = vec![TAG_PCM, 0];
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        assert!(is_bad(decode_frame(&bytes, SPEC)));
    }

    #[test]
    fn decode_opus_tag_is_bad_frame_without_feature() {
        // Tests always run with default features (opus OFF). With the feature
        // enabled this would instead try to decode (and fail on garbage), but
        // the no-libopus host guarantees the feature is off here.
        let mut bytes = vec![TAG_OPUS, 2];
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        let res = decode_frame(&bytes, SPEC);
        if cfg!(feature = "opus") {
            assert!(is_bad(res), "garbage opus payload must still be rejected");
        } else {
            assert!(is_bad(res), "opus tag without feature must be BadFrame");
        }
    }

    #[test]
    fn header_and_tag_constants_are_stable() {
        assert_eq!(HEADER_LEN, 6);
        assert_eq!(TAG_PCM, 0);
        assert_eq!(TAG_OPUS, 1);
    }
}
