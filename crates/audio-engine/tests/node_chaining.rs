//! Integration tests: graph-based chaining of the second-wave DSP nodes.
//!
//! Unlike `node_pipeline.rs` (which wires nodes by directly invoking
//! `AudioNode::process`), these build real [`Graph`] topologies —
//! `add_node` / `link` / `compile` / `process_cycle` — and verify the
//! audio that reaches each sink via `Graph::read_input`. This covers the
//! nodes that shipped after the initial six (noise source, distortion,
//! tremolo, delay, reverb, aux send, stereo widener, tone generator, …)
//! in the scheduling context they actually run in.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};
use audio_engine::builtins::{Capture, Gain};
use audio_engine::nodes::{
    AuxSendNode, CompressorNode, DelayNode, DistortionMode, DistortionNode, LimiterNode, MeterNode,
    MixerNode, NoiseColor, NoiseSource, ReverbNode, StereoWidenerNode, ToneGenerator, TremoloNode,
    Waveform,
};
use audio_graph_bsd::{Graph, GraphConfig};

const NF: usize = 256;
const SR: u32 = 48_000;
const CH: u16 = 1;

fn peak(frame: &AudioFrame) -> f32 {
    frame
        .samples
        .iter()
        .copied()
        .map(f32::abs)
        .fold(0.0_f32, f32::max)
}

/// Channel-count-parametrised capture sink. The `builtins::Capture` is
/// mono-only, and `Graph::link` rejects ports whose channel counts differ —
/// so stereo topologies need a 2-channel variant of the same no-op tap.
struct CaptureN {
    in_port: [PortDescriptor; 1],
}
impl CaptureN {
    fn new(channels: u16) -> Self {
        Self {
            in_port: [PortDescriptor::input(channels, SampleFormat::F32)],
        }
    }
}
impl AudioNode for CaptureN {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.in_port
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &[]
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        _in_frames: &[AudioFrame],
        _out_frames: &mut [AudioFrame],
    ) {
        // Empty: the scheduler copies upstream output into this node's input
        // scratch; observers read it there via `Graph::read_input`.
    }
}

/// NoiseSource → Distortion(SoftClip) → Limiter → Capture.
///
/// The limiter ceiling (−1 dB ≈ 0.891) must hold for every sample that
/// reaches the sink, and the chain must stay non-silent (white noise at
/// amp 0.5 through tanh drive stays well above the noise floor).
#[test]
fn source_through_effect_chain() {
    let mut graph = Graph::new();
    let noise = graph.add_node(Box::new(NoiseSource::new(NoiseColor::White, 0.5, 42, CH)));
    let dist = graph.add_node(Box::new(DistortionNode::new(
        DistortionMode::SoftClip,
        2.0,
        1.0,
        1.0,
        CH,
    )));
    let lim = graph.add_node(Box::new(LimiterNode::new(-1.0, CH)));
    let cap = graph.add_node(Box::new(Capture::new()));

    graph.link((noise, 0), (dist, 0)).expect("link noise→dist");
    graph.link((dist, 0), (lim, 0)).expect("link dist→lim");
    graph.link((lim, 0), (cap, 0)).expect("link lim→capture");
    graph
        .compile(GraphConfig::new(NF, SR, CH))
        .expect("compile");

    let mut ctx = ProcessContext::new(NF, 0, SR);
    for _ in 0..8 {
        graph.process_cycle(&mut ctx).expect("process_cycle");
    }

    let out = graph.read_input(cap, 0).expect("capture input");
    let threshold = 10.0_f32.powf(-1.0 / 20.0);
    let p = peak(out);
    for &s in &out.samples {
        assert!(
            s.abs() <= threshold + 1e-6,
            "sample {s} passed limiter ceiling {threshold:.4}"
        );
        assert!(s.is_finite(), "non-finite sample {s}");
    }
    assert!(p > 0.2, "chain output is near-silent (peak {p:.4})");
}

