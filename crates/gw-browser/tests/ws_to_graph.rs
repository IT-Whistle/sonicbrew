//! Integration i5 — encode_frame → decode_frame → graph insertion round-trip.
//!
//! Tests the full pipeline: codec encode → decode → AudioFrame →
//! RingSource → process_cycle → RingSink flush → pop, verifying sample
//! integrity across the codec+graph boundary. No WebSocket server involved.

use audio_core_bsd::AudioFrame;
use audio_dsp_bsd::GainProcessor;
use audio_graph_bsd::{Graph, GraphConfig, RingSink, RingSource};
use gw_browser::{decode_frame, encode_frame, FrameSpec};

const NUM_FRAMES: usize = 8;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_graph_with_handles(
    spec: FrameSpec,
) -> (
    Graph,
    audio_graph_bsd::NodeId,
    audio_graph_bsd::NodeId,
    rtrb::Producer<AudioFrame>,
    rtrb::Consumer<AudioFrame>,
) {
    let mut g = Graph::new();
    let (prod_in, cons_in) = rtrb::RingBuffer::<AudioFrame>::new(64);
    let (prod_out, cons_out) = rtrb::RingBuffer::<AudioFrame>::new(64);

    let src = g.add_node(Box::new(RingSource::new(
        cons_in,
        spec.channels,
        spec.sample_rate,
        NUM_FRAMES,
    )));
    let sink = g.add_sink(Box::new(RingSink::new(
        prod_out,
        spec.channels,
        spec.sample_rate,
        NUM_FRAMES,
    )));
    g.link((src, 0), (sink, 0)).expect("link src→sink");
    g.compile(GraphConfig::new(
        NUM_FRAMES,
        spec.sample_rate,
        spec.channels,
    ))
    .expect("compile");

    (g, src, sink, prod_in, cons_out)
}

fn stereo_samples(offset: f32) -> Vec<f32> {
    // 2 channels × NUM_FRAMES = 16 planar samples
    (0..NUM_FRAMES * 2)
        .map(|i| (i as f32 + offset) * 0.01)
        .collect()
}

fn mono_samples(offset: f32) -> Vec<f32> {
    (0..NUM_FRAMES).map(|i| (i as f32 + offset) * 0.1).collect()
}

fn assert_samples_eq(got: &[f32], want: &[f32]) {
    assert_eq!(got.len(), want.len(), "sample count mismatch");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert!((g - w).abs() < 1e-5, "sample[{i}]: got {g}, want {w}");
    }
}

// ---------------------------------------------------------------------------
// Integration: encode → decode → RingSource → process_cycle → RingSink
// ---------------------------------------------------------------------------

#[test]
fn encode_decode_push_process_flush_pop_round_trip() {
    let spec = FrameSpec::new(2, 48_000);
    let (mut g, _src, _sink, mut prod_in, mut cons_out) = build_graph_with_handles(spec);

    // 1. Create original audio frame with NUM_FRAMES worth of stereo samples.
    let samples = stereo_samples(0.0);
    let original = AudioFrame::from_planar(2, 48_000, samples.clone());
    let wire = encode_frame(&original).expect("encode");

    // 2. Decode wire bytes back to AudioFrame (simulates the WS receive path).
    let decoded = decode_frame(&wire, spec).expect("decode");

    // 3. Push decoded frame into the inbound ring.
    prod_in.push(decoded).expect("push to inbound ring");

    // 4. Process one audio cycle (RT path).
    let mut ctx = audio_core_bsd::ProcessContext::new(NUM_FRAMES, 0, 48_000);
    g.process_cycle(&mut ctx).expect("process_cycle");

    // 5. Flush RingSink stash into the outbound ring.
    g.flush_sinks();

    // 6. Pop from the outbound ring — should match original samples.
    let output = cons_out.pop().expect("pop outbound frame");
    assert_samples_eq(&output.samples, &samples);
}

// ---------------------------------------------------------------------------
// Integration: mono 44100 encode → graph → output
// ---------------------------------------------------------------------------

