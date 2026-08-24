//! Performance regression benchmark for the net-rtp-aes67 codec (heatmap
//! Performance i5). Measures L16 encode/decode and RTP packet serialize/parse
//! latency for a 256-frame stereo payload — the per-packet cost on the RTP
//! data path.
//!
//! Run with: `cargo bench -p net-rtp-aes67`

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use net_rtp_aes67::{decode_l16, encode_l16, RtpHeader, RtpPacket};

const CHANNELS: u16 = 2;
const NUM_FRAMES: usize = 256;

fn sine_samples() -> Vec<f32> {
    (0..NUM_FRAMES * CHANNELS as usize)
        .map(|i| {
            let t = (i as f32) / 48_000.0;
            (t * 440.0 * std::f32::consts::TAU).sin() * 0.5
        })
        .collect()
}

fn bench_l16_encode(c: &mut Criterion) {
    let samples = sine_samples();
    c.bench_function("encode_l16/256f_stereo", |b| {
        b.iter(|| {
            let bytes = encode_l16(black_box(&samples), CHANNELS);
            black_box(bytes);
        })
    });
}

fn bench_l16_decode(c: &mut Criterion) {
    let samples = sine_samples();
    let bytes = encode_l16(&samples, CHANNELS);
    c.bench_function("decode_l16/256f_stereo", |b| {
        b.iter(|| {
            let decoded = decode_l16(black_box(&bytes), CHANNELS);
            black_box(decoded);
        })
    });
}

fn bench_rtp_packet_serialize(c: &mut Criterion) {
    let samples = sine_samples();
    let payload = encode_l16(&samples, CHANNELS);
    let header = RtpHeader::new(96, 1, 0, 0x1234_5678);
    let packet = RtpPacket {
        header,
        payload: payload.clone(),
    };
    c.bench_function("rtp_packet_encode/256f_stereo", |b| {
        b.iter(|| {
            let bytes = black_box(&packet).encode();
            black_box(bytes);
        })
    });
}

fn bench_rtp_packet_parse(c: &mut Criterion) {
    let samples = sine_samples();
    let payload = encode_l16(&samples, CHANNELS);
    let header = RtpHeader::new(96, 1, 0, 0x1234_5678);
    let packet = RtpPacket {
        header,
        payload: payload.clone(),
    };
    let bytes = packet.encode();
    c.bench_function("rtp_packet_parse/256f_stereo", |b| {
        b.iter(|| {
            let parsed = RtpPacket::parse(black_box(&bytes)).unwrap();
            black_box(parsed);
        })
    });
}

criterion_group!(
    benches,
    bench_l16_encode,
    bench_l16_decode,
    bench_rtp_packet_serialize,
    bench_rtp_packet_parse
);
criterion_main!(benches);