/// ToneGenerator → TremoloNode → Capture.
///
/// A 2 Hz / depth 0.8 LFO sweeps the per-cycle peak between ~0.5 (full gain)
/// and ~0.1 (0.2 residual gain). Over one full LFO period the quietest
/// cycle must be well below the loudest — the classic tremolo wobble.
#[test]
fn modulation_chain() {
    let mut graph = Graph::new();
    let tone = graph.add_node(Box::new(ToneGenerator::new(
        Waveform::Sine,
        440.0,
        0.5,
        SR,
        CH,
    )));
    let trem = graph.add_node(Box::new(TremoloNode::new(2.0, 0.8, SR, CH)));
    let cap = graph.add_node(Box::new(Capture::new()));

    graph.link((tone, 0), (trem, 0)).expect("link tone→tremolo");
    graph
        .link((trem, 0), (cap, 0))
        .expect("link tremolo→capture");
    graph
        .compile(GraphConfig::new(NF, SR, CH))
        .expect("compile");

    let mut ctx = ProcessContext::new(NF, 0, SR);
    // One full 2 Hz LFO period = 24 000 samples ≈ 94 cycles of 256 frames.
    let mut cycle_peaks: Vec<f32> = Vec::new();
    for _ in 0..96 {
        graph.process_cycle(&mut ctx).expect("process_cycle");
        cycle_peaks.push(peak(graph.read_input(cap, 0).expect("capture input")));
    }

    let max = cycle_peaks.iter().copied().fold(0.0_f32, f32::max);
    let min = cycle_peaks.iter().copied().fold(f32::INFINITY, f32::min);
    assert!(max > 0.35, "tremolo never opened up (max peak {max:.4})");
    assert!(
        min < 0.5 * max,
        "no tremolo attenuation observed (min {min:.4} vs max {max:.4})"
    );
}

/// ToneGenerator → DelayNode → ReverbNode → Capture.
///
/// The delay (2400 samples) plus the reverb's comb tail (~1200+ samples)
/// must still produce audio after the transient settles: the dry paths of
/// both effects alone guarantee a healthy level.
#[test]
fn time_effect_chain() {
    let mut graph = Graph::new();
    let tone = graph.add_node(Box::new(ToneGenerator::new(
        Waveform::Sine,
        220.0,
        0.4,
        SR,
        CH,
    )));
    let delay = graph.add_node(Box::new(DelayNode::new(4800, 2400, 0.3, 0.5, CH)));
    let rev = graph.add_node(Box::new(ReverbNode::new(0.5, 0.5, 0.3, 0.7, SR, CH)));
    let cap = graph.add_node(Box::new(Capture::new()));

    graph.link((tone, 0), (delay, 0)).expect("link tone→delay");
    graph.link((delay, 0), (rev, 0)).expect("link delay→reverb");
    graph.link((rev, 0), (cap, 0)).expect("link reverb→capture");
    graph
        .compile(GraphConfig::new(NF, SR, CH))
        .expect("compile");

    let mut ctx = ProcessContext::new(NF, 0, SR);
    // 16 cycles = 4096 samples: past the 2400-sample delay tap and well into
    // the reverb comb build-up, but still far cheaper than a real tail.
    for _ in 0..16 {
        graph.process_cycle(&mut ctx).expect("process_cycle");
    }

    let out = graph.read_input(cap, 0).expect("capture input");
    let p = peak(out);
    assert!(p > 0.05, "delay+reverb chain went silent (peak {p:.4})");
    for &s in &out.samples {
        assert!(s.is_finite(), "non-finite sample {s}");
    }
}

/// NoiseSource → AuxSend ─┬─ out 0 (main) → Gain(1.0) → Capture A
///                        └─ out 1 (aux)  → Gain(0.5) → Capture B
///
/// Exercises multi-port linking (`link` with explicit port indices). The main
/// path carries the signal at unity while the aux path carries it at
/// send_level 0.5 × gain 0.5 = 0.25 — so A's peak must dominate B's by
/// roughly 4× (asserted loosely as > 2× to stay ratio-robust).
#[test]
fn aux_send_split_chain() {
    let mut graph = Graph::new();
    let noise = graph.add_node(Box::new(NoiseSource::new(NoiseColor::White, 0.5, 7, CH)));
    let aux = graph.add_node(Box::new(AuxSendNode::new(0.5, CH)));
    let gain_a = graph.add_node(Box::new(Gain::new(1.0)));
    let gain_b = graph.add_node(Box::new(Gain::new(0.5)));
    let cap_a = graph.add_node(Box::new(Capture::new()));
    let cap_b = graph.add_node(Box::new(Capture::new()));

    graph
        .link((noise, 0), (aux, 0))
        .expect("link noise→auxsend");
    // Multi-port link: AuxSend output 0 = main, output 1 = scaled aux.
    graph
        .link((aux, 0), (gain_a, 0))
        .expect("link aux main→gain_a");
    graph
        .link((aux, 1), (gain_b, 0))
        .expect("link aux tap→gain_b");
    graph
        .link((gain_a, 0), (cap_a, 0))
        .expect("link gain_a→capture_a");
    graph
        .link((gain_b, 0), (cap_b, 0))
        .expect("link gain_b→capture_b");
    graph
        .compile(GraphConfig::new(NF, SR, CH))
        .expect("compile");

    let mut ctx = ProcessContext::new(NF, 0, SR);
    for _ in 0..4 {
        graph.process_cycle(&mut ctx).expect("process_cycle");
    }

    let a = peak(graph.read_input(cap_a, 0).expect("capture A input"));
    let b = peak(graph.read_input(cap_b, 0).expect("capture B input"));
    assert!(b > 0.02, "aux branch is near-silent (peak {b:.4})");
    assert!(
        a > 2.0 * b,
        "main peak {a:.4} does not dominate aux peak {b:.4} (expected ~4×)"
    );
    assert!(
        a < 8.0 * b,
        "main/aux ratio {a:.4}/{b:.4} implausibly high (expected ~4×)"
    );
}

