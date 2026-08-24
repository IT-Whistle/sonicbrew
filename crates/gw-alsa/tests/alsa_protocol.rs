//! Protocol i4 — ALSA PCM format mapping + hw_params negotiation deep tests.
//!
//! These integration tests exercise the public API of `gw-alsa` from an
//! external crate perspective. The scenarios are:
//!
//! (a) `snd_pcm_format_t` → sonicbrew `AlsaFormat` mapping
//! (b) `hw_params` constraint reduction negotiation
//! (c) channel / rate / format combination matrix
//! (d) ALSA PCM plugin interface contract (gateway + graph wiring)

use gw_alsa::format::AlsaFormat;
use gw_alsa::params::{negotiate, HwConstraints, HwParams, NegotiateError};
use gw_alsa::{AlsaGateway, Gateway, GatewayError, Result};

// =========================================================================
// (a) snd_pcm_format_t → sonicbrew format mapping
// =========================================================================

#[test]
fn s16le_wire_value_is_2() {
    assert_eq!(AlsaFormat::S16Le.to_alsa_value(), 2);
}

#[test]
fn s24le_wire_value_is_6() {
    assert_eq!(AlsaFormat::S24Le.to_alsa_value(), 6);
}

#[test]
fn s24_3le_wire_value_is_32() {
    assert_eq!(AlsaFormat::S24_3Le.to_alsa_value(), 32);
}

#[test]
fn s32le_wire_value_is_10() {
    assert_eq!(AlsaFormat::S32Le.to_alsa_value(), 10);
}

#[test]
fn float32le_wire_value_is_14() {
    assert_eq!(AlsaFormat::Float32Le.to_alsa_value(), 14);
}

#[test]
fn float64le_wire_value_is_15() {
    assert_eq!(AlsaFormat::Float64Le.to_alsa_value(), 15);
}

#[test]
fn s16le_core_mapping_is_i16() {
    use audio_core_bsd::SampleFormat;
    assert_eq!(
        gw_alsa::to_core_sample_format(AlsaFormat::S16Le),
        Some(SampleFormat::I16)
    );
}

#[test]
fn s32le_core_mapping_is_f32_lossy() {
    use audio_core_bsd::SampleFormat;
    // S32Le → F32 is documented lossy (24-bit mantissa can't hold full i32 range).
    assert_eq!(
        gw_alsa::to_core_sample_format(AlsaFormat::S32Le),
        Some(SampleFormat::F32)
    );
}

#[test]
fn float32le_core_mapping_is_f32() {
    use audio_core_bsd::SampleFormat;
    assert_eq!(
        gw_alsa::to_core_sample_format(AlsaFormat::Float32Le),
        Some(SampleFormat::F32)
    );
}

#[test]
fn float64le_core_mapping_is_f64() {
    use audio_core_bsd::SampleFormat;
    assert_eq!(
        gw_alsa::to_core_sample_format(AlsaFormat::Float64Le),
        Some(SampleFormat::F64)
    );
}

#[test]
fn s24le_has_no_core_mapping() {
    assert_eq!(gw_alsa::to_core_sample_format(AlsaFormat::S24Le), None);
}

#[test]
fn s24_3le_has_no_core_mapping() {
    assert_eq!(gw_alsa::to_core_sample_format(AlsaFormat::S24_3Le), None);
}

#[test]
fn all_formats_have_distinct_wire_values() {
    let formats = [
        AlsaFormat::S16Le,
        AlsaFormat::S24Le,
        AlsaFormat::S24_3Le,
        AlsaFormat::S32Le,
        AlsaFormat::Float32Le,
        AlsaFormat::Float64Le,
    ];
    let mut values: Vec<u32> = formats.iter().map(|f| f.to_alsa_value()).collect();
    values.sort();
    values.dedup();
    assert_eq!(values.len(), formats.len(), "wire values must be unique");
}

#[test]
fn s24le_container_wider_than_physical() {
    // S24Le: 3 bytes physical, 4 bytes container (LSB-aligned padding).
    assert_eq!(AlsaFormat::S24Le.physical_width_bytes(), 3);
    assert_eq!(AlsaFormat::S24Le.container_width_bytes(), 4);
}

#[test]
fn s24_3le_container_equals_physical() {
    // S24_3Le: 3 bytes physical, 3 bytes container (packed, no padding).
    assert_eq!(AlsaFormat::S24_3Le.physical_width_bytes(), 3);
    assert_eq!(AlsaFormat::S24_3Le.container_width_bytes(), 3);
}

#[test]
fn float64le_widest_format() {
    assert_eq!(AlsaFormat::Float64Le.physical_width_bytes(), 8);
    assert_eq!(AlsaFormat::Float64Le.container_width_bytes(), 8);
}

