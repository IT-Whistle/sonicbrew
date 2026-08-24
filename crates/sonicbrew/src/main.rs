//! `sonicbrew` server binary — MVP integration entry point.
//!
//! Assembles the three MVP modules into a runnable server:
//!
//! - **session-store** (M07) — `RaftEngine` persisting the topology snapshot
//!   to a redb WAL.
//! - **gw-browser** (M12) — `BrowserGateway` WebSocket bridge wired into the
//!   graph via `audio-graph-bsd`'s built-in `RingSource` / `RingSink`.
//! - **control-api** (M13) — `RestApi` (axum) over the session store.
//!
//! and drives the `audio-graph-bsd` real-time graph on a dedicated OS thread.
//!
//! # Dev-host constraints
//!
//! The dev host is Linux x86_64 with no audio hardware and no `libasound` /
//! `libopus`. The binary therefore pulls in **no** `audio-io-bsd` (which would
//! link cpal/ALSA) and uses a ring↔graph pipeline only — no speaker path
//! (BUILD-PLAN §6, p11 decision #5).
//!
//! # Threading model
//!
//! - The `Graph` is owned by a **dedicated `std::thread`** that ticks
//!   [`Graph::process_cycle`] once per block (~5.3 ms for 256/48 kHz). The
//!   graph never crosses an `await` and is never shared: [`Graph::process_cycle`]
//!   is `&self`, and the RT thread is the sole mutator during processing, so
//!   owning it directly is both sound and simplest.
//! - The async servers (browser gateway + control API) run on the `tokio`
//!   runtime and hold only the `rtrb` ring handles + the `Arc<dyn
//!   SessionStore>` — never the live `Graph`.
//!
//! # Status
//!
//! - **Outbound flush-gap RESOLVED (audio-graph-bsd 0.4.0).** Gateways register
//!   their `RingSink` via `Graph::add_sink` (a `Flushable` `SinkNode`); the RT
//!   loop calls `Graph::flush_sinks` between cycles to ship stashed frames
//!   across the outbound `rtrb` ring. Both directions now flow: browser→graph
//!   (via `RingSource` in `process_cycle`) and graph→browser (via `flush_sinks`).
//! - **Hot-reload demonstrated (`--hot-reload-test`).** The arc-swap
//!   `RtHandle::install` live-graph-swap is proven by the self-test. The server
//!   RT loop owns the `Graph` by value (to drive `flush_sinks`); reconciling
//!   `flush_sinks` (`&mut`) with the shared `RtHandle` (`&self`) for a single
//!   unified server loop, plus topology-event-driven rebuild, is the remaining
//!   follow-up (see `docs/.../audio-graph-bsd-engine-changes.md` §5).
//! - **Kind/params persistence via preset sidecar (`--server-engine`).** The
//!   redb store persists topology but loses the in-memory kind/params
//!   registries on restart; `run_server_engine` now autosaves the full graph
//!   state to a preset JSON sidecar every 2 s (write-on-change, atomic
//!   tmp+rename) and restores it on boot — so REST-created node kinds and
//!   params survive restarts.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "diagnose")]
mod diagnose;

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};
use audio_graph_bsd::{Graph, GraphConfig, NodeId, RtHandle};
use control_api::RestApi;
use gw_browser::BrowserGateway;
use monitor::{serve_metrics, MetricsRecorder, MetricsSink};
use session_store::{RaftEngine, SessionStore};

// Optional Bluetooth A2DP input backend (Part B). The module compiles only
// with `--features bluetooth`; the default build stays free of audio-io-bsd.
#[cfg(feature = "bluetooth")]
mod bt_input;

/// Audio block shape used by the MVP binary (stereo, 48 kHz, 256-sample
/// blocks). Matches the gateway defaults and the compile config.
const CHANNELS: u16 = 2;
const SAMPLE_RATE: u32 = 48_000;
const NUM_FRAMES: usize = 256;

/// Per-cycle sleep for the RT tick loop: `NUM_FRAMES / SAMPLE_RATE`.
const FRAME_DURATION: Duration =
    Duration::from_nanos((NUM_FRAMES as u64 * 1_000_000_000) / SAMPLE_RATE as u64);

/// Parsed command-line arguments.
struct Args {
    /// Run the deterministic self-test and exit (no servers).
    self_test: bool,
    /// Run the deterministic hot-reload self-test and exit (no servers).
    hot_reload_test: bool,
    /// Run the topology-driven live-rebuild self-test and exit (no servers).
    live_rebuild_test: bool,
    /// Run the audio-engine end-to-end live-rebuild self-test and exit.
    engine_live_rebuild_test: bool,
    /// Run the gateway-bridge end-to-end live-reload self-test and exit.
    gateway_live_reload_test: bool,
    /// Launch the interactive diagnostic TUI (signal waveform + metrics;
    /// requires the `diagnose` feature).
    diagnose: bool,
    /// Boot the audio-engine-based server (live-reload capable: control-api
    /// topology changes → from_snapshot rebuild → GraphEngine swap, gateway
    /// survives via GatewayBridge).
    server_engine: bool,
    /// Print usage and exit.
    help: bool,
    /// Browser gateway (WebSocket) listen address.
    ws_addr: SocketAddr,
    /// Control API (REST) listen address.
    api_addr: SocketAddr,
    /// Prometheus `/metrics` scrape endpoint address.
    metrics_addr: SocketAddr,
    /// Load an audio file (FLAC/WAV/PCM) as a looping FileSource node at
    /// startup (server-engine mode; decoded once on a worker thread, then
    /// registered for the rebuild factory by node id).
    load_file: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            self_test: false,
            hot_reload_test: false,
            live_rebuild_test: false,
            engine_live_rebuild_test: false,
            gateway_live_reload_test: false,
            diagnose: false,
            server_engine: false,
            help: false,
            ws_addr: DEFAULT_WS_ADDR
                .parse()
                .expect("hard-coded default WS address always parses"),
            api_addr: DEFAULT_API_ADDR
                .parse()
                .expect("hard-coded default API address always parses"),
            metrics_addr: DEFAULT_METRICS_ADDR
                .parse()
                .expect("hard-coded default metrics address always parses"),
            load_file: None,
        }
    }
}

/// Default browser gateway listen address.
const DEFAULT_WS_ADDR: &str = "127.0.0.1:9001";
/// Default control API listen address.
const DEFAULT_API_ADDR: &str = "127.0.0.1:9002";
/// Default Prometheus `/metrics` scrape endpoint address.
const DEFAULT_METRICS_ADDR: &str = "127.0.0.1:9003";

/// Dev-only session-store path (production uses a configured path).
const DEV_STORE_PATH: &str = "sonicbrew-dev.redb";

/// Preset sidecar path (next to the dev redb store) — kind/params persistence.
const DEV_PRESET_PATH: &str = "sonicbrew-dev.preset.json";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let args = parse_args().map_err(|e| format!("argument error: {e}"))?;
    if args.help {
        print_usage();
        return Ok(());
    }
    if args.hot_reload_test {
        return run_hot_reload_test();
    }
    if args.live_rebuild_test {
        return run_live_rebuild_test();
    }
    if args.engine_live_rebuild_test {
        return run_engine_live_rebuild_test();
    }
    if args.gateway_live_reload_test {
        return run_gateway_live_reload_test();
    }
    #[cfg(feature = "diagnose")]
    if args.diagnose {
        diagnose::run()?;
        return Ok(());
    }
    if args.server_engine {
        return run_server_engine(args).await;
    }

    // NOTE: the session store is opened ONLY in server mode (below). The
    // `--self-test` path exercises the graph pipeline alone and must not touch
    // the dev DB — otherwise it cannot run while a server holds the redb lock.

    // --- Build the graph and wire the browser gateway ----------------------
    // `register` adds a RingSource (inbound) and a RingSink (outbound) but
    // does NOT connect them; the explicit `link` is required for audio to
    // flow src→sink during `process_cycle`.
    let mut graph = Graph::new();
    let gw = BrowserGateway::new()
        .with_listen_addr(args.ws_addr)
        .with_channels(CHANNELS)
        .with_sample_rate(SAMPLE_RATE)
        .with_num_frames(NUM_FRAMES);
    let (src, sink, inbound, outbound) = gw.register(&mut graph)?;
    graph.link((src, 0), (sink, 0))?;
    graph.compile(GraphConfig::new(NUM_FRAMES, SAMPLE_RATE, CHANNELS))?;

    if args.self_test {
        let recorder = Arc::new(MetricsRecorder::new());
        return run_self_test(graph, src, sink, inbound, outbound, &recorder);
    }

    // --- Default mode: RT thread owns the graph; tokio runs the servers ----
    // Session store is opened here (server mode only) so `--self-test` stays
    // independent of the dev DB lock.
    let store_path = std::env::temp_dir().join(DEV_STORE_PATH);
    let engine = RaftEngine::open(&store_path)?;
    let store: Arc<dyn SessionStore> = Arc::new(engine);

    let ws_addr = gw.listen_addr();
    let rt_recorder = Arc::new(MetricsRecorder::new());
    let rt_recorder_handle = Arc::clone(&rt_recorder);
    // The RT thread owns the `Graph` by value so it can call `flush_sinks`
    // (audio-graph-bsd 0.4.0) between cycles to ship outbound audio.
    let rt_thread = std::thread::Builder::new()
        .name("sonicbrew-rt".into())
        .spawn(move || run_rt_loop(graph, rt_recorder_handle))?;

    let gw_task = tokio::spawn(async move {
        if let Err(e) = gw.serve(inbound, outbound).await {
            tracing::error!(error = %e, "browser gateway exited");
        }
    });
    let api_addr = args.api_addr;
    let api_store = Arc::clone(&store);
    let api_task = tokio::spawn(async move {
        if let Err(e) = RestApi::new(api_store).serve(api_addr).await {
            tracing::error!(error = %e, "control API exited");
        }
    });

    tracing::info!(
        ws_addr = %ws_addr,
        api_addr = %args.api_addr,
        "sonicbrew MVP running (Ctrl-C to shut down)"
    );

    // Periodic one-line metrics summary (~1 s cadence). Full Prometheus text
    // is emitted at debug; a short latency line at info. Lightweight: reads
    // wall-time, not every RT cycle.
    let metrics_recorder = Arc::clone(&rt_recorder);
    let metrics_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.tick().await; // skip immediate first tick
        loop {
            tick.tick().await;
            let exported = metrics_recorder.export_prometheus();
            tracing::debug!(target: "sonicbrew::metrics", "{}", exported.trim_end());
            if let Some(summary) = parse_metrics_summary(&exported) {
                tracing::info!(target: "sonicbrew::metrics", "{summary}");
            }
        }
    });

    // Prometheus `/metrics` scrape endpoint: raw HTTP over a dedicated port,
    // sharing the same RT `MetricsRecorder`. Low scrape cadence (~15s) keeps
    // the brief Mutex<VecDeque> snapshot cost negligible.
    let metrics_http_rec = Arc::clone(&rt_recorder);
    let metrics_http_addr = args.metrics_addr;
    let metrics_endpoint_task = tokio::spawn(async move {
        if let Err(e) = serve_metrics(metrics_http_addr, metrics_http_rec).await {
            tracing::error!(error = %e, "metrics endpoint exited");
        }
    });
    tracing::info!(metrics_addr = %args.metrics_addr, "metrics endpoint enabled");

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    gw_task.abort();
    api_task.abort();
    metrics_task.abort();
    metrics_endpoint_task.abort();
    // The RT std-thread ticks forever; it is detached and reaped on process
    // exit. (A shared shutdown flag could join it cleanly; out of MVP scope.)
    drop(rt_thread);
    Ok(())
}

