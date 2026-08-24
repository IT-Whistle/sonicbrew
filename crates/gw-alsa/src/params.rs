//! M11 — ALSA `hw_params` negotiation (pure constraint reduction).
//!
//! Mirrors the ALSA `snd_pcm_hw_params` negotiation loop as a pure,
//! deterministic function over [`HwConstraints`]: no `libasound` call is
//! made. The policy reduces the allowed set to a single "best" concrete
//! [`HwParams`] (see [`negotiate`]).
//!
//! # Selection policy
//!
//! Each axis is reduced independently with a fixed, documented preference:
//!
//! | axis         | unconstrained (`None`) | preference among the allowed set    |
//! |--------------|------------------------|-------------------------------------|
//! | sample rate  | 48 000 Hz              | 48 000 > 44 100 > lowest remaining  |
//! | channels     | stereo (2)             | 2 > 1 > lowest remaining            |
//! | format       | `S16Le`                | first listed (caller's order)       |
//! | period size  | 256 frames             | smallest in range                   |
//! | buffer size  | `2 * period`           | smallest in range, floored at       |
//! |              |                        | `2 * period`                        |
//!
//! An explicit-but-empty constraint (e.g. `rates = Some(vec![])`) means "no
//! value is permitted" and yields the matching [`NegotiateError`]; `None`
//! means "unconstrained" and falls back to the policy default.

use std::ops::RangeInclusive;

use crate::format::AlsaFormat;

/// Default rate set applied when `rates` is `None`, in preference order.
const DEFAULT_RATES: [u32; 2] = [48_000, 44_100];
/// Default channel set applied when `channels` is `None`, in preference order.
const DEFAULT_CHANNELS: [u16; 2] = [2, 1];
/// Default period size (frames) applied when `period_size` is `None`.
const DEFAULT_PERIOD: u32 = 256;

/// Requested PCM hardware parameters with optional constraints.
///
/// Each axis is `None` when unconstrained; an explicit empty `Vec` /
/// inverted range means "nothing permitted" (see [`negotiate`] errors).
#[derive(Debug, Clone, Default)]
pub struct HwConstraints {
    /// Allowed sample rates; `None` = unconstrained (falls back to the
    /// default-preference list).
    pub rates: Option<Vec<u32>>,
    /// Allowed channel counts; `None` = unconstrained.
    pub channels: Option<Vec<u16>>,
    /// Allowed sample formats; `None` = unconstrained.
    pub formats: Option<Vec<AlsaFormat>>,
    /// Allowed period-size range (inclusive); `None` = unconstrained.
    pub period_size: Option<RangeInclusive<u32>>,
    /// Allowed buffer-size range (inclusive); `None` = unconstrained.
    pub buffer_size: Option<RangeInclusive<u32>>,
}

/// A fully-resolved PCM configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwParams {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Sample format.
    pub format: AlsaFormat,
    /// Period size in frames.
    pub period_size: u32,
    /// Buffer size in frames.
    pub buffer_size: u32,
}

/// Errors returned by [`negotiate`].
#[derive(Debug, thiserror::Error)]
pub enum NegotiateError {
    /// No common sample rate in the constraint set.
    #[error("no common sample rate")]
    NoRate,
    /// No common channel count in the constraint set.
    #[error("no common channel count")]
    NoChannels,
    /// No common sample format in the constraint set.
    #[error("no common format")]
    NoFormat,
    /// No period size satisfies the range.
    #[error("no period size in range")]
    NoPeriod,
    /// Resolved buffer is smaller than twice the period.
    #[error("buffer must be >= 2*period")]
    BufferTooSmall,
}

/// Reduce `constraints` to a concrete [`HwParams`].
///
/// Pure and deterministic; see the module-level [selection policy](self).
///
/// # Errors
///
/// Returns the matching [`NegotiateError`] when an axis is explicitly empty
/// or when the buffer range cannot satisfy the `>= 2 * period` latency floor.
pub fn negotiate(c: &HwConstraints) -> std::result::Result<HwParams, NegotiateError> {
    let sample_rate = pick_rate(&c.rates)?;
    let channels = pick_channels(&c.channels)?;
    let format = pick_format(&c.formats)?;
    let period_size = pick_period(&c.period_size)?;

    // Latency floor: a sane ALSA buffer holds at least two full periods.
    let two_period = period_size.saturating_mul(2);
    let buffer_size = pick_buffer(&c.buffer_size, two_period)?;

    Ok(HwParams {
        sample_rate,
        channels,
        format,
        period_size,
        buffer_size,
    })
}

/// Pick the best sample rate (48 000 > 44 100 > lowest).
fn pick_rate(rates: &Option<Vec<u32>>) -> std::result::Result<u32, NegotiateError> {
    let allowed: &[u32] = match rates {
        None => &DEFAULT_RATES,
        Some(v) => v.as_slice(),
    };
    if allowed.is_empty() {
        return Err(NegotiateError::NoRate);
    }
    if allowed.contains(&48_000) {
        Ok(48_000)
    } else if allowed.contains(&44_100) {
        Ok(44_100)
    } else {
        allowed.iter().copied().min().ok_or(NegotiateError::NoRate)
    }
}

/// Pick the best channel count (2 > 1 > lowest).
fn pick_channels(channels: &Option<Vec<u16>>) -> std::result::Result<u16, NegotiateError> {
    let allowed: &[u16] = match channels {
        None => &DEFAULT_CHANNELS,
        Some(v) => v.as_slice(),
    };
    if allowed.is_empty() {
        return Err(NegotiateError::NoChannels);
    }
    if allowed.contains(&2) {
        Ok(2)
    } else if allowed.contains(&1) {
        Ok(1)
    } else {
        allowed
            .iter()
            .copied()
            .min()
            .ok_or(NegotiateError::NoChannels)
    }
}