// =========================================================================
// (b) hw_params constraint reduction negotiation
// =========================================================================

#[test]
fn negotiate_respects_single_rate_constraint() {
    let c = HwConstraints {
        rates: Some(vec![44_100]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.sample_rate, 44_100);
}

#[test]
fn negotiate_respects_single_channel_constraint() {
    let c = HwConstraints {
        channels: Some(vec![1]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.channels, 1);
}

#[test]
fn negotiate_respects_single_format_constraint() {
    let c = HwConstraints {
        formats: Some(vec![AlsaFormat::Float32Le]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.format, AlsaFormat::Float32Le);
}

#[test]
fn negotiate_prefers_first_format_in_list() {
    let c = HwConstraints {
        formats: Some(vec![AlsaFormat::S32Le, AlsaFormat::S16Le]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    // Caller's order is authoritative — first listed wins.
    assert_eq!(p.format, AlsaFormat::S32Le);
}

#[test]
fn negotiate_period_picks_smallest_in_range() {
    let c = HwConstraints {
        period_size: Some(128..=1024),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.period_size, 128);
}

#[test]
fn negotiate_buffer_floor_at_2x_period() {
    // Period = 256 → floor = 512. Buffer range starts at 64 but floor wins.
    let c = HwConstraints {
        period_size: Some(256..=256),
        buffer_size: Some(64..=4096),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.period_size, 256);
    assert_eq!(p.buffer_size, 512);
}

#[test]
fn negotiate_buffer_range_satisfies_2x() {
    // Period = 64 → floor = 128. Buffer range 200..=800 — smallest ≥ 128 is 200.
    let c = HwConstraints {
        period_size: Some(64..=64),
        buffer_size: Some(200..=800),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.period_size, 64);
    assert_eq!(p.buffer_size, 200);
    assert!(p.buffer_size >= 2 * p.period_size);
}

#[test]
fn negotiate_rejects_no_rate() {
    let c = HwConstraints {
        rates: Some(vec![]),
        ..HwConstraints::default()
    };
    assert!(matches!(negotiate(&c), Err(NegotiateError::NoRate)));
}

#[test]
fn negotiate_rejects_no_channels() {
    let c = HwConstraints {
        channels: Some(vec![]),
        ..HwConstraints::default()
    };
    assert!(matches!(negotiate(&c), Err(NegotiateError::NoChannels)));
}

#[test]
fn negotiate_rejects_no_format() {
    let c = HwConstraints {
        formats: Some(vec![]),
        ..HwConstraints::default()
    };
    assert!(matches!(negotiate(&c), Err(NegotiateError::NoFormat)));
}

#[test]
#[allow(clippy::reversed_empty_ranges)] // intentionally invalid (empty) range
fn negotiate_rejects_no_period_range() {
    let c = HwConstraints {
        period_size: Some(512..=128),
        ..HwConstraints::default()
    };
    assert!(matches!(negotiate(&c), Err(NegotiateError::NoPeriod)));
}

#[test]
fn negotiate_rejects_buffer_below_2x_period() {
    let c = HwConstraints {
        period_size: Some(256..=256),
        buffer_size: Some(1..=511),
        ..HwConstraints::default()
    };
    assert!(matches!(negotiate(&c), Err(NegotiateError::BufferTooSmall)));
}

#[test]
fn negotiate_buffer_at_exactly_2x_period() {
    let c = HwConstraints {
        period_size: Some(128..=128),
        buffer_size: Some(256..=256),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.buffer_size, 256);
    assert_eq!(p.buffer_size, 2 * p.period_size);
}

// =========================================================================
// (c) channel / rate / format combination matrix
// =========================================================================

#[test]
fn matrix_48k_stereo_s16le() {
    let c = HwConstraints {
        rates: Some(vec![48_000]),
        channels: Some(vec![2]),
        formats: Some(vec![AlsaFormat::S16Le]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.sample_rate, 48_000);
    assert_eq!(p.channels, 2);
    assert_eq!(p.format, AlsaFormat::S16Le);
}

#[test]
fn matrix_44100_mono_s32le() {
    let c = HwConstraints {
        rates: Some(vec![44_100]),
        channels: Some(vec![1]),
        formats: Some(vec![AlsaFormat::S32Le]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.sample_rate, 44_100);
    assert_eq!(p.channels, 1);
    assert_eq!(p.format, AlsaFormat::S32Le);
}

#[test]
fn matrix_96k_stereo_float64le() {
    let c = HwConstraints {
        rates: Some(vec![96_000]),
        channels: Some(vec![2]),
        formats: Some(vec![AlsaFormat::Float64Le]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.sample_rate, 96_000);
    assert_eq!(p.channels, 2);
    assert_eq!(p.format, AlsaFormat::Float64Le);
}

#[test]
fn matrix_22050_mono_s24_3le() {
    let c = HwConstraints {
        rates: Some(vec![22_050]),
        channels: Some(vec![1]),
        formats: Some(vec![AlsaFormat::S24_3Le]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.sample_rate, 22_050);
    assert_eq!(p.channels, 1);
    assert_eq!(p.format, AlsaFormat::S24_3Le);
}

#[test]
fn matrix_multi_rate_prefers_48k() {
    let c = HwConstraints {
        rates: Some(vec![22_050, 44_100, 48_000, 96_000]),
        channels: Some(vec![1, 2]),
        formats: Some(vec![AlsaFormat::S16Le, AlsaFormat::S32Le]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.sample_rate, 48_000);
    assert_eq!(p.channels, 2);
    assert_eq!(p.format, AlsaFormat::S16Le);
}

#[test]
fn matrix_no_48k_prefers_44100() {
    let c = HwConstraints {
        rates: Some(vec![22_050, 44_100, 96_000]),
        channels: Some(vec![2]),
        formats: Some(vec![AlsaFormat::S16Le]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.sample_rate, 44_100);
}

#[test]
fn matrix_no_48k_no_44100_prefers_lowest() {
    let c = HwConstraints {
        rates: Some(vec![96_000, 22_050]),
        channels: Some(vec![1]),
        formats: Some(vec![AlsaFormat::Float32Le]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.sample_rate, 22_050);
    assert_eq!(p.channels, 1);
    assert_eq!(p.format, AlsaFormat::Float32Le);
}

#[test]
fn matrix_single_mono_no_stereo_prefers_1() {
    let c = HwConstraints {
        channels: Some(vec![1]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    assert_eq!(p.channels, 1);
}

#[test]
fn matrix_high_channel_count_falls_back_to_lowest() {
    let c = HwConstraints {
        channels: Some(vec![8, 4, 6]),
        ..HwConstraints::default()
    };
    let p = negotiate(&c).expect("ok");
    // None contain 2 or 1, so lowest (4) wins.
    assert_eq!(p.channels, 4);
}

// =========================================================================
// (d) ALSA PCM plugin interface contract
// =========================================================================

#[test]
fn gateway_register_returns_two_distinct_nodes() {
    let gw = AlsaGateway::new();
    let mut graph = gw_alsa::Graph::new();
    let (src, sink, _inbound, _outbound) = gw.register(&mut graph).expect("register ok");
    assert_ne!(src, sink);
}

#[test]
fn gateway_register_links_source_to_sink() {
    let gw = AlsaGateway::new();
    let mut graph = gw_alsa::Graph::new();
    let (src, sink, _inbound, _outbound) = gw.register(&mut graph).expect("register ok");
    assert!(graph.link((src, 0), (sink, 0)).is_ok());
}

#[test]
fn gateway_default_stereo_48k_256frames() {
    let gw = AlsaGateway::new();
    assert_eq!(gw.channels(), 2);
    assert_eq!(gw.sample_rate(), 48_000);
    assert_eq!(gw.num_frames(), 256);
    assert!(gw.listen_addr().ip().is_loopback());
}

#[test]
fn gateway_builder_chain() {
    let gw = AlsaGateway::new()
        .with_channels(1)
        .with_sample_rate(44_100)
        .with_num_frames(128);
    assert_eq!(gw.channels(), 1);
    assert_eq!(gw.sample_rate(), 44_100);
    assert_eq!(gw.num_frames(), 128);
}

#[test]
fn gateway_trait_object_safe() {
    let gw = AlsaGateway::new();
    let _: Box<dyn Gateway> = Box::new(gw);
}

#[test]
fn gateway_run_wires_graph_then_unimplemented() {
    let mut graph = gw_alsa::Graph::new();
    let before = graph.node_count();
    let mut gw = AlsaGateway::new();
    let err = gw.run(&mut graph).expect_err("P2 stub");
    assert!(
        matches!(err, GatewayError::Unimplemented(_)),
        "expected Unimplemented, got {err:?}"
    );
    // register() ran as a side effect: two nodes added.
    assert_eq!(graph.node_count(), before + 2);
}

#[test]
fn gateway_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<AlsaGateway>();
}

#[test]
fn negotiate_result_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Result<HwParams>>();
}

#[test]
fn negotiate_error_is_send_and_display() {
    fn assert_send<T: Send>() {}
    assert_send::<NegotiateError>();
    // Verify Display impl exists (used in logging).
    let err_str = format!("{}", NegotiateError::NoRate);
    assert!(!err_str.is_empty());
}