/// Deterministic integration check: push a sine frame through the
/// inbound ring, run several process cycles, and verify non-silent audio
/// reaches the sink's input port.
///
/// This proves the `RingSource` → link → `RingSink` pipeline end-to-end
/// WITHOUT a browser or speaker.
///
/// # Why `read_input` (not `read_output`) and the inbound ring (not `feed`)
///
/// `RingSink` declares zero output ports, so its output scratch is empty —
/// the correct tap for a sink's consumed audio is [`Graph::read_input`].
/// Likewise, `RingSource::process` overwrites its single output by popping
/// the inbound ring on every cycle, so seeding via [`Graph::feed`] would be
/// clobbered; the honest way to drive a `RingSource` is to push frames to
/// the `rtrb` producer returned by [`BrowserGateway::register`].
fn run_self_test(
    mut graph: Graph,
    _src: NodeId,
    sink: NodeId,
    mut inbound: gw_browser::InboundHandle,
    mut outbound: gw_browser::OutboundHandle,
    recorder: &MetricsRecorder,
) -> Result<(), Box<dyn std::error::Error>> {
    // Build a 440 Hz sine frame (2 ch × 256 frames = 512 samples, planar).
    let sine_samples: Vec<f32> = (0..NUM_FRAMES)
        .flat_map(|i| {
            let phase = 2.0 * std::f32::consts::PI * (440.0 * i as f32 / SAMPLE_RATE as f32);
            let s = phase.sin() * 0.5;
            [s, s]
        })
        .collect();
    let sine = AudioFrame::from_planar(CHANNELS, SAMPLE_RATE, sine_samples);
    inbound
        .push(sine)
        .map_err(|e| format!("push inbound sine frame: {e:?}"))?;

    // Run a handful of cycles, measuring each one's latency and feeding it to
    // the M14 monitor (p11 §7c resolved — MetricsRecorder owns measurement).
    let mut ctx = ProcessContext::new(NUM_FRAMES, 0, SAMPLE_RATE);
    const SELF_TEST_CYCLES: usize = 10;
    for cycle in 0..SELF_TEST_CYCLES {
        ctx.sample_position = (cycle as u64) * NUM_FRAMES as u64;
        let t0 = Instant::now();
        graph.process_cycle(&mut ctx)?;
        let us = t0.elapsed().as_micros() as u64;
        recorder.record_cycle(us);
    }

    // RingSink has 0 outputs → tap the consumed audio via read_input.
    let received = graph.read_input(sink, 0).ok_or(
        "self-test FAILED: sink input slot is absent after process_cycle \
         (graph wiring / compile error)",
    )?;
    let peak = received
        .samples
        .iter()
        .copied()
        .fold(0.0_f32, f32::max)
        .abs();
    if peak < 1e-6 {
        return Err(
            "self-test FAILED: sink received silence — audio did not flow \
             src→sink through the graph"
                .into(),
        );
    }

    // M14 monitor acceptance check: the recorder must have captured the
    // SELF_TEST_CYCLES real cycles. Assert a non-empty export AND a max
    // latency line with a value >= 1 (a real cycle was recorded).
    let exported = recorder.export_prometheus();
    if exported.is_empty() {
        return Err("self-test FAILED: monitor export_prometheus is empty".into());
    }
    let max_line = exported
        .lines()
        .find(|l| l.contains("process_latency_us_max"))
        .ok_or_else(|| {
            "self-test FAILED: export missing `process_latency_us_max` line".to_string()
        })?;
    let max_val: u64 = max_line
        .split_whitespace()
        .next_back()
        .ok_or("self-test FAILED: process_latency_us_max line has no value")?
        .parse()
        .map_err(|_| "self-test FAILED: process_latency_us_max value not a u64")?;
    if max_val < 1 {
        return Err(format!(
            "self-test FAILED: process_latency_us_max={max_val} (expected >= 1; \
             no real cycle was recorded)"
        )
        .into());
    }

    // BIDIRECTIONAL check (audio-graph-bsd 0.4.0 `Flushable`/`flush_sinks`):
    // ship the `RingSink`'s stashed frame across the outbound ring, then confirm
    // the outbound consumer actually receives non-silent audio (graph → client).
    let (flushed, ferr) = graph.flush_sinks();
    if flushed == 0 {
        return Err(
            "self-test FAILED: flush_sinks flushed 0 sinks (RingSink not registered via add_sink?)"
                .into(),
        );
    }
    if let Some(e) = ferr {
        return Err(format!("self-test FAILED: flush_sinks error: {e}").into());
    }
    let out_frame = outbound.pop().map_err(|e| {
        format!("self-test FAILED: outbound pop after flush_sinks failed: {e:?} (graph→client audio did not ship)")
    })?;
    let out_peak = out_frame
        .samples
        .iter()
        .copied()
        .fold(0.0_f32, f32::max)
        .abs();
    if out_peak < 1e-6 {
        return Err(
            format!("self-test FAILED: outbound frame is silent (peak={out_peak:.6})").into(),
        );
    }

    println!(
        "self-test OK — {SELF_TEST_CYCLES} cycles, sink input peak={peak:.4}, \
         outbound peak={out_peak:.4} after flush_sinks ({flushed} sink), \
         latency_max={max_val}µs (bidirectional: RingSource→graph→flush_sinks→RingSink; \
         M14 monitor wired)"
    );
    Ok(())
}

// ===== Hot-reload self-test nodes (GainNode is not public in audio-graph-bsd,
// so the binary defines minimal RT-safe internal nodes for this check.) =====

/// Source emitting a pre-allocated mono sine each cycle (0 in, 1 out). The
/// sine buffer is allocated at construction; `process` only copies (RT-safe).
struct SineSourceNode {
    out_port: [PortDescriptor; 1],
    sine: AudioFrame,
}
impl SineSourceNode {
    fn new(freq: f32, amp: f32) -> Self {
        let samples: Vec<f32> = (0..NUM_FRAMES)
            .map(|i| {
                (2.0 * std::f32::consts::PI * freq * i as f32 / SAMPLE_RATE as f32).sin() * amp
            })
            .collect();
        Self {
            out_port: [PortDescriptor::output(1, SampleFormat::F32)],
            sine: AudioFrame::from_planar(1, SAMPLE_RATE, samples),
        }
    }
}
impl AudioNode for SineSourceNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &[]
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_port
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        _in_frames: &[AudioFrame],
        out_frames: &mut [AudioFrame],
    ) {
        if let Some(out) = out_frames.get_mut(0) {
            let n = self.sine.samples.len().min(out.samples.len());
            out.samples[..n].copy_from_slice(&self.sine.samples[..n]);
            out.channels = self.sine.channels;
            out.sample_rate = self.sine.sample_rate;
        }
    }
}

/// 1-in / 1-out linear gain (RT-safe: bounded loop, no alloc).
struct GainNode {
    gain: f32,
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
}
impl GainNode {
    fn new(gain: f32) -> Self {
        Self {
            gain,
            in_port: [PortDescriptor::input(1, SampleFormat::F32)],
            out_port: [PortDescriptor::output(1, SampleFormat::F32)],
        }
    }
}
impl AudioNode for GainNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.in_port
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_port
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        in_frames: &[AudioFrame],
        out_frames: &mut [AudioFrame],
    ) {
        let (Some(inp), Some(out)) = (in_frames.first(), out_frames.get_mut(0)) else {
            return;
        };
        let n = inp.samples.len().min(out.samples.len());
        for k in 0..n {
            out.samples[k] = inp.samples[k] * self.gain;
        }
    }
}

