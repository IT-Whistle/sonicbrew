//! M11 — ALSA PCM sample-format representation and core-format mapping.
//!
//! Pure-Rust subset of the ALSA `snd_pcm_format_t` enum (see
//! `<alsa/pcm.h>`): the common linear-PCM formats used by professional audio.
//! Compressed µ-law/A-law and exotic-endian variants are intentionally
//! excluded. This module links nothing but the standard library and
//! `audio-core-bsd` — no `libasound` / `alsa-sys` dependency is pulled in at
//! this layer.

use audio_core_bsd::SampleFormat;

/// Subset of ALSA `snd_pcm_format_t` (see `<alsa/pcm.h>`).
///
/// Compressed µ-law/A-law formats are intentionally excluded. Variant names
/// mirror the ALSA enum; the wire `snd_pcm_format_t` integer is recoverable
/// via [`to_alsa_value`](Self::to_alsa_value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlsaFormat {
    /// `SND_PCM_FORMAT_S16_LE` = 2. Signed 16-bit little-endian.
    S16Le,
    /// `SND_PCM_FORMAT_S24_LE` = 6. Signed 24-bit in a 4-byte container,
    /// LSB-aligned (the low byte is padding).
    S24Le,
    /// `SND_PCM_FORMAT_S24_3LE` = 32. Signed 24-bit packed into 3 bytes
    /// (no container padding).
    S24_3Le,
    /// `SND_PCM_FORMAT_S32_LE` = 10. Signed 32-bit little-endian.
    S32Le,
    /// `SND_PCM_FORMAT_FLOAT_LE` = 14. 32-bit IEEE-754 float.
    Float32Le,
    /// `SND_PCM_FORMAT_FLOAT64_LE` = 15. 64-bit IEEE-754 float.
    Float64Le,
}

impl AlsaFormat {
    /// The wire `snd_pcm_format_t` integer value (matching `<alsa/pcm.h>`).
    #[must_use]
    pub fn to_alsa_value(self) -> u32 {
        match self {
            AlsaFormat::S16Le => 2,
            AlsaFormat::S24Le => 6,
            AlsaFormat::S24_3Le => 32,
            AlsaFormat::S32Le => 10,
            AlsaFormat::Float32Le => 14,
            AlsaFormat::Float64Le => 15,
        }
    }

    /// Physical sample width in bytes on the wire — the bytes actually
    /// carrying audio data, excluding any container padding.
    ///
    /// `S24Le` and `S24_3Le` both carry 24 bits (3 bytes) of audio; `S24Le`
    /// merely stores them padded inside a 4-byte slot (see
    /// [`container_width_bytes`](Self::container_width_bytes)).
    #[must_use]
    pub fn physical_width_bytes(self) -> u8 {
        match self {
            AlsaFormat::S16Le => 2,
            AlsaFormat::S24Le | AlsaFormat::S24_3Le => 3,
            AlsaFormat::S32Le | AlsaFormat::Float32Le => 4,
            AlsaFormat::Float64Le => 8,
        }
    }

    /// Container width in bytes — the storage slot per sample, including any
    /// LSB-aligned padding. Packed formats (`S24_3Le`) have container ==
    /// physical; padded formats (`S24Le`) widen the container.
    #[must_use]
    pub fn container_width_bytes(self) -> u8 {
        match self {
            // The only variant whose container is wider than its payload.
            AlsaFormat::S24Le => 4,
            _ => self.physical_width_bytes(),
        }
    }
}

/// Map an ALSA format onto an [`audio_core_bsd::SampleFormat`].
///
/// Returns `None` where no lossless core representation exists:
///   - `S24Le` / `S24_3Le` — there is no 24-bit core variant. The core engine
///     is `F32`-native; 24-bit PCM must be widened/converted by the caller.
///
/// `S32Le` maps to `F32` **lossily**: the 24-bit mantissa of `f32` cannot
/// hold the full 32-bit integer range. This keeps the core engine in its
/// native `F32` lane; a future converter node may upcast losslessly.
#[must_use]
pub fn to_core_sample_format(f: AlsaFormat) -> Option<SampleFormat> {
    match f {
        AlsaFormat::S16Le => Some(SampleFormat::I16),
        AlsaFormat::S32Le | AlsaFormat::Float32Le => Some(SampleFormat::F32),
        AlsaFormat::Float64Le => Some(SampleFormat::F64),
        AlsaFormat::S24Le | AlsaFormat::S24_3Le => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alsa_format_values_and_widths() {
        // snd_pcm_format_t wire values.
        assert_eq!(AlsaFormat::S16Le.to_alsa_value(), 2);
        assert_eq!(AlsaFormat::S24Le.to_alsa_value(), 6);
        assert_eq!(AlsaFormat::S24_3Le.to_alsa_value(), 32);
        assert_eq!(AlsaFormat::S32Le.to_alsa_value(), 10);
        assert_eq!(AlsaFormat::Float32Le.to_alsa_value(), 14);
        assert_eq!(AlsaFormat::Float64Le.to_alsa_value(), 15);

        // Physical widths: the bytes of actual audio data.
        assert_eq!(AlsaFormat::S16Le.physical_width_bytes(), 2);
        assert_eq!(AlsaFormat::S24Le.physical_width_bytes(), 3);
        assert_eq!(AlsaFormat::S24_3Le.physical_width_bytes(), 3);
        assert_eq!(AlsaFormat::S32Le.physical_width_bytes(), 4);
        assert_eq!(AlsaFormat::Float32Le.physical_width_bytes(), 4);
        assert_eq!(AlsaFormat::Float64Le.physical_width_bytes(), 8);

        // Container widths: storage slot including padding.
        assert_eq!(AlsaFormat::S16Le.container_width_bytes(), 2);
        // The headline distinction: S24Le pads to a 4-byte container while
        // the packed S24_3Le stays at 3.
        assert_eq!(AlsaFormat::S24Le.container_width_bytes(), 4);
        assert_eq!(AlsaFormat::S24_3Le.container_width_bytes(), 3);
        assert_eq!(AlsaFormat::S32Le.container_width_bytes(), 4);
        assert_eq!(AlsaFormat::Float32Le.container_width_bytes(), 4);
        assert_eq!(AlsaFormat::Float64Le.container_width_bytes(), 8);
    }

    #[test]
    fn core_format_mapping() {
        use audio_core_bsd::SampleFormat;
        assert_eq!(
            to_core_sample_format(AlsaFormat::S16Le),
            Some(SampleFormat::I16)
        );
        assert_eq!(
            to_core_sample_format(AlsaFormat::Float32Le),
            Some(SampleFormat::F32)
        );
        assert_eq!(
            to_core_sample_format(AlsaFormat::Float64Le),
            Some(SampleFormat::F64)
        );
        // S32Le → F32 (documented lossy).
        assert_eq!(
            to_core_sample_format(AlsaFormat::S32Le),
            Some(SampleFormat::F32)
        );
        // 24-bit has no core representation.
        assert_eq!(to_core_sample_format(AlsaFormat::S24Le), None);
        assert_eq!(to_core_sample_format(AlsaFormat::S24_3Le), None);
    }
}