/// 2-channel ToneGenerator → StereoWidenerNode(width 0) → stereo capture.
///
/// Width 0 collapses to mono: every frame must leave the widener with
/// L == R == mid. (The tone generator's two channels start at the same
/// phase, so this doubles as a passthrough-consistency check of the
/// mid/side reconstruction at the null width.)
#[test]
fn stereo_widener_in_graph() {
    const CH2: u16 = 2;
    let mut graph = Graph::new();
    let tone = graph.add_node(Box::new(ToneGenerator::new(
        Waveform::Sine,
        440.0,
        0.5,
        SR,
        CH2,
    )));
    let widener = graph.add_node(Box::new(StereoWidenerNode::new(0.0, CH2)));
    let cap = graph.add_node(Box::new(CaptureN::new(CH2)));

    graph
        .link((tone, 0), (widener, 0))
        .expect("link tone→widener");
    graph
        .link((widener, 0), (cap, 0))
        .expect("link widener→capture");
    graph
        .compile(GraphConfig::new(NF, SR, CH2))
        .expect("compile");

    let mut ctx = ProcessContext::new(NF, 0, SR);
    for _ in 0..4 {
        graph.process_cycle(&mut ctx).expect("process_cycle");
    }

    let out = graph.read_input(cap, 0).expect("capture input");
    let n = out.samples.len();
    let per_ch = n / 2;
    let p = peak(out);
    assert!(p > 0.4, "stereo chain output near-silent (peak {p:.4})");
    for i in 0..per_ch {
        let l = out.samples[i];
        let r = out.samples[per_ch + i];
        assert!(
            (l - r).abs() < 1e-6,
            "width 0 must collapse to mono: L[{i}]={l} vs R[{i}]={r}"
        );
    }
}

/// Full mixing-console topology:
///
/// ToneGenerator → AuxSend ─┬─ main → Mixer(in 0) ─ Compressor → Meter → Capture
///                          └─ aux  → Reverb  → Mixer(in 1)
///
/// The meter cannot be read back from the graph (nodes are owned by value),
/// so the chain is verified at the terminal capture: non-silent, finite,
/// and the graph exposes the expected node/link counts.
#[test]
fn full_mixing_console() {
    let mut graph = Graph::new();
    let tone = graph.add_node(Box::new(ToneGenerator::new(
        Waveform::Sine,
        440.0,
        0.5,
        SR,
        CH,
    )));
    let aux = graph.add_node(Box::new(AuxSendNode::new(0.5, CH)));
    let rev = graph.add_node(Box::new(ReverbNode::new(0.5, 0.5, 0.3, 0.7, SR, CH)));
    let mixer = graph.add_node(Box::new(MixerNode::new(2, vec![1.0, 0.8], CH)));
    let comp = graph.add_node(Box::new(CompressorNode::new(
        -12.0, 4.0, 1.0, 100.0, 0.0, SR, CH,
    )));
    let meter = graph.add_node(Box::new(MeterNode::new(CH)));
    let cap = graph.add_node(Box::new(Capture::new()));

    graph.link((tone, 0), (aux, 0)).expect("link tone→auxsend");
    graph
        .link((aux, 0), (mixer, 0))
        .expect("link aux main→mixer in 0");
    graph.link((aux, 1), (rev, 0)).expect("link aux tap→reverb");
    graph
        .link((rev, 0), (mixer, 1))
        .expect("link reverb→mixer in 1");
    graph
        .link((mixer, 0), (comp, 0))
        .expect("link mixer→compressor");
    graph
        .link((comp, 0), (meter, 0))
        .expect("link compressor→meter");
    graph
        .link((meter, 0), (cap, 0))
        .expect("link meter→capture");
    graph
        .compile(GraphConfig::new(NF, SR, CH))
        .expect("compile");

    assert_eq!(graph.node_count(), 7);
    assert_eq!(graph.link_count(), 7);

    let mut ctx = ProcessContext::new(NF, 0, SR);
    // 16 cycles = 4096 samples so the reverb return contributes alongside
    // the dry main path.
    for _ in 0..16 {
        graph.process_cycle(&mut ctx).expect("process_cycle");
    }

    let out = graph.read_input(cap, 0).expect("capture input");
    let p = peak(out);
    assert!(p > 0.05, "console output near-silent (peak {p:.4})");
    for &s in &out.samples {
        assert!(s.is_finite(), "non-finite sample {s}");
    }
}