/// 1-in / 0-out sink; the test reads its input scratch via `Graph::read_input`.
struct CaptureSinkNode {
    in_port: [PortDescriptor; 1],
}
impl CaptureSinkNode {
    fn new() -> Self {
        Self {
            in_port: [PortDescriptor::input(1, SampleFormat::F32)],
        }
    }
}
impl Default for CaptureSinkNode {
    fn default() -> Self {
        Self::new()
    }
}
impl AudioNode for CaptureSinkNode {
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
    }
}

/// Deterministic hot-reload self-test (no audio hardware, no servers).
///
/// Builds `SineSource → Gain(1.0) → CaptureSink`, publishes it via
/// [`RtHandle`], runs cycles, and measures the sink-input peak. Then it
/// rebuilds the graph with `Gain(0.5)` and calls [`RtHandle::install`] — the
/// atomic arc-swap — and re-measures. A successful run proves a **live graph
/// swap changed RT behavior without rebuilding the runtime**: the second peak
/// is ~half the first.
///
/// (Production hot-reload rebuilds via `Graph::from_snapshot(topology,
/// factory)` driven by `TopologyEvent`s; this self-test builds the two graphs
/// directly for node-id determinism. See
/// `docs/.../audio-graph-bsd-engine-changes.md` §5.)
fn run_hot_reload_test() -> Result<(), Box<dyn std::error::Error>> {
    /// Build & compile `SineSource → Gain(gain) → CaptureSink` (mono).
    fn build_graph(gain: f32) -> Result<(Graph, NodeId), Box<dyn std::error::Error>> {
        let mut g = Graph::new();
        let src = g.add_node(Box::new(SineSourceNode::new(440.0, 0.5)));
        let gn = g.add_node(Box::new(GainNode::new(gain)));
        let sink = g.add_node(Box::new(CaptureSinkNode::new()));
        g.link((src, 0), (gn, 0))?;
        g.link((gn, 0), (sink, 0))?;
        g.compile(GraphConfig::new(NUM_FRAMES, SAMPLE_RATE, 1))?;
        Ok((g, sink))
    }

    /// Run a few cycles through `handle` and return the peak sample reaching
    /// `sink`'s input scratch (read wait-free via the `RtHandle` guard).
    fn peak_at_sink(handle: &RtHandle, sink: NodeId) -> f32 {
        let mut ctx = ProcessContext::new(NUM_FRAMES, 0, SAMPLE_RATE);
        let mut peak = 0.0_f32;
        for cycle in 0..4u64 {
            ctx.sample_position = cycle * NUM_FRAMES as u64;
            let _ = handle.process_cycle(&mut ctx);
        }
        let g = handle.graph();
        if let Some(frame) = g.read_input(sink, 0) {
            for &s in &frame.samples {
                peak = peak.max(s.abs());
            }
        }
        peak
    }

    // Graph A: gain 1.0 — published into the RtHandle.
    let (graph_a, sink_a) = build_graph(1.0)?;
    let handle = RtHandle::new(graph_a);
    let peak_a = peak_at_sink(&handle, sink_a);
    if peak_a <= 1e-3 {
        return Err(format!("graph A (gain 1.0) produced silence (peak={peak_a:.6})").into());
    }

    // Hot-reload: build graph B (gain 0.5) and atomically install (arc-swap).
    let (graph_b, sink_b) = build_graph(0.5)?;
    handle.install(graph_b);
    let peak_b = peak_at_sink(&handle, sink_b);

    let ratio = peak_b / peak_a;
    if !(0.45..=0.55).contains(&ratio) {
        return Err(format!(
            "hot-reload ratio {ratio:.3} not ~0.5 (peak_a={peak_a:.4}, peak_b={peak_b:.4})"
        )
        .into());
    }

    println!(
        "hot-reload OK — gain 1.0→0.5 live swap: peak {peak_a:.4}→{peak_b:.4} (ratio {ratio:.3})"
    );
    tracing::info!(
        "hot-reload OK: gain 1.0->0.5 live swap, peak {peak_a:.4} -> {peak_b:.4} (ratio {ratio:.3})"
    );
    Ok(())
}

/// Deterministic topology-driven live-rebuild self-test (no hardware/servers).
///
/// Proves the **production** hot-reload path (vs `--hot-reload-test` which
/// exercises the `RtHandle` primitive): rebuild a graph from a
/// `TopologySnapshot` via `Graph::from_snapshot(snapshot, factory)`, where the
/// factory maps each node id to a concrete node via a sonicbrew-side registry
/// (`NodeSnapshot` carries only ports, not a type tag, so the engine keeps this
/// side table). Run cycles (process + flush), then mutate the registry's gain
/// factor (as `control-api` would mutate topology state), rebuild a fresh graph
/// from the SAME snapshot, and swap it in. Assert the live audio peak halves.
///
/// This is flush-compatible: the engine owns the `Graph` by value (so
/// `flush_sinks` works between cycles) — no `RtHandle` `&self`/`&mut` tension.
fn run_live_rebuild_test() -> Result<(), Box<dyn std::error::Error>> {
    use audio_graph_bsd::{
        Mutation, NodeSnapshot, PortDir, PortMeta, SampleFmt, SnapshotEdge, TopologySnapshot,
    };
    use std::collections::HashMap;

    /// sonicbrew-side node-kind registry: `NodeId → how the factory rebuilds it`.
    #[derive(Clone)]
    enum Kind {
        Sine { freq: f32, amp: f32 },
        Gain { factor: f32 },
        Capture,
    }

    // Fixed topology: SineSource(0) → Gain(1) → CaptureSink(2).
    let mono_out = vec![PortMeta {
        direction: PortDir::Output,
        channels: 1,
        sample_format: SampleFmt::F32,
    }];
    let mono_in = vec![PortMeta {
        direction: PortDir::Input,
        channels: 1,
        sample_format: SampleFmt::F32,
    }];
    let mut topo = TopologySnapshot::new();
    topo.apply(&Mutation::AddNode(NodeSnapshot {
        id: 0,
        inputs: vec![],
        outputs: mono_out.clone(),
    }));
    topo.apply(&Mutation::AddNode(NodeSnapshot {
        id: 1,
        inputs: mono_in.clone(),
        outputs: mono_out.clone(),
    }));
    topo.apply(&Mutation::AddNode(NodeSnapshot {
        id: 2,
        inputs: mono_in,
        outputs: vec![],
    }));
    topo.apply(&Mutation::AddLink(SnapshotEdge {
        from: (0, 0),
        to: (1, 0),
    }));
    topo.apply(&Mutation::AddLink(SnapshotEdge {
        from: (1, 0),
        to: (2, 0),
    }));
    // `from_snapshot` adds nodes in snapshot order (0,1,2) → the sink is NodeId 2.
    const SINK: NodeId = 2;

    let mut registry: HashMap<NodeId, Kind> = HashMap::new();
    registry.insert(
        0,
        Kind::Sine {
            freq: 440.0,
            amp: 0.5,
        },
    );
    registry.insert(1, Kind::Gain { factor: 1.0 });
    registry.insert(2, Kind::Capture);

    /// Build & compile a graph from the snapshot + a clone of the registry.
    fn build(
        topo: &TopologySnapshot,
        registry: &HashMap<NodeId, Kind>,
    ) -> Result<Graph, Box<dyn std::error::Error>> {
        let reg = registry.clone();
        let mut factory = move |id: NodeId| -> Option<Box<dyn AudioNode>> {
            match reg.get(&id)? {
                Kind::Sine { freq, amp } => Some(Box::new(SineSourceNode::new(*freq, *amp))),
                Kind::Gain { factor } => Some(Box::new(GainNode::new(*factor))),
                Kind::Capture => Some(Box::new(CaptureSinkNode::new())),
            }
        };
        let mut g = Graph::from_snapshot(topo, &mut factory)?;
        g.compile(GraphConfig::new(NUM_FRAMES, SAMPLE_RATE, 1))?;
        Ok(g)
    }

    // Run a few cycles through `g` and return the peak sample reaching SINK.
    let peak = |g: &Graph| -> f32 {
        let mut ctx = ProcessContext::new(NUM_FRAMES, 0, SAMPLE_RATE);
        for cycle in 0..4u64 {
            ctx.sample_position = cycle * NUM_FRAMES as u64;
            let _ = g.process_cycle(&mut ctx);
        }
        g.read_input(SINK, 0).map_or(0.0, |f| {
            f.samples.iter().copied().map(f32::abs).fold(0.0, f32::max)
        })
    };

    // Graph A: gain 1.0, built from the snapshot.
    let graph_a = build(&topo, &registry)?;
    let peak_a = peak(&graph_a);
    if peak_a <= 1e-3 {
        return Err(format!(
            "live-rebuild: graph A (gain 1.0) produced silence (peak={peak_a:.6})"
        )
        .into());
    }

    // Live rebuild: mutate the registry's gain factor (as control-api would),
    // rebuild a fresh graph from the SAME snapshot, and swap it in.
    registry.insert(1, Kind::Gain { factor: 0.5 });
    let graph_b = build(&topo, &registry)?;
    let peak_b = peak(&graph_b);

    let ratio = peak_b / peak_a;
    if !(0.45..=0.55).contains(&ratio) {
        return Err(format!(
            "live-rebuild ratio {ratio:.3} not ~0.5 (peak_a={peak_a:.4}, peak_b={peak_b:.4})"
        )
        .into());
    }

    println!(
        "live-rebuild OK — from_snapshot rebuild: gain 1.0→0.5, peak {peak_a:.4}→{peak_b:.4} (ratio {ratio:.3})"
    );
    tracing::info!(
        "live-rebuild OK: from_snapshot rebuild, peak {peak_a:.4} -> {peak_b:.4} (ratio {ratio:.3})"
     );
    Ok(())
}

