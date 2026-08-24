//! Performance regression benchmark for the gw-browser wire codec (heatmap
//! Performance i4). Measures the encode/decode round-trip latency of a typical
//! 256-frame stereo 48 kHz PCM frame, which is the per-cycle cost on the WS
//! hot path.
//!
//! Run with: `cargo bench -p gw-browser`

use audio_core_bsd::AudioFrame;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use gw_browser::{decode_frame, encode_frame, FrameSpec};

const CHANNELS: u16 = 2;
const SAMPLE_RATE: u32 = 48_000;
const NUM_FRAMES: usize = 256;
const SPEC: FrameSpec = FrameSpec::new(CHANNELS, SAMPLE_RATE);

fn sine_frame() -> AudioFrame {
    let samples: Vec<f32> = (0..NUM_FRAMES * CHANNELS as usize)
        .map(|i| {
            let t = (i as f32) / SAMPLE_RATE as f32;
            (t * 440.0 * std::f32::consts::TAU).sin() * 0.5
        })
        .collect();
    AudioFrame::from_planar(CHANNELS, SAMPLE_RATE, samples)
}

fn bench_encode(c: &mut Criterion) {
    let frame = sine_frame();
    c.bench_function("encode_frame/256f_stereo_48k", |b| {
        b.iter(|| {
            let bytes = encode_frame(black_box(&frame)).unwrap();
            black_box(bytes);
        })
    });
}

fn bench_decode(c: &mut Criterion) {
    let frame = sine_frame();
    let bytes = encode_frame(&frame).unwrap();
    c.bench_function("decode_frame/256f_stereo_48k", |b| {
        b.iter(|| {
            let decoded = decode_frame(black_box(&bytes), SPEC).unwrap();
            black_box(decoded);
        })
    });
}

fn bench_round_trip(c: &mut Criterion) {
    let frame = sine_frame();
    c.bench_function("encode+decode_roundtrip/256f_stereo_48k", |b| {
        b.iter(|| {
            let bytes = encode_frame(black_box(&frame)).unwrap();
            let decoded = decode_frame(black_box(&bytes), SPEC).unwrap();
            black_box(decoded);
        })
    });
}

criterion_group!(benches, bench_encode, bench_decode, bench_round_trip);
criterion_main!(benches);