#[test]
fn mono_44100_encode_to_graph_round_trip() {
    let spec = FrameSpec::new(1, 44_100);
    let (mut g, _src, _sink, mut prod_in, mut cons_out) = build_graph_with_handles(spec);

    let samples = mono_samples(0.0);
    let original = AudioFrame::from_planar(1, 44_100, samples.clone());
    let wire = encode_frame(&original).expect("encode");
    let decoded = decode_frame(&wire, spec).expect("decode");

    prod_in.push(decoded).expect("push");

    let mut ctx = audio_core_bsd::ProcessContext::new(NUM_FRAMES, 0, 44_100);
    g.process_cycle(&mut ctx).expect("process_cycle");

    g.flush_sinks();

    let output = cons_out.pop().expect("pop");
    assert_samples_eq(&output.samples, &samples);
}

// ---------------------------------------------------------------------------
// Integration: bidirectional — inbound + outbound in same graph cycle
// ---------------------------------------------------------------------------

#[test]
fn bidirectional_inbound_and_outbound_same_cycle() {
    let spec = FrameSpec::new(2, 48_000);
    let (mut g, _src, _sink, mut prod_in, mut cons_out) = build_graph_with_handles(spec);

    // Push inbound frame (simulates WS client → graph).
    let inbound_samples = stereo_samples(5.0);
    let inbound = AudioFrame::from_planar(2, 48_000, inbound_samples.clone());
    let wire = encode_frame(&inbound).expect("encode inbound");
    let decoded = decode_frame(&wire, spec).expect("decode inbound");
    prod_in.push(decoded).expect("push inbound");

    // Process cycle — RingSource feeds into RingSink (passthrough).
    let mut ctx = audio_core_bsd::ProcessContext::new(NUM_FRAMES, 0, 48_000);
    g.process_cycle(&mut ctx).expect("process_cycle");

    // Flush outbound stash.
    g.flush_sinks();

    // Pop and verify the outbound frame matches the inbound.
    let output = cons_out.pop().expect("pop outbound");
    assert_samples_eq(&output.samples, &inbound_samples);
}

// ---------------------------------------------------------------------------
// Integration: multiple sequential cycles
// ---------------------------------------------------------------------------

#[test]
fn multiple_cycles_preserve_frame_integrity() {
    let spec = FrameSpec::new(1, 48_000);
    let (mut g, _src, _sink, mut prod_in, mut cons_out) = build_graph_with_handles(spec);

    let num_cycles = 5;
    for cycle in 0..num_cycles {
        let samples: Vec<f32> = (0..NUM_FRAMES)
            .map(|i| (i as f32 + cycle as f32) * 0.1)
            .collect();
        let frame = AudioFrame::from_planar(1, 48_000, samples.clone());
        let wire = encode_frame(&frame).expect("encode");
        let decoded = decode_frame(&wire, spec).expect("decode");
        prod_in.push(decoded).expect("push");

        let mut ctx = audio_core_bsd::ProcessContext::new(NUM_FRAMES, 0, 48_000);
        g.process_cycle(&mut ctx).expect("process_cycle");
        g.flush_sinks();

        let output = cons_out.pop().expect("pop");
        assert_samples_eq(&output.samples, &samples);
    }
}

// ---------------------------------------------------------------------------
// Integration: read_input on sink after process_cycle shows the routed data
// ---------------------------------------------------------------------------

#[test]
fn read_input_on_sink_after_process_cycle() {
    let spec = FrameSpec::new(2, 48_000);
    let (g, _src, sink, mut prod_in, _cons_out) = build_graph_with_handles(spec);

    let samples = stereo_samples(3.0);
    let frame = AudioFrame::from_planar(2, 48_000, samples.clone());
    let wire = encode_frame(&frame).expect("encode");
    let decoded = decode_frame(&wire, spec).expect("decode");
    prod_in.push(decoded).expect("push");

    let mut ctx = audio_core_bsd::ProcessContext::new(NUM_FRAMES, 0, 48_000);
    g.process_cycle(&mut ctx).expect("process_cycle");

    // RingSink has 0 outputs, but we can read its input port.
    let input = g
        .read_input(sink, 0)
        .expect("sink input exists after cycle");
    assert_samples_eq(&input.samples, &samples);
}