/// End-to-end audio-engine live-rebuild self-test (no hardware/servers).
///
/// Exercises the **production** live-reload chain through the `audio-engine`
/// crate: a real `RaftEngine` session-store → `spawn_rebuild_task` (subscribes
/// to `TopologyEvent`) → `Graph::from_snapshot` rebuild → a shared rebuild slot
/// → `GraphEngine` swaps it in between cycles. A topology-param change (gain
/// 1.0→0.5, applied as control-api would mutate a node) drives a rebuild whose
/// swapped-in graph halves the live audio peak — proving the full
/// control-api ↔ live-graph path in the binary.
fn run_engine_live_rebuild_test() -> Result<(), Box<dyn std::error::Error>> {
    use audio_engine::{
        builtins, rebuild_slot, spawn_rebuild_task, BuiltNode, GraphEngine, NodeFactory,
    };
    use audio_graph_bsd::{NodeSnapshot, PortDir, PortMeta, SampleFmt, SnapshotEdge};
    use session_store::{Mutation, RaftEngine, SessionStore};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    const NF: usize = 64;
    const SR: u32 = 48_000;
    const SINK: audio_engine::NodeId = 2;

    let mono = |direction: PortDir| {
        vec![PortMeta {
            direction,
            channels: 1,
            sample_format: SampleFmt::F32,
        }]
    };

    /// Factory: id 0=SineSource, 1=Gain(factor from a shared atomic), 2=Capture.
    /// The gain is mutable so a "param change" can drive a rebuild.
    struct EngineFactory {
        gain_milli: Arc<AtomicU32>, // gain × 1000 (1000 = 1.0)
    }
    impl NodeFactory for EngineFactory {
        fn build(&self, id: audio_engine::NodeId) -> Option<BuiltNode> {
            match id {
                0 => Some(BuiltNode::Plain(Box::new(builtins::SineSource::new(
                    440.0, 0.5, NF, SR,
                )))),
                1 => Some(BuiltNode::Plain(Box::new(builtins::Gain::new(
                    self.gain_milli.load(Ordering::SeqCst) as f32 / 1000.0,
                )))),
                2 => Some(BuiltNode::Plain(Box::new(builtins::Capture::new()))),
                _ => None,
            }
        }
    }

    let store: Arc<dyn SessionStore> = Arc::new(RaftEngine::default());
    let gain_milli = Arc::new(AtomicU32::new(1000)); // 1.0
    let factory = Arc::new(EngineFactory {
        gain_milli: gain_milli.clone(),
    });
    let slot = rebuild_slot();
    let config = audio_engine::GraphConfig::new(NF, SR, 1);
    let _rebuild = spawn_rebuild_task(store.clone(), config, factory, slot.clone());

    // Phase A topology: SineSource(0) → Gain(1.0)(1) → Capture(2).
    store.apply_mutation(Mutation::AddNode(NodeSnapshot {
        id: 0,
        inputs: vec![],
        outputs: mono(PortDir::Output),
    }))?;
    store.apply_mutation(Mutation::AddNode(NodeSnapshot {
        id: 1,
        inputs: mono(PortDir::Input),
        outputs: mono(PortDir::Output),
    }))?;
    store.apply_mutation(Mutation::AddNode(NodeSnapshot {
        id: 2,
        inputs: mono(PortDir::Input),
        outputs: vec![],
    }))?;
    store.apply_mutation(Mutation::AddLink(SnapshotEdge {
        from: (0, 0),
        to: (1, 0),
    }))?;
    store.apply_mutation(Mutation::AddLink(SnapshotEdge {
        from: (1, 0),
        to: (2, 0),
    }))?;

    // Wait for the rebuild task to deposit a 3-node graph, then run it.
    let deadline = Instant::now() + Duration::from_secs(2);
    while {
        let guard = slot.lock().expect("slot");
        guard
            .as_ref()
            .map_or((0_usize, 0_usize), |g| (g.node_count(), g.link_count()))
    } < (3, 2)
    {
        if Instant::now() > deadline {
            return Err("engine-live-rebuild: phase A rebuild never landed".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let graph_a = slot.lock().expect("slot").take().unwrap();
    let mut eng = GraphEngine::new(graph_a, slot.clone());
    for cycle in 0..4u64 {
        let mut ctx = ProcessContext::new(NF, cycle * NF as u64, SR);
        eng.step(&mut ctx);
    }
    let peak_a = eng.graph().read_input(SINK, 0).map_or(0.0, |f| {
        f.samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max)
    });
    if !(0.45..=0.55).contains(&peak_a) {
        return Err(format!(
            "engine-live-rebuild: phase A peak {peak_a:.4} not ~0.5 (gain 1.0 × sine 0.5)"
        )
        .into());
    }

    // Phase B: change the gain node's factor to 0.5 (as control-api would mutate
    // a node param) and trigger a rebuild by re-applying AddNode(1) (idempotent
    // replace → NodeAdded event → rebuild task rebuilds with the new gain).
    gain_milli.store(500, Ordering::SeqCst); // 0.5
    store.apply_mutation(Mutation::AddNode(NodeSnapshot {
        id: 1,
        inputs: mono(PortDir::Input),
        outputs: mono(PortDir::Output),
    }))?;

    // Step the engine until it swaps in the rebuilt (gain 0.5) graph.
    let deadline = Instant::now() + Duration::from_secs(2);
    let peak_b = loop {
        for cycle in 0..2u64 {
            let mut ctx = ProcessContext::new(NF, cycle * NF as u64, SR);
            eng.step(&mut ctx);
        }
        let pb = eng.graph().read_input(SINK, 0).map_or(0.0, |f| {
            f.samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max)
        });
        if pb < peak_a * 0.6 {
            break pb; // gain dropped → rebuilt graph swapped in
        }
        if Instant::now() > deadline {
            return Err(format!(
                "engine-live-rebuild: phase B swap never reflected (peak_b {pb:.4})"
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if !(0.20..=0.30).contains(&peak_b) {
        return Err(
            format!("engine-live-rebuild: phase B peak {peak_b:.4} not ~0.25 (gain 0.5)").into(),
        );
    }

    println!(
        "engine-live-rebuild OK — TopologyEvent→rebuild→swap: gain 1.0→0.5, peak {peak_a:.4}→{peak_b:.4}"
    );
    tracing::info!(
         "engine-live-rebuild OK: gain 1.0->0.5 via TopologyEvent rebuild, peak {peak_a:.4} -> {peak_b:.4}"
    );
    Ok(())
}

/// Gateway-bridge end-to-end live-reload self-test (no hardware/servers).
///
/// Combines `spawn_rebuild_task` (TopologyEvent-driven) with `GatewayBridge`
/// (gateway survives rebuild): the topology has a bridge-source → gain →
/// bridge-sink. A simulated gateway worker pushes/pops via the SAME bridge API
/// across a rebuild that changes the gain 1.0→0.5; the engine swaps in the
/// rebuilt graph and the worker keeps flowing audio at the new gain — proving
/// the full live-reload chain with gateway survival.
fn run_gateway_live_reload_test() -> Result<(), Box<dyn std::error::Error>> {
    use audio_engine::{
        builtins, rebuild_slot, spawn_rebuild_task, BuiltNode, GatewayBridge, GraphEngine,
        NodeFactory,
    };
    use audio_graph_bsd::{NodeSnapshot, PortDir, PortMeta, SampleFmt, SnapshotEdge};
    use session_store::{Mutation, RaftEngine, SessionStore};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    const NF: usize = 64;
    const SR: u32 = 48_000;

    let mono = |direction: PortDir| {
        vec![PortMeta {
            direction,
            channels: 1,
            sample_format: SampleFmt::F32,
        }]
    };
    let sine = || {
        AudioFrame::from_planar(
            1,
            SR,
            (0..NF)
                .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SR as f32).sin() * 0.5)
                .collect::<Vec<_>>(),
        )
    };

    /// Factory: id 0=bridge source, 1=Gain(mutable), 2=bridge sink.
    struct ServerFactory {
        bridge: Arc<GatewayBridge>,
        gain_milli: Arc<AtomicU32>,
    }
    impl NodeFactory for ServerFactory {
        fn build(&self, id: audio_engine::NodeId) -> Option<BuiltNode> {
            match id {
                0 => Some(self.bridge.make_source_node()),
                1 => Some(BuiltNode::Plain(Box::new(builtins::Gain::new(
                    self.gain_milli.load(Ordering::SeqCst) as f32 / 1000.0,
                )))),
                2 => Some(self.bridge.make_sink_node()),
                _ => None,
            }
        }
    }

    let store: Arc<dyn SessionStore> = Arc::new(RaftEngine::default());
    let bridge = Arc::new(GatewayBridge::new(1, SR, NF));
    let gain_milli = Arc::new(AtomicU32::new(1000)); // 1.0
    let factory = Arc::new(ServerFactory {
        bridge: bridge.clone(),
        gain_milli: gain_milli.clone(),
    });
    let slot = rebuild_slot();
    let config = audio_engine::GraphConfig::new(NF, SR, 1);
    let _rebuild = spawn_rebuild_task(store.clone(), config, factory, slot.clone());

    // Topology: bridge-source(0) → gain(1) → bridge-sink(2).
    store.apply_mutation(Mutation::AddNode(NodeSnapshot {
        id: 0,
        inputs: vec![],
        outputs: mono(PortDir::Output),
    }))?;
    store.apply_mutation(Mutation::AddNode(NodeSnapshot {
        id: 1,
        inputs: mono(PortDir::Input),
        outputs: mono(PortDir::Output),
    }))?;
    store.apply_mutation(Mutation::AddNode(NodeSnapshot {
        id: 2,
        inputs: mono(PortDir::Input),
        outputs: vec![],
    }))?;
    store.apply_mutation(Mutation::AddLink(SnapshotEdge {
        from: (0, 0),
        to: (1, 0),
    }))?;
    store.apply_mutation(Mutation::AddLink(SnapshotEdge {
        from: (1, 0),
        to: (2, 0),
    }))?;

    // Wait for the rebuild to deposit a 3-node graph, then run it.
    let deadline = Instant::now() + Duration::from_secs(2);
    while {
        let guard = slot.lock().expect("slot");
        guard
            .as_ref()
            .map_or((0_usize, 0_usize), |g| (g.node_count(), g.link_count()))
    } < (3, 2)
    {
        if Instant::now() > deadline {
            return Err("gateway-live-reload: initial rebuild never landed".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let graph_a = slot.lock().expect("slot").take().unwrap();
    let mut eng = GraphEngine::new(graph_a, slot.clone());

    // Phase A: gateway worker pushes a sine, engine processes+flushes, worker pops.
    bridge.push_inbound(sine()).expect("push A");
    let mut ctx = ProcessContext::new(NF, 0, SR);
    eng.step(&mut ctx);
    let peak_a = match bridge.pop_outbound() {
        Ok(f) => f.samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max),
        Err(_) => 0.0,
    };
    if !(0.4..=0.6).contains(&peak_a) {
        return Err(format!(
            "gateway-live-reload: phase A peak {peak_a:.4} not ~0.5 (gain 1.0 × sine 0.5)"
        )
        .into());
    }

    // Phase B: change the gain to 0.5 (as control-api would) + trigger a rebuild.
    gain_milli.store(500, Ordering::SeqCst); // 0.5
    store.apply_mutation(Mutation::AddNode(NodeSnapshot {
        id: 1,
        inputs: mono(PortDir::Input),
        outputs: mono(PortDir::Output),
    }))?;

    // IMPORTANT timing: the rebuild task updates the bridge to the NEW rings at
    // build time, but the engine only swaps to the new graph at end-of-step. So
    // first poll the SLOT until the rebuilt graph lands, then step (swap), THEN
    // push/step/pop — so the bridge's new rings line up with the engine's graph.
    let deadline = Instant::now() + Duration::from_secs(2);
    while slot.lock().expect("slot").is_none() {
        if Instant::now() > deadline {
            return Err("gateway-live-reload: phase B rebuild never landed".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    eng.step(&mut ctx); // takes graph B from the slot, swaps A→B between cycles

    // The SAME bridge API now drives graph B (new rings) — gateway survived.
    bridge.push_inbound(sine()).expect("push B");
    eng.step(&mut ctx); // graph B processes + flushes
    let peak_b = match bridge.pop_outbound() {
        Ok(f) => f.samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max),
        Err(_) => 0.0,
    };
    if !(0.2..=0.3).contains(&peak_b) {
        return Err(
            format!("gateway-live-reload: phase B peak {peak_b:.4} not ~0.25 (gain 0.5)").into(),
        );
    }

    println!(
        "gateway-live-reload OK — bridge survived rebuild: gain 1.0→0.5, peak {peak_a:.4}→{peak_b:.4}"
    );
    tracing::info!(
         "gateway-live-reload OK: bridge survived rebuild, gain 1.0->0.5, peak {peak_a:.4} -> {peak_b:.4}"
    );
    Ok(())
}

/// Render an `AudioNode` from a `kind` string and optional typed params.
///
/// When `params` is `None` or the variant does not match `kind`, per-kind
/// defaults are used (matching ADR 0004's default-parameter policy). Unknown
/// kinds fall back to a `Gain(1.0)` passthrough so the rebuild never
/// hard-fails.
fn render_node(
    kind: Option<&str>,
    params: Option<control_api::NodeParams>,
    sample_rate: u32,
    channels: u16,
) -> audio_engine::BuiltNode {
    use audio_engine::{builtins, nodes, BuiltNode};
    use control_api::NodeParams;

    match (kind, params) {
        (Some("gain"), Some(NodeParams::Gain { gain })) => {
            BuiltNode::Plain(Box::new(builtins::Gain::new(gain)))
        }
        (Some("gain"), _) => BuiltNode::Plain(Box::new(builtins::Gain::new(0.5))),

        (
            Some("eq"),
            Some(NodeParams::Eq {
                filter_type,
                freq,
                gain_db,
                q,
            }),
        ) => BuiltNode::Plain(Box::new(nodes::EqNode::new(
            parse_filter_type(&filter_type),
            freq,
            gain_db,
            q,
            sample_rate,
            channels,
        ))),
        (Some("eq"), _) => BuiltNode::Plain(Box::new(nodes::EqNode::new(
            nodes::FilterType::Peaking,
            1000.0,
            0.0,
            0.707,
            sample_rate,
            channels,
        ))),

        (
            Some("compressor"),
            Some(NodeParams::Compressor {
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                makeup_db,
            }),
        ) => BuiltNode::Plain(Box::new(nodes::CompressorNode::new(
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            makeup_db,
            sample_rate,
            channels,
        ))),
        (Some("compressor"), _) => BuiltNode::Plain(Box::new(nodes::CompressorNode::new(
            -12.0,
            4.0,
            1.0,
            50.0,
            0.0,
            sample_rate,
            channels,
        ))),

        (Some("limiter"), Some(NodeParams::Limiter { threshold_db })) => {
            BuiltNode::Plain(Box::new(nodes::LimiterNode::new(threshold_db, channels)))
        }
        (Some("limiter"), _) => BuiltNode::Plain(Box::new(nodes::LimiterNode::new(-1.0, channels))),

        (Some("meter"), _) => BuiltNode::Plain(Box::new(nodes::MeterNode::new(channels))),

        (Some("mixer"), Some(NodeParams::Mixer { inputs, gains })) => {
            BuiltNode::Plain(Box::new(nodes::MixerNode::new(inputs, gains, channels)))
        }
        (Some("mixer"), _) => {
            BuiltNode::Plain(Box::new(nodes::MixerNode::new(2, vec![0.5, 0.5], channels)))
        }

        (Some("channel_map"), Some(NodeParams::ChannelMap { mode, pan })) => {
            BuiltNode::Plain(Box::new(nodes::ChannelMapNode::new(
                channels,
                parse_channel_mode(&mode, pan),
            )))
        }
        (Some("channel_map"), _) => BuiltNode::Plain(Box::new(nodes::ChannelMapNode::new(
            channels,
            nodes::ChannelMode::Passthrough,
        ))),

        (
            Some("delay"),
            Some(NodeParams::Delay {
                max_delay_ms,
                delay_ms,
                feedback,
                mix,
            }),
        ) => {
            let sr = sample_rate as f32;
            let max_samples = (max_delay_ms * 0.001 * sr) as usize;
            let delay_samples = (delay_ms * 0.001 * sr) as usize;
            BuiltNode::Plain(Box::new(nodes::DelayNode::new(
                max_samples,
                delay_samples,
                feedback,
                mix,
                channels,
            )))
        }
        (Some("delay"), _) => BuiltNode::Plain(Box::new(nodes::DelayNode::new(
            (0.5 * sample_rate as f32) as usize,
            (0.25 * sample_rate as f32) as usize,
            0.3,
            0.3,
            channels,
        ))),

        (
            Some("noise_gate"),
            Some(NodeParams::NoiseGate {
                threshold_db,
                attack_ms,
                hold_ms,
                release_ms,
            }),
        ) => BuiltNode::Plain(Box::new(nodes::NoiseGateNode::new(
            threshold_db,
            attack_ms,
            hold_ms,
            release_ms,
            sample_rate,
            channels,
        ))),
        (Some("noise_gate"), _) => BuiltNode::Plain(Box::new(nodes::NoiseGateNode::new(
            -50.0,
            1.0,
            50.0,
            100.0,
            sample_rate,
            channels,
        ))),

        (Some("noise"), Some(NodeParams::Noise { color, amp, seed })) => {
            BuiltNode::Plain(Box::new(nodes::NoiseSource::new(
                parse_noise_color(&color),
                amp,
                seed,
                channels,
            )))
        }
        (Some("noise"), _) => BuiltNode::Plain(Box::new(nodes::NoiseSource::new(
            nodes::NoiseColor::White,
            0.5,
            12_345,
            channels,
        ))),

        (
            Some("tone"),
            Some(NodeParams::Tone {
                waveform,
                freq,
                amp,
            }),
        ) => BuiltNode::Plain(Box::new(nodes::ToneGenerator::new(
            parse_waveform(&waveform),
            freq,
            amp,
            sample_rate,
            channels,
        ))),
        (Some("tone"), _) => BuiltNode::Plain(Box::new(nodes::ToneGenerator::new(
            nodes::Waveform::Sine,
            440.0,
            0.5,
            sample_rate,
            channels,
        ))),

        (
            Some("reverb"),
            Some(NodeParams::Reverb {
                room_size,
                damping,
                wet,
                dry,
            }),
        ) => BuiltNode::Plain(Box::new(nodes::ReverbNode::new(
            room_size,
            damping,
            wet,
            dry,
            sample_rate,
            channels,
        ))),
        (Some("reverb"), _) => BuiltNode::Plain(Box::new(nodes::ReverbNode::new(
            0.5,
            0.5,
            0.3,
            0.7,
            sample_rate,
            channels,
        ))),

        (
            Some("chorus"),
            Some(NodeParams::Chorus {
                rate_hz,
                depth_ms,
                center_delay_ms,
                mix,
            }),
        ) => BuiltNode::Plain(Box::new(nodes::ChorusNode::new(
            rate_hz,
            depth_ms,
            center_delay_ms,
            mix,
            sample_rate,
            channels,
        ))),
        (Some("chorus"), _) => BuiltNode::Plain(Box::new(nodes::ChorusNode::new(
            1.5,
            3.0,
            20.0,
            0.5,
            sample_rate,
            channels,
        ))),

        (
            Some("distortion"),
            Some(NodeParams::Distortion {
                mode,
                drive,
                threshold,
                output_level,
            }),
        ) => BuiltNode::Plain(Box::new(nodes::DistortionNode::new(
            parse_distortion_mode(&mode),
            drive,
            threshold,
            output_level,
            channels,
        ))),
        (Some("distortion"), _) => BuiltNode::Plain(Box::new(nodes::DistortionNode::new(
            nodes::DistortionMode::SoftClip,
            3.0,
            0.7,
            1.0,
            channels,
        ))),

        (
            Some("flanger"),
            Some(NodeParams::Flanger {
                rate_hz,
                depth_ms,
                center_delay_ms,
                feedback,
                mix,
            }),
        ) => BuiltNode::Plain(Box::new(nodes::FlangerNode::new(
            rate_hz,
            depth_ms,
            center_delay_ms,
            feedback,
            mix,
            sample_rate,
            channels,
        ))),
        (Some("flanger"), _) => BuiltNode::Plain(Box::new(nodes::FlangerNode::new(
            0.5,
            2.0,
            3.0,
            0.5,
            0.5,
            sample_rate,
            channels,
        ))),

        (Some("aux_send"), Some(NodeParams::AuxSend { send_level })) => {
            BuiltNode::Plain(Box::new(nodes::AuxSendNode::new(send_level, channels)))
        }
        (Some("aux_send"), _) => BuiltNode::Plain(Box::new(nodes::AuxSendNode::new(0.5, channels))),

        (
            Some("phaser"),
            Some(NodeParams::Phaser {
                rate_hz,
                base_freq,
                depth,
                feedback,
                mix,
                stages,
            }),
        ) => BuiltNode::Plain(Box::new(nodes::PhaserNode::new(
            rate_hz,
            base_freq,
            depth,
            feedback,
            mix,
            stages as usize,
            sample_rate,
            channels,
        ))),
        (Some("phaser"), _) => BuiltNode::Plain(Box::new(nodes::PhaserNode::new(
            0.5,
            800.0,
            0.5,
            0.3,
            0.5,
            4,
            sample_rate,
            channels,
        ))),

        (Some("bitcrusher"), Some(NodeParams::Bitcrusher { bits, hold_factor })) => {
            BuiltNode::Plain(Box::new(nodes::BitcrusherNode::new(
                bits,
                hold_factor as usize,
                channels,
            )))
        }
        (Some("bitcrusher"), _) => {
            BuiltNode::Plain(Box::new(nodes::BitcrusherNode::new(8, 1, channels)))
        }

        (Some("tremolo"), Some(NodeParams::Tremolo { rate_hz, depth })) => {
            BuiltNode::Plain(Box::new(nodes::TremoloNode::new(
                rate_hz,
                depth,
                sample_rate,
                channels,
            )))
        }
        (Some("tremolo"), _) => BuiltNode::Plain(Box::new(nodes::TremoloNode::new(
            5.0,
            0.5,
            sample_rate,
            channels,
        ))),

        (Some("stereo_widener"), Some(NodeParams::StereoWidener { width })) => {
            BuiltNode::Plain(Box::new(nodes::StereoWidenerNode::new(width, channels)))
        }
        (Some("stereo_widener"), _) => {
            BuiltNode::Plain(Box::new(nodes::StereoWidenerNode::new(1.0, channels)))
        }

        _ => BuiltNode::Plain(Box::new(builtins::Gain::new(1.0))),
    }
}

/// Parse a REST filter-type string into an `audio_engine::FilterType`.
fn parse_filter_type(s: &str) -> audio_engine::nodes::FilterType {
    use audio_engine::nodes::FilterType;
    match s.to_ascii_lowercase().as_str() {
        "lowpass" => FilterType::LowPass,
        "highpass" => FilterType::HighPass,
        "bandpass" => FilterType::BandPass,
        "peaking" => FilterType::Peaking,
        "lowshelf" => FilterType::LowShelf,
        "highshelf" => FilterType::HighShelf,
        _ => FilterType::Peaking,
    }
}

/// Parse a REST channel-mode string (+ optional pan) into a `ChannelMode`.
fn parse_channel_mode(s: &str, pan: Option<f32>) -> audio_engine::nodes::ChannelMode {
    use audio_engine::nodes::ChannelMode;
    match s.to_ascii_lowercase().as_str() {
        "passthrough" => ChannelMode::Passthrough,
        "swap" => ChannelMode::Swap,
        "mute_left" => ChannelMode::MuteLeft,
        "mute_right" => ChannelMode::MuteRight,
        "pan" => ChannelMode::Pan(pan.unwrap_or(0.0)),
        "mono_to_stereo" => ChannelMode::MonoToStereo,
        "stereo_to_mono" => ChannelMode::StereoToMono,
        _ => ChannelMode::Passthrough,
    }
}

/// Parse a REST noise-color string into a `NoiseColor`.
fn parse_noise_color(s: &str) -> audio_engine::nodes::NoiseColor {
    use audio_engine::nodes::NoiseColor;
    match s.to_ascii_lowercase().as_str() {
        "pink" => NoiseColor::Pink,
        _ => NoiseColor::White,
    }
}

/// Parse a REST waveform string into a `Waveform`.
fn parse_waveform(s: &str) -> audio_engine::nodes::Waveform {
    use audio_engine::nodes::Waveform;
    match s.to_ascii_lowercase().as_str() {
        "square" => Waveform::Square,
        "saw" => Waveform::Saw,
        "triangle" => Waveform::Triangle,
        _ => Waveform::Sine,
    }
}

/// Parse a REST distortion-mode string into a `DistortionMode`.
fn parse_distortion_mode(s: &str) -> audio_engine::nodes::DistortionMode {
    use audio_engine::nodes::DistortionMode;
    match s.to_ascii_lowercase().as_str() {
        "hard_clip" | "hardclip" => DistortionMode::HardClip,
        "foldback" => DistortionMode::Foldback,
        "overdrive" => DistortionMode::Overdrive,
        _ => DistortionMode::SoftClip,
    }
}
///
/// Unlike the default mode (owned-handle gateway + ad-hoc RT loop), this mode
/// builds the graph via `audio-engine` (`GatewayBridge` source/sink + `spawn_rebuild_task`
/// topology-driven rebuild + `GraphEngine` process/flush/swap) and drives the
/// browser WS worker via `BrowserGateway::serve_with_io` over the bridge — so a
/// topology change (e.g. via the REST control-api) rebuilds the live graph and
/// the WS client transparently survives the swap.
///
/// Limitation: the engine factory only renders the bridge source/sink node ids;
/// arbitrary control-api-created nodes need a `kind` registry to be rendered
/// (future). REST + `/metrics` are wired identically to the default mode.
async fn run_server_engine(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    use audio_engine::{
        builtins, nodes, rebuild_slot, spawn_rebuild_task, BuiltNode, GatewayBridge, GraphEngine,
        NodeFactory,
    };
    use audio_graph_bsd::{NodeSnapshot, PortDir, PortMeta, SampleFmt, SnapshotEdge};
    use session_store::{Mutation, RaftEngine, SessionStore};

    // --- Shared registries (control-api + seeding write; factory reads) ------
    let kinds: control_api::KindRegistry =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let params: control_api::ParamsRegistry =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

    /// A decoded audio file ready to be (re)built into a `FileSource` node.
    #[derive(Clone)]
    struct FileBuffer {
        /// Planar f32 samples (`[ch0…, ch1…]`). Cloned into a fresh
        /// `FileSource` on every rebuild — a full copy per build, accepted
        /// at single-file scale (Arc-sharing the buffer is out of scope).
        samples: Vec<f32>,
        channels: u16,
        sample_rate: u32,
        looping: bool,
    }
    /// Loaded audio file buffers addressable by node id — the factory clones
    /// the buffer into a fresh `FileSource` on each (re)build.
    type FileBufferRegistry =
        Arc<std::sync::RwLock<std::collections::HashMap<audio_engine::NodeId, FileBuffer>>>;
    let files: FileBufferRegistry =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

    // --- --load-file: decode once up-front (worker thread — file I/O) --------
    let mut loaded: Option<FileBuffer> = None;
    if let Some(path) = args.load_file.clone() {
        let decode_path = path.clone();
        match tokio::task::spawn_blocking(move || {
            nodes::load_file_source(std::path::Path::new(&decode_path), true)
        })
        .await
        {
            Ok(Ok(node)) => {
                let (samples, channels, sample_rate, looping) = node.into_parts();
                tracing::info!(
                    path = %path,
                    channels,
                    sample_rate,
                    frames = samples.len() / usize::from(channels.max(1)),
                    "--load-file decoded (looping FileSource)"
                );
                loaded = Some(FileBuffer {
                    samples,
                    channels,
                    sample_rate,
                    looping,
                });
            }
            Ok(Err(e)) => tracing::error!(
                error = %e,
                path = %path,
                "--load-file decode failed; booting without the file source"
            ),
            Err(e) => tracing::error!(
                error = %e,
                path = %path,
                "--load-file decode task failed; booting without the file source"
            ),
        }
    }

    // --- Session store (seed an empty topology with bridge source → sink) ----
    let store_path = std::env::temp_dir().join(DEV_STORE_PATH);
    let store: Arc<dyn SessionStore> = Arc::new(RaftEngine::open(&store_path)?);
    let preset_path = std::env::temp_dir().join(DEV_PRESET_PATH);

    // Restore kind/params (+topology) from the preset sidecar if present.
    // The redb store already persists topology; the preset import REPLACES it with
    // the exported state (which includes kind/params the store lacks).
    if preset_path.exists() {
        match control_api::Preset::from_json_file(&preset_path) {
            Ok(preset) => {
                let restore_ctrl = control_api::GraphController::new_with_registries(
                    Arc::clone(&store),
                    Arc::clone(&kinds),
                    Arc::clone(&params),
                );
                match restore_ctrl.import_preset(&preset) {
                    Ok(()) => tracing::info!(path = %preset_path.display(), "preset restored"),
                    Err(e) => {
                        tracing::warn!(error = %e, "preset restore failed; continuing with stored topology")
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "preset file unreadable; ignoring"),
        }
    }

    let topo = store.get_topology();
    let topo_empty = topo.nodes.is_empty();
    // File node id: max existing id + 1. On a fresh store ids 0/1 are
    // reserved by the bridge seeding below, so the file lands at 2.
    let file_id: audio_engine::NodeId = if topo_empty {
        2
    } else {
        topo.nodes.iter().map(|n| n.id).max().map_or(0, |m| m + 1)
    };
    let file_snapshot = loaded.as_ref().map(|buf| NodeSnapshot {
        id: file_id,
        inputs: vec![],
        outputs: vec![PortMeta {
            direction: PortDir::Output,
            channels: buf.channels,
            sample_format: SampleFmt::F32,
        }],
    });

    if topo_empty {
        let mono = |direction: PortDir| {
            vec![PortMeta {
                direction,
                channels: CHANNELS,
                sample_format: SampleFmt::F32,
            }]
        };
        // The bridge source(0) is always created for id consistency; with a
        // loaded file of matching channel count it stays UNLINKED — the sink
        // has a single input port, so the file node takes the 0→1 baseline's
        // place instead of adding a second source into the same port.
        store.apply_mutation(Mutation::AddNode(NodeSnapshot {
            id: 0,
            inputs: vec![],
            outputs: mono(PortDir::Output),
        }))?;
        store.apply_mutation(Mutation::AddNode(NodeSnapshot {
            id: 1,
            inputs: mono(PortDir::Input),
            outputs: vec![],
        }))?;
        let file_matches_graph = matches!(&loaded, Some(buf) if buf.channels == CHANNELS);
        if let Some(snapshot) = &file_snapshot {
            store.apply_mutation(Mutation::AddNode(snapshot.clone()))?;
        }
        if file_matches_graph {
            store.apply_mutation(Mutation::AddLink(SnapshotEdge {
                from: (file_id, 0),
                to: (1, 0),
            }))?;
            tracing::info!(
                node = file_id,
                "--load-file: file source linked to the bridge sink (0→1 baseline omitted)"
            );
        } else {
            // Baseline bridge passthrough. With a mismatched-channel file the
            // file node was added above but stays UNLINKED — `link` would be
            // `PortIncompatible` against the stereo sink (wire it via REST,
            // e.g. through a channel_map node).
            store.apply_mutation(Mutation::AddLink(SnapshotEdge {
                from: (0, 0),
                to: (1, 0),
            }))?;
            if loaded.is_some() {
                tracing::warn!(
                    node = file_id,
                    file_channels = loaded.as_ref().map_or(0, |b| b.channels),
                    graph_channels = CHANNELS,
                    "--load-file: channel count differs from the graph; node added unlinked"
                );
            }
        }
    } else if let Some(snapshot) = &file_snapshot {
        // Existing topology: add the node only; wiring is REST-owned (the
        // bridge sink's single input may already be taken).
        store.apply_mutation(Mutation::AddNode(snapshot.clone()))?;
        tracing::info!(
            node = file_id,
            "--load-file: file node added to the existing topology; link it via the control API"
        );
    }

    // Register kind + buffer so the rebuild factory renders a FileSource.
    if let Some(buf) = loaded {
        kinds
            .write()
            .expect("kinds lock")
            .insert(file_id, "file".to_string());
        files.write().expect("files lock").insert(file_id, buf);
    }

    // --- Gateway bridge + factory (source/sink survive rebuilds) ------------
    /// Factory: id 0 = bridge source, id 1 = bridge sink; other ids = a 1-in/1-out
    /// passthrough Gain(1.0) so the rebuild never hard-fails on an unknown node
    /// (proper kind-based rendering is a follow-up). A port-count mismatch would
    /// surface as a build_graph error (traced by the rebuild task) — acceptable.
    struct EngineServerFactory {
        bridge: Arc<GatewayBridge>,
        /// Shared kind registry (written by control-api create_node). Lets the
        /// factory render REST-created nodes meaningfully: kind "gain" → a real
        /// attenuating Gain(0.5); anything else → a Gain(1.0) passthrough.
        kinds: control_api::KindRegistry,
        /// Shared params registry (written by control-api create_node). Lets the
        /// factory render REST-created nodes with user-supplied parameters
        /// instead of defaults.
        params: control_api::ParamsRegistry,
        /// Decoded file buffers for kind "file" nodes (written once by
        /// `--load-file` seeding). A fresh `FileSource` is cloned from the
        /// buffer on every rebuild; a missing buffer falls back to silence.
        files: FileBufferRegistry,
    }
    impl NodeFactory for EngineServerFactory {
        fn build(&self, id: audio_engine::NodeId) -> Option<BuiltNode> {
            match id {
                0 => Some(self.bridge.make_source_node()),
                1 => Some(self.bridge.make_sink_node()),
                other => {
                    let kind = self.kinds.read().expect("kinds lock").get(&other).cloned();
                    // "file" nodes carry a decoded sample buffer (too large for
                    // NodeParams), so intercept BEFORE the render_node dispatch.
                    if kind.as_deref() == Some("file") {
                        let buf = self.files.read().expect("files lock").get(&other).cloned();
                        return Some(match buf {
                            Some(f) => BuiltNode::Plain(Box::new(nodes::FileSource::new(
                                f.samples.clone(),
                                f.channels,
                                f.sample_rate,
                                f.looping,
                            ))),
                            None => BuiltNode::Plain(Box::new(builtins::Gain::new(0.0))),
                        });
                    }
                    let params = self
                        .params
                        .read()
                        .expect("params lock")
                        .get(&other)
                        .cloned();
                    Some(render_node(kind.as_deref(), params, SAMPLE_RATE, CHANNELS))
                }
            }
        }
    }

    let bridge = Arc::new(GatewayBridge::new(CHANNELS, SAMPLE_RATE, NUM_FRAMES));
    let factory = Arc::new(EngineServerFactory {
        bridge: bridge.clone(),
        kinds: Arc::clone(&kinds),
        params: Arc::clone(&params),
        files: Arc::clone(&files),
    });
    let slot = rebuild_slot();
    let config = audio_engine::GraphConfig::new(NUM_FRAMES, SAMPLE_RATE, CHANNELS);
    let _rebuild = spawn_rebuild_task(store.clone(), config, factory, slot.clone());

    // Wait for the initial rebuild (source+sink+link), then hand it to the engine.
    let deadline = Instant::now() + Duration::from_secs(2);
    while {
        let g = slot.lock().expect("slot");
        g.as_ref()
            .map_or((0_usize, 0_usize), |gr| (gr.node_count(), gr.link_count()))
    } < (2, 1)
    {
        if Instant::now() > deadline {
            return Err("server-engine: initial graph rebuild never landed".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let graph = slot.lock().expect("slot").take().expect("graph present");

    // --- RT thread: GraphEngine::step (process + flush + swap) + metrics ----
    let rt_recorder = Arc::new(MetricsRecorder::new());
    let rt_recorder_handle = Arc::clone(&rt_recorder);
    let rt_thread = std::thread::Builder::new()
        .name("sonicbrew-rt".into())
        .spawn(move || {
            let mut eng = GraphEngine::new(graph, slot);
            let mut position: u64 = 0;
            loop {
                let mut ctx = ProcessContext::new(NUM_FRAMES, position, SAMPLE_RATE);
                let t0 = Instant::now();
                eng.step(&mut ctx);
                rt_recorder_handle.record_cycle(t0.elapsed().as_micros() as u64);
                position += NUM_FRAMES as u64;
                std::thread::sleep(FRAME_DURATION);
            }
        })?;

    // Periodic preset autosave: export the full graph state (kind/params included)
    // and write it when the JSON changes. Latest-wins; abrupt kills lose ≤2s.
    let save_store = Arc::clone(&store);
    let save_kinds = Arc::clone(&kinds);
    let save_params = Arc::clone(&params);
    let save_path = preset_path.clone();
    let autosave = std::thread::Builder::new()
        .name("sonicbrew-autosave".into())
        .spawn(move || {
            let ctrl = control_api::GraphController::new_with_registries(
                save_store,
                save_kinds,
                save_params,
            );
            let mut last_written: Option<String> = None;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let preset = ctrl.export_preset();
                let json = match serde_json::to_string_pretty(&preset) {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::warn!(error = %e, "autosave: serialize failed");
                        continue;
                    }
                };
                if last_written.as_deref() == Some(json.as_str()) {
                    continue; // unchanged
                }
                // Atomic-ish write: temp file + rename so a crash never leaves a
                // half-written preset.
                let tmp = save_path.with_extension("json.tmp");
                match std::fs::write(&tmp, &json).and_then(|()| std::fs::rename(&tmp, &save_path)) {
                    Ok(()) => {
                        last_written = Some(json);
                        tracing::debug!("autosave: preset written");
                    }
                    Err(e) => tracing::warn!(error = %e, "autosave: write failed"),
                }
            }
        })?;
    drop(autosave); // detached — process-lifetime thread

    // --- Browser gateway via serve_with_io (bridge-driven, survives rebuild) -
    let gw = BrowserGateway::new()
        .with_listen_addr(args.ws_addr)
        .with_channels(CHANNELS)
        .with_sample_rate(SAMPLE_RATE)
        .with_num_frames(NUM_FRAMES);
    let ws_addr = gw.listen_addr();
    let push_bridge = Arc::clone(&bridge);
    let pop_bridge = Arc::clone(&bridge);
    let gw_task = tokio::spawn(async move {
        if let Err(e) = gw
            .serve_with_io(
                move |frame| push_bridge.push_inbound(frame),
                move || pop_bridge.pop_outbound(),
            )
            .await
        {
            tracing::error!(error = %e, "browser gateway exited");
        }
    });

    // --- Control REST API + /metrics (identical to default mode) ------------
    let api_store = Arc::clone(&store);
    let api_task = tokio::spawn(async move {
        if let Err(e) = control_api::RestApi::new_with_registries(
            api_store,
            Arc::clone(&kinds),
            Arc::clone(&params),
        )
        .serve(args.api_addr)
        .await
        {
            tracing::error!(error = %e, "control API exited");
        }
    });
    let metrics_rec = Arc::clone(&rt_recorder);
    let metrics_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Some(summary) = parse_metrics_summary(&metrics_rec.export_prometheus()) {
                tracing::info!(target: "sonicbrew::metrics", "{summary}");
            }
        }
    });
    let metrics_http_rec = Arc::clone(&rt_recorder);
    let metrics_endpoint_task = tokio::spawn(async move {
        if let Err(e) = serve_metrics(args.metrics_addr, metrics_http_rec).await {
            tracing::error!(error = %e, "metrics endpoint exited");
        }
    });

    tracing::info!(
        ws_addr = %ws_addr,
        api_addr = %args.api_addr,
        metrics_addr = %args.metrics_addr,
        preset_path = %preset_path.display(),
        "sonicbrew audio-engine server running — live-reload capable (Ctrl-C to shut down)"
    );

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    gw_task.abort();
    api_task.abort();
    metrics_task.abort();
    metrics_endpoint_task.abort();
    drop(rt_thread);
    Ok(())
}

/// Real-time tick loop, run on a dedicated OS thread that owns the `Graph`.
///
/// Each iteration builds a fresh [`ProcessContext`], runs one
/// [`Graph::process_cycle`] (which lets the inbound `RingSource` drain browser
/// frames into the graph), then — **between cycles (off-RT)** — calls
/// [`Graph::flush_sinks`] to ship every `RingSink`'s stashed frame across its
/// `rtrb` ring so the outbound worker (WS/RTP/Pulse) actually receives audio.
/// This resolves the outbound flush-gap (audio-graph-bsd 0.4.0 `Flushable`/
/// `SinkNode`/`add_sink`/`flush_sinks`). Finally it measures the cycle latency
/// and feeds it to the M14 [`MetricsRecorder`].
///
/// The loop owns the `Graph` by value (so `flush_sinks(&mut self)` is sound).
/// Live hot-reload via `RtHandle` is demonstrated by `--hot-reload-test`;
/// reconciling `flush_sinks` (`&mut`) with the shared `RtHandle` (`&self`) for
/// a single unified server loop is a follow-up.
fn run_rt_loop(mut graph: Graph, recorder: Arc<MetricsRecorder>) {
    let mut position: u64 = 0;
    loop {
        let mut ctx = ProcessContext::new(NUM_FRAMES, position, SAMPLE_RATE);
        let t0 = Instant::now();
        if let Err(e) = graph.process_cycle(&mut ctx) {
            tracing::error!(error = %e, "RT process_cycle failed; continuing");
        }
        // Off-RT between-cycle flush: drain every RingSink stash → outbound ring.
        let (flushed, ferr) = graph.flush_sinks();
        if let Some(e) = ferr {
            tracing::warn!(error = %e, flushed, "RT flush_sinks reported an error (xrun?)");
        }
        let us = t0.elapsed().as_micros() as u64;
        recorder.record_cycle(us);
        position += NUM_FRAMES as u64;
        std::thread::sleep(FRAME_DURATION);
    }
}

/// Renders a one-line latency summary from a Prometheus export, e.g.
/// `latency p50=12µs p99=18µs max=20µs`. Returns `None` if the expected
/// lines are absent (e.g. before any cycle has been recorded).
fn parse_metrics_summary(exported: &str) -> Option<String> {
    let mut p50 = None::<u64>;
    let mut p99 = None::<u64>;
    let mut max = None::<u64>;
    for line in exported.lines() {
        let (key, val) = line.split_once(' ')?;
        let val: u64 = val.parse().ok()?;
        if key.contains(r#"quantile="0.5""#) {
            p50 = Some(val);
        } else if key.contains(r#"quantile="0.99""#) {
            p99 = Some(val);
        } else if key.ends_with("_process_latency_us_max") {
            max = Some(val);
        }
    }
    match (p50, p99, max) {
        (Some(p50), Some(p99), Some(max)) => {
            Some(format!("latency p50={p50}µs p99={p99}µs max={max}µs"))
        }
        _ => None,
    }
}

/// Parses `std::env::args` manually (no clap dependency).
///
/// Supported flags:
/// - `--self-test` — deterministic integration check then exit 0.
/// - `--ws-addr <SOCKETADDR>` — browser gateway listen address.
/// - `--api-addr <SOCKETADDR>` — control API listen address.
/// - `--metrics-addr <SOCKETADDR>` — Prometheus `/metrics` scrape endpoint.
/// - `--load-file <PATH>` — decode an audio file (FLAC/WAV/PCM) into a
///   looping FileSource node at startup (`--server-engine` mode).
/// - `--help` / `-h` — print usage.
fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--self-test" => args.self_test = true,
            "--hot-reload-test" => args.hot_reload_test = true,
            "--live-rebuild-test" => args.live_rebuild_test = true,
            "--engine-live-rebuild-test" => args.engine_live_rebuild_test = true,
            "--gateway-live-reload-test" => args.gateway_live_reload_test = true,
            "--diagnose" => args.diagnose = true,
            "--server-engine" => args.server_engine = true,
            "--help" | "-h" => args.help = true,
            "--ws-addr" => {
                let v = iter.next().ok_or("--ws-addr requires a value")?;
                args.ws_addr = v.parse().map_err(|e| format!("--ws-addr '{v}': {e}"))?;
            }
            "--api-addr" => {
                let v = iter.next().ok_or("--api-addr requires a value")?;
                args.api_addr = v.parse().map_err(|e| format!("--api-addr '{v}': {e}"))?;
            }
            "--metrics-addr" => {
                let v = iter.next().ok_or("--metrics-addr requires a value")?;
                args.metrics_addr = v
                    .parse()
                    .map_err(|e| format!("--metrics-addr '{v}': {e}"))?;
            }
            "--load-file" => {
                let v = iter.next().ok_or("--load-file requires a value")?;
                args.load_file = Some(v);
            }
            other => return Err(format!("unknown argument '{other}' (try --help)")),
        }
    }
    Ok(args)
}

/// Prints a short usage summary.
fn print_usage() {
    println!("sonicbrew — distributed audio server (MVP binary)");
    println!();
    println!("USAGE:");
    println!("    sonicbrew [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    --self-test              Run the deterministic integration check and exit");
    println!("    --hot-reload-test        Run the RtHandle live-graph-swap check and exit");
    println!(
        "    --live-rebuild-test      Run the topology-driven from_snapshot rebuild check and exit"
    );
    println!(
        "    --engine-live-rebuild-test  Run the audio-engine end-to-end live-rebuild check and exit"
    );
    println!(
        "    --gateway-live-reload-test Run the gateway-bridge live-reload (survives rebuild) check and exit"
    );
    #[cfg(feature = "diagnose")]
    println!(
        "    --diagnose              Launch the interactive diagnostic TUI (signal waveform + metrics)"
    );
    println!(
        "    --server-engine         Boot the audio-engine server (live-reload via GatewayBridge + serve_with_io)"
    );
    println!("    --ws-addr <ADDR>         Browser gateway WebSocket address (default {DEFAULT_WS_ADDR})");
    println!("    --api-addr <ADDR>        Control REST API address (default {DEFAULT_API_ADDR})");
    println!("    --metrics-addr <ADDR>    Prometheus /metrics endpoint (default {DEFAULT_METRICS_ADDR})");
    println!(
        "    --load-file <PATH>       Load an audio file (FLAC/WAV/PCM) as a looping FileSource node at"
    );
    println!(
        "                             startup (server-engine mode; linked into the bridge when channels match)"
    );
    println!("    -h, --help               Print this help and exit");
    println!();
    println!("The default mode boots the browser gateway, the control API, and an");
    println!("idle real-time graph tick loop. Ctrl-C shuts down gracefully.");
    println!();
    println!("NOTE:");
    #[cfg(feature = "bluetooth")]
    println!(
        "    Bluetooth A2DP input is available (built with the `bluetooth` feature): {}",
        bt_input::describe_integration()
    );
    #[cfg(not(feature = "bluetooth"))]
    println!("    Bluetooth A2DP input is available behind the `bluetooth` feature");
    println!("    (FreeBSD runtime; compiles on Linux).");
}