/// Pick the format (first allowed; caller's order is authoritative).
fn pick_format(
    formats: &Option<Vec<AlsaFormat>>,
) -> std::result::Result<AlsaFormat, NegotiateError> {
    let allowed: &[AlsaFormat] = match formats {
        None => &[AlsaFormat::S16Le],
        Some(v) => v.as_slice(),
    };
    allowed.first().copied().ok_or(NegotiateError::NoFormat)
}

/// Pick the period size (smallest in range; default 256).
fn pick_period(range: &Option<RangeInclusive<u32>>) -> std::result::Result<u32, NegotiateError> {
    match range {
        None => Ok(DEFAULT_PERIOD),
        Some(r) => {
            // Empty (inverted) range: nothing permitted.
            if r.start() > r.end() {
                return Err(NegotiateError::NoPeriod);
            }
            Ok(*r.start())
        }
    }
}

/// Pick the buffer size: the smallest value in range, but never below
/// `two_period`. If the range's maximum is below the latency floor the
/// constraint set is unsatisfiable.
fn pick_buffer(
    range: &Option<RangeInclusive<u32>>,
    two_period: u32,
) -> std::result::Result<u32, NegotiateError> {
    match range {
        None => Ok(two_period),
        Some(r) => {
            // Even the largest permitted buffer can't hold two periods.
            if *r.end() < two_period {
                return Err(NegotiateError::BufferTooSmall);
            }
            // Smallest in range, floored at the latency requirement.
            Ok((*r.start()).max(two_period))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_picks_48k_stereo() {
        let c = HwConstraints {
            rates: Some(vec![44_100, 48_000]),
            channels: Some(vec![1, 2]),
            formats: Some(vec![AlsaFormat::S16Le]),
            ..HwConstraints::default()
        };
        let p = negotiate(&c).expect("ok");
        assert_eq!(p.sample_rate, 48_000);
        assert_eq!(p.channels, 2);
        assert_eq!(p.format, AlsaFormat::S16Le);
    }

    #[test]
    fn negotiate_prefers_48k_over_44100() {
        let c = HwConstraints {
            rates: Some(vec![44_100, 48_000]),
            ..HwConstraints::default()
        };
        assert_eq!(negotiate(&c).unwrap().sample_rate, 48_000);
    }

    #[test]
    fn negotiate_buffer_ge_2x_period() {
        let c = HwConstraints {
            period_size: Some(64..=256),
            ..HwConstraints::default()
        };
        let p = negotiate(&c).expect("ok");
        assert_eq!(p.period_size, 64);
        assert!(p.buffer_size >= 2 * p.period_size);
    }

    #[test]
    fn negotiate_rejects_empty_rate() {
        let c = HwConstraints {
            rates: Some(vec![]),
            ..HwConstraints::default()
        };
        assert!(matches!(negotiate(&c), Err(NegotiateError::NoRate)));
    }

    #[test]
    fn negotiate_defaults_when_unconstrained() {
        // Every axis None: policy defaults kick in.
        let p = negotiate(&HwConstraints::default()).expect("ok");
        assert_eq!(p.sample_rate, 48_000);
        assert_eq!(p.channels, 2);
        assert_eq!(p.format, AlsaFormat::S16Le);
        assert_eq!(p.period_size, DEFAULT_PERIOD);
        assert_eq!(p.buffer_size, 2 * DEFAULT_PERIOD);
    }

    #[test]
    fn negotiate_picks_44100_when_48k_absent() {
        let c = HwConstraints {
            rates: Some(vec![22_050, 44_100]),
            ..HwConstraints::default()
        };
        assert_eq!(negotiate(&c).unwrap().sample_rate, 44_100);
    }

    #[test]
    fn negotiate_falls_back_to_lowest_rate() {
        let c = HwConstraints {
            rates: Some(vec![96_000, 22_050]),
            ..HwConstraints::default()
        };
        assert_eq!(negotiate(&c).unwrap().sample_rate, 22_050);
    }

    #[test]
    fn negotiate_rejects_empty_channels_and_format() {
        let c = HwConstraints {
            channels: Some(vec![]),
            ..HwConstraints::default()
        };
        assert!(matches!(negotiate(&c), Err(NegotiateError::NoChannels)));

        let c = HwConstraints {
            formats: Some(vec![]),
            ..HwConstraints::default()
        };
        assert!(matches!(negotiate(&c), Err(NegotiateError::NoFormat)));
    }

    #[test]
    fn negotiate_rejects_buffer_range_below_2x_period() {
        // period = 64 → floor 128; buffer range tops out at 10.
        let c = HwConstraints {
            period_size: Some(64..=256),
            buffer_size: Some(1..=10),
            ..HwConstraints::default()
        };
        assert!(matches!(negotiate(&c), Err(NegotiateError::BufferTooSmall)));
    }

    #[test]
    // The inverted literal is the point: it exercises `pick_period`'s empty
    // (start > end) branch. Silence clippy's `reversed_empty_ranges`, which
    // otherwise treats this as an accidental iteration bug.
    #[allow(clippy::reversed_empty_ranges)]
    fn negotiate_rejects_inverted_period_range() {
        let c = HwConstraints {
            period_size: Some(256..=64),
            ..HwConstraints::default()
        };
        assert!(matches!(negotiate(&c), Err(NegotiateError::NoPeriod)));
    }
}