// ---------------------------------------------------------------------------
// Integration: silence frame passes through codec + graph unchanged
// ---------------------------------------------------------------------------

#[test]
fn silence_frame_passes_unchanged() {
    let spec = FrameSpec::new(2, 48_000);
    let (mut g, _src, _sink, mut prod_in, mut cons_out) = build_graph_with_handles(spec);

    // All-zero frame = silence.
    let silence = AudioFrame::silence(2, NUM_FRAMES, 48_000);
    let wire = encode_frame(&silence).expect("encode silence");
    let decoded = decode_frame(&wire, spec).expect("decode silence");

    prod_in.push(decoded).expect("push silence");

    let mut ctx = audio_core_bsd::ProcessContext::new(NUM_FRAMES, 0, 48_000);
    g.process_cycle(&mut ctx).expect("process_cycle");

    g.flush_sinks();

    let output = cons_out.pop().expect("pop silence");
    assert!(
        output.samples.iter().all(|&s| s == 0.0),
        "silence frame must remain all zeros after codec+graph"
    );
}

// ---------------------------------------------------------------------------
// Integration: GainProcessor(0.5) between RingSource and RingSink halves signal
// ---------------------------------------------------------------------------

#[test]
fn graph_with_gain_node_modifies_signal() {
    let spec = FrameSpec::new(2, 48_000);
    let mut g = Graph::new();
    let (prod_in, cons_in) = rtrb::RingBuffer::<AudioFrame>::new(64);
    let (prod_out, cons_out) = rtrb::RingBuffer::<AudioFrame>::new(64);

    // RingSource → GainProcessor(0.5) → RingSink
    let src = g.add_node(Box::new(RingSource::new(
        cons_in,
        spec.channels,
        spec.sample_rate,
        NUM_FRAMES,
    )));
    let mut gain_proc = GainProcessor::new(spec.channels);
    gain_proc.set_gain(0.5);
    let gain = g.add_node(Box::new(gain_proc));
    let sink = g.add_sink(Box::new(RingSink::new(
        prod_out,
        spec.channels,
        spec.sample_rate,
        NUM_FRAMES,
    )));
    g.link((src, 0), (gain, 0)).expect("link src→gain");
    g.link((gain, 0), (sink, 0)).expect("link gain→sink");
    g.compile(GraphConfig::new(
        NUM_FRAMES,
        spec.sample_rate,
        spec.channels,
    ))
    .expect("compile");

    let mut prod_in = prod_in;
    let mut cons_out = cons_out;

    // Push a frame with known non-zero samples.
    let samples = stereo_samples(0.0);
    let frame = AudioFrame::from_planar(2, 48_000, samples.clone());
    let wire = encode_frame(&frame).expect("encode");
    let decoded = decode_frame(&wire, spec).expect("decode");
    prod_in.push(decoded).expect("push frame 1");

    // Cycle 1: settles gain from 1.0→0.5 (click prevention ramp).
    let mut ctx = audio_core_bsd::ProcessContext::new(NUM_FRAMES, 0, 48_000);
    g.process_cycle(&mut ctx).expect("process_cycle");
    g.flush_sinks();
    let _ramped = cons_out.pop().expect("pop ramp frame");

    // Push a second frame for cycle 2.
    let wire2 = encode_frame(&frame).expect("encode frame 2");
    let decoded2 = decode_frame(&wire2, spec).expect("decode frame 2");
    prod_in.push(decoded2).expect("push frame 2");

    // Cycle 2: gain settled at 0.5, output = input × 0.5.
    let mut ctx2 = audio_core_bsd::ProcessContext::new(NUM_FRAMES, 0, 48_000);
    g.process_cycle(&mut ctx2).expect("process_cycle");
    g.flush_sinks();

    // Pop the settled output (frame 2).
    let output = cons_out.pop().expect("pop settled output");

    // After settling, output ≈ input × 0.5.
    for (i, (&got, expected)) in output
        .samples
        .iter()
        .zip(samples.iter().map(|s| s * 0.5))
        .enumerate()
    {
        assert!(
            (got - expected).abs() < 0.01,
            "sample[{i}]: got {got}, expected {expected}"
        );
    }
}
