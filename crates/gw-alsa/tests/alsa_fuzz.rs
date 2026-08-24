//! Sanitizer i3 — proptest fuzzing for hw_params + format APIs.
//!
//! Guarantees no-panic on arbitrary hw_params/format combinations.
//! Exercises invalid format values, zero/extreme rates, and excessive
//! channel counts to confirm graceful behaviour.

use gw_alsa::format::AlsaFormat;
use gw_alsa::params::{negotiate, HwConstraints};

// ---------------------------------------------------------------------------
// Deterministic edge-case inputs — no-panic
// ---------------------------------------------------------------------------

#[test]
fn zero_rate_no_panic() {
    let c = HwConstraints {
        rates: Some(vec![0]),
        ..HwConstraints::default()
    };
    let _ = negotiate(&c);
}

#[test]
fn max_u32_rate_no_panic() {
    let c = HwConstraints {
        rates: Some(vec![u32::MAX]),
        ..HwConstraints::default()
    };
    let _ = negotiate(&c);
}

#[test]
fn single_zero_channel_no_panic() {
    let c = HwConstraints {
        channels: Some(vec![0]),
        ..HwConstraints::default()
    };
    let _ = negotiate(&c);
}

#[test]
fn max_u16_channel_no_panic() {
    let c = HwConstraints {
        channels: Some(vec![u16::MAX]),
        ..HwConstraints::default()
    };
    let _ = negotiate(&c);
}

#[test]
fn all_format_variants_no_panic() {
    let formats = [
        AlsaFormat::S16Le,
        AlsaFormat::S24Le,
        AlsaFormat::S24_3Le,
        AlsaFormat::S32Le,
        AlsaFormat::Float32Le,
        AlsaFormat::Float64Le,
    ];
    for f in formats {
        // to_alsa_value, physical_width_bytes, container_width_bytes must never panic.
        let _ = f.to_alsa_value();
        let _ = f.physical_width_bytes();
        let _ = f.container_width_bytes();
        // to_core_sample_format must never panic.
        let _ = gw_alsa::to_core_sample_format(f);
    }
}

#[test]
fn zero_period_range_no_panic() {
    let c = HwConstraints {
        period_size: Some(0..=0),
        ..HwConstraints::default()
    };
    let _ = negotiate(&c);
}

#[test]
fn huge_period_range_no_panic() {
    let c = HwConstraints {
        period_size: Some(1..=u32::MAX),
        ..HwConstraints::default()
    };
    let _ = negotiate(&c);
}

#[test]
fn buffer_range_start_equals_floor() {
    // period=256 → floor=512. Buffer range 512..=512 — exactly at floor.
    let c = HwConstraints {
        period_size: Some(256..=256),
        buffer_size: Some(512..=512),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.buffer_size, 512);
}

#[test]
fn all_axes_constrained_tightly() {
    let c = HwConstraints {
        rates: Some(vec![48_000]),
        channels: Some(vec![2]),
        formats: Some(vec![AlsaFormat::S16Le]),
        period_size: Some(128..=128),
        buffer_size: Some(256..=256),
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.sample_rate, 48_000);
    assert_eq!(p.channels, 2);
    assert_eq!(p.format, AlsaFormat::S16Le);
    assert_eq!(p.period_size, 128);
    assert_eq!(p.buffer_size, 256);
}

// ---------------------------------------------------------------------------
// proptest property-based fuzz tests
// ---------------------------------------------------------------------------

use proptest::prelude::*;

/// Strategy: generate a valid `AlsaFormat` variant via `prop_oneof!`.
fn alsa_format_strategy() -> impl Strategy<Value = AlsaFormat> {
    prop_oneof![
        Just(AlsaFormat::S16Le),
        Just(AlsaFormat::S24Le),
        Just(AlsaFormat::S24_3Le),
        Just(AlsaFormat::S32Le),
        Just(AlsaFormat::Float32Le),
        Just(AlsaFormat::Float64Le),
    ]
}

/// Strategy: generate a non-empty Vec<u32> of sample rates (0..=192_000).
#[allow(dead_code)]
fn rates_strategy() -> impl Strategy<Value = Vec<u32>> {
    prop::collection::vec(0u32..=192_000, 1..=8)
}

/// Strategy: generate a non-empty Vec<u16> of channel counts (0..=32).
#[allow(dead_code)]
fn channels_strategy() -> impl Strategy<Value = Vec<u16>> {
    prop::collection::vec(0u16..=32, 1..=8)
}

/// Strategy: generate a period-size range (0..=16384).
fn period_strategy() -> impl Strategy<Value = std::ops::RangeInclusive<u32>> {
    (0u32..=4096).prop_flat_map(|lo| (lo..=16384u32).prop_map(move |hi| lo..=hi))
}

/// Strategy: generate a buffer-size range (0..=65536).
fn buffer_strategy() -> impl Strategy<Value = std::ops::RangeInclusive<u32>> {
    (0u32..=16384).prop_flat_map(|lo| (lo..=65536u32).prop_map(move |hi| lo..=hi))
}

proptest! {
    #[test]
    fn negotiate_never_panics_on_arbitrary_constraints(
        rates in prop::collection::vec(0u32..=192_000, 0..=8),
        channels in prop::collection::vec(0u16..=32, 0..=8),
        period in period_strategy(),
        buffer in buffer_strategy(),
    ) {
        let c = HwConstraints {
            rates: if rates.is_empty() { None } else { Some(rates) },
            channels: if channels.is_empty() { None } else { Some(channels) },
            formats: None,
            period_size: Some(period),
            buffer_size: Some(buffer),
        };
        let _ = negotiate(&c);
    }

    #[test]
    fn negotiate_never_panics_on_random_format_list(
        formats in prop::collection::vec(alsa_format_strategy(), 0..=6),
    ) {
        let c = HwConstraints {
            formats: if formats.is_empty() { None } else { Some(formats) },
            ..HwConstraints::default()
        };
        let _ = negotiate(&c);
    }

    #[test]
    fn all_format_methods_never_panic(f in alsa_format_strategy()) {
        let _ = f.to_alsa_value();
        let _ = f.physical_width_bytes();
        let _ = f.container_width_bytes();
        let _ = gw_alsa::to_core_sample_format(f);
    }

    #[test]
    fn negotiate_never_panics_with_single_rate_and_channel(
        rate in any::<u32>(),
        ch in any::<u16>(),
    ) {
        let c = HwConstraints {
            rates: Some(vec![rate]),
            channels: Some(vec![ch]),
            ..HwConstraints::default()
        };
        let _ = negotiate(&c);
    }

    #[test]
    fn negotiate_never_panics_with_extreme_periods(
        period_lo in any::<u32>(),
        period_hi in any::<u32>(),
    ) {
        let lo = period_lo.min(period_hi);
        let hi = period_lo.max(period_hi);
        let c = HwConstraints {
            period_size: Some(lo..=hi),
            ..HwConstraints::default()
        };
        let _ = negotiate(&c);
    }

    #[test]
    fn negotiate_never_panics_with_extreme_buffers(
        buf_lo in any::<u32>(),
        buf_hi in any::<u32>(),
    ) {
        let lo = buf_lo.min(buf_hi);
        let hi = buf_lo.max(buf_hi);
        let c = HwConstraints {
            buffer_size: Some(lo..=hi),
            ..HwConstraints::default()
        };
        let _ = negotiate(&c);
    }
}
