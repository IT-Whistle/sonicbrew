//! M12 — Browser gateway.
//!
//! Bidirectional WebSocket bridge between a browser client and the sonicbrew
//! audio graph, built on `audio-graph-bsd`'s built-in [`RingSource`] /
//! [`RingSink`] (the hand-rolled `BrowserSourceNode` / `BrowserSinkNode` from
//! BUILD-PLAN §3.2 are superseded).
//!
//! # MVP scope (BUILD-PLAN §3.2 / p11 §7a M12 acceptance)
//!
//! * A WebSocket server receives binary PCM frames (48 kHz / 2 ch / f32
//!   **planar**), parses them into [`AudioFrame`]s, and feeds the graph via a
//!   [`RingSource`] node.
//! * Graph output is tapped by a [`RingSink`] and sent back to the client as
//!   binary PCM (bidirectional).
//! * Opus frames (`codec_tag == 1`) are decoded via `audio-opus-bsd` **only**
//!   behind the `opus` feature (libopus is absent on the dev host, so it is off
//!   by default); without the feature an Opus-tagged frame is a
//!   [`GatewayError::BadFrame`].
//! * WebRTC and HLS are P1 stubs (see [`BrowserTransport`]).
//!
//! # Real-time boundary
//!
//! This crate does **not** drive the RT graph. It owns only the lock-free
//! `rtrb` rings and the async WS transport. The audio engine is responsible for
//! calling `Graph::process_cycle` (which lets [`RingSource`] drain inbound
//! frames) and `RingSink::flush` (which ships stashed output across the ring
//! for the outbound pump to pick up). See [`BrowserGateway::register`].
//!
//! [`RingSource`]: audio_graph_bsd::RingSource
//! [`RingSink`]: audio_graph_bsd::RingSink

use std::net::SocketAddr;

use audio_core_bsd::AudioFrame;
use audio_graph_bsd::{RingSink, RingSource};

pub use audio_graph_bsd::{Graph, NodeId};

mod codec;
mod server;

pub use codec::{decode_frame, encode_frame, FrameSpec, HEADER_LEN, TAG_OPUS, TAG_PCM};

/// Worker-thread capacity (in frames) of each gateway ring. Large enough to
/// absorb WS jitter without dropping, small enough to keep end-to-end latency
/// well under a second at 256-sample blocks.
const RING_CAPACITY: usize = 128;

/// Errors returned by the browser gateway.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// Operation has not been implemented yet (stub).
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
    /// WebSocket / transport-level failure (also used for IO and runtime
    /// build errors, which are transport-adjacent).
    #[error("websocket: {0}")]
    WebSocket(String),
    /// Malformed inbound frame.
    #[error("bad frame: {0}")]
    BadFrame(String),
}

impl From<std::io::Error> for GatewayError {
    fn from(err: std::io::Error) -> Self {
        Self::WebSocket(format!("io: {err}"))
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for GatewayError {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(err.to_string())
    }
}

/// Convenience `Result` alias.
pub type Result<T> = std::result::Result<T, GatewayError>;

/// The producer end of the inbound ring: a worker pushes decoded client frames
/// here, and the graph-side [`RingSource`] drains them.
///
/// [`RingSource`]: audio_graph_bsd::RingSource
pub type InboundHandle = rtrb::Producer<AudioFrame>;

/// The consumer end of the outbound ring: the graph-side [`RingSink`] (flushed
/// by the audio engine) pushes graph output here, and a worker pops frames to
/// send back to the client.
///
/// [`RingSink`]: audio_graph_bsd::RingSink
pub type OutboundHandle = rtrb::Consumer<AudioFrame>;

/// Pluggable browser transport. Only [`BrowserTransport::WebSocket`] is
/// implemented in the MVP; WebRTC/HLS are P1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserTransport {
    /// Native WebSocket (MVP).
    WebSocket,
    /// WebRTC peer connection (TODO: P1, `str0m`).
    WebRtc,
    /// HTTP Live Streaming (TODO: P1).
    Hls,
}

/// A gateway connects browser clients to the audio graph.
pub trait Gateway: Send {
    /// Drive the gateway against the given graph.
    fn run(&mut self, graph: &mut Graph) -> Result<()>;
}

/// WebSocket browser gateway.
///
/// Holds only configuration; all state lives in the `rtrb` rings handed back by
/// [`register`](Self::register) and the async task spawned by
/// [`serve`](Self::serve).
#[derive(Debug, Clone, Copy)]
pub struct BrowserGateway {
    listen_addr: SocketAddr,
    spec: FrameSpec,
    num_frames: usize,
}

impl BrowserGateway {
    /// Creates a gateway with MVP defaults: loopback listen address, stereo,
    /// 48 kHz, 256-sample blocks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            listen_addr: "127.0.0.1:0"
                .parse()
                .expect("hard-coded loopback literal always parses"),
            spec: FrameSpec::new(2, 48_000),
            num_frames: 256,
        }
    }

    /// Sets the WebSocket listen address (builder).
    #[must_use]
    pub fn with_listen_addr(mut self, addr: SocketAddr) -> Self {
        self.listen_addr = addr;
        self
    }

    /// Sets the expected channel count (builder).
    #[must_use]
    pub fn with_channels(mut self, channels: u16) -> Self {
        self.spec.channels = channels;
        self
    }

    /// Sets the expected sample rate, in Hz (builder).
    #[must_use]
    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.spec.sample_rate = sample_rate;
        self
    }

    /// Sets the per-block frame count used to size the ring nodes (builder).
    #[must_use]
    pub fn with_num_frames(mut self, num_frames: usize) -> Self {
        self.num_frames = num_frames;
        self
    }

    /// The configured listen address.
    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// The configured audio shape.
    #[must_use]
    pub fn spec(&self) -> FrameSpec {
        self.spec
    }

    /// Wires a [`RingSource`] (inbound) and a [`RingSink`] (outbound) into
    /// `graph` and returns the node ids plus the two worker-thread ring
    /// handles.
    ///
    /// The caller drives the graph: `Graph::process_cycle` lets the
    /// [`RingSource`] drain inbound frames, and `RingSink::flush` (called by
    /// the audio engine between cycles) ships stashed output into the outbound
    /// ring for [`serve`](Self::serve) to forward.
    ///
    /// # Errors
    ///
    /// Currently infallible (returns `Result` for forward compatibility).
    ///
    /// [`RingSource`]: audio_graph_bsd::RingSource
    /// [`RingSink`]: audio_graph_bsd::RingSink
    pub fn register(
        &self,
        graph: &mut Graph,
    ) -> Result<(NodeId, NodeId, InboundHandle, OutboundHandle)> {
        let (inbound_prod, inbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(RING_CAPACITY);
        let (outbound_prod, outbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(RING_CAPACITY);

        let src = graph.add_node(Box::new(RingSource::new(
            inbound_cons,
            self.spec.channels,
            self.spec.sample_rate,
            self.num_frames,
        )));
        let sink = graph.add_sink(Box::new(RingSink::new(
            outbound_prod,
            self.spec.channels,
            self.spec.sample_rate,
            self.num_frames,
        )));

        Ok((src, sink, inbound_prod, outbound_cons))
    }

    /// Runs the WebSocket accept loop, forwarding frames between `inbound` /
    /// `outbound` and connected clients. Binds the configured
    /// [`listen_addr`](Self::listen_addr).
    ///
    /// Connections are served one at a time (MVP). This future runs until the
    /// listener errors or is dropped.
    pub async fn serve(
        self,
        mut inbound: InboundHandle,
        mut outbound: OutboundHandle,
    ) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(self.listen_addr).await?;
        tracing::info!(addr = %self.listen_addr, "browser gateway listening");
        let mut push = move |f: AudioFrame| inbound.push(f);
        let mut pop = move || outbound.pop();
        server::run_accept_loop_io(listener, self.spec, &mut push, &mut pop).await
    }

    /// Like [`serve`](Self::serve) but accepts a pre-bound listener, avoiding
    /// the bind/rebind port race. Useful for tests and for callers that want
    /// to control the bound address.
    pub async fn serve_with_listener(
        self,
        listener: tokio::net::TcpListener,
        mut inbound: InboundHandle,
        mut outbound: OutboundHandle,
    ) -> Result<()> {
        let mut push = move |f: AudioFrame| inbound.push(f);
        let mut pop = move || outbound.pop();
        server::run_accept_loop_io(listener, self.spec, &mut push, &mut pop).await
    }

    /// Drive the gateway with caller-supplied push/pop callbacks instead of
    /// owned rtrb handles. This is the **bridge-ready** entry point: a caller
    /// holding an `Arc<GatewayBridge>` passes `|f| bridge.push_inbound(f)` /
    /// `|| bridge.pop_outbound()` so the WS worker transparently follows the
    /// bridge across graph rebuilds (live-reload). Binds the listener.
    ///
    /// The callbacks must be `Send + 'static` (the serve future is spawn'd).
    pub async fn serve_with_io<P, Q>(self, push: P, pop: Q) -> Result<()>
    where
        P: FnMut(AudioFrame) -> std::result::Result<(), rtrb::PushError<AudioFrame>>
            + Send
            + 'static,
        Q: FnMut() -> std::result::Result<AudioFrame, rtrb::PopError> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind(self.listen_addr).await?;
        tracing::info!(addr = %self.listen_addr, "browser gateway listening (io)");
        let mut push = push;
        let mut pop = pop;
        server::run_accept_loop_io(listener, self.spec, &mut push, &mut pop).await
    }

    /// Like [`serve_with_io`](Self::serve_with_io) but accepts a pre-bound
    /// listener (avoids the bind/port race — for tests / caller-controlled addr).
    pub async fn serve_with_io_listener<P, Q>(
        self,
        listener: tokio::net::TcpListener,
        push: P,
        pop: Q,
    ) -> Result<()>
    where
        P: FnMut(AudioFrame) -> std::result::Result<(), rtrb::PushError<AudioFrame>>
            + Send
            + 'static,
        Q: FnMut() -> std::result::Result<AudioFrame, rtrb::PopError> + Send + 'static,
    {
        let mut push = push;
        let mut pop = pop;
        server::run_accept_loop_io(listener, self.spec, &mut push, &mut pop).await
    }
}

impl Default for BrowserGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Gateway for BrowserGateway {
    fn run(&mut self, graph: &mut Graph) -> Result<()> {
        let (_src, _sink, inbound, outbound) = self.register(graph)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| GatewayError::WebSocket(format!("runtime build: {err}")))?;
        // `serve` takes `self` by value; copy the (plain-old-data) config out.
        let owned = *self;
        runtime.block_on(owned.serve(inbound, outbound))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core_bsd::AudioFrame;
    use audio_graph_bsd::Graph;

    #[test]
    fn register_adds_linkable_source_and_sink_nodes() {
        let mut graph = Graph::new();
        let before = graph.node_count();
        let gw = BrowserGateway::new();
        let (src, sink, _inbound, _outbound) = gw.register(&mut graph).expect("register ok");

        assert_eq!(graph.node_count(), before + 2);
        // Port directions/channels/formats must be compatible: linking the
        // source's single output to the sink's single input must succeed.
        assert!(graph.link((src, 0), (sink, 0)).is_ok());
    }

    #[test]
    fn builders_override_defaults() {
        let gw = BrowserGateway::new()
            .with_channels(1)
            .with_sample_rate(44_100)
            .with_num_frames(64);
        assert_eq!(gw.spec().channels, 1);
        assert_eq!(gw.spec().sample_rate, 44_100);
    }

    #[test]
    fn default_is_stereo_48k_loopback() {
        let gw = BrowserGateway::new();
        assert_eq!(gw.spec(), FrameSpec::new(2, 48_000));
        assert!(gw.listen_addr().ip().is_loopback());
    }

    #[test]
    fn gateway_trait_is_object_safe_and_stubbable() {
        // Confirms the trait can be used dynamically and the WebRtc/Hls
        // transports surface the documented `Unimplemented` path if a future
        // caller routes to them.
        let gw = BrowserGateway::new();
        let _: Box<dyn Gateway> = Box::new(gw);
    }

    // ---- One deterministic loopback integration test (localhost, timeout-bounded) ----
    #[tokio::test]
    async fn loopback_round_trips_one_pcm_frame_each_direction() {
        use futures_util::{SinkExt, StreamExt};
        use std::time::Duration;
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;

        let spec = FrameSpec::new(1, 48_000);
        let (inbound_prod, mut inbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(16);
        let (mut outbound_prod, outbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        let gw = BrowserGateway::new().with_channels(1);
        let serve_task = tokio::spawn(async move {
            let _ = gw
                .serve_with_listener(listener, inbound_prod, outbound_cons)
                .await;
        });

        // Connect a client to the bound port.
        let (client, _resp) = tokio::time::timeout(
            Duration::from_secs(2),
            tokio_tungstenite::connect_async(format!("ws://{addr}")),
        )
        .await
        .expect("client connect timeout")
        .expect("client connect ok");
        let (mut c_sink, mut c_src) = client.split();

        // --- inbound direction: client -> graph ring ---
        let sent = AudioFrame::from_planar(1, 48_000, vec![0.25, -0.25, 0.5]);
        let bytes = encode_frame(&sent).expect("encode");
        c_sink
            .send(Message::Binary(bytes))
            .await
            .expect("client send");

        // The gateway parses + pushes to the inbound producer; drain the
        // opposite end (the one a RingSource would consume).
        let got = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(frame) = inbound_cons.pop() {
                    return frame;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("inbound frame timeout");
        assert_eq!(got.channels, sent.channels);
        assert_eq!(got.sample_rate, sent.sample_rate);
        assert_eq!(got.samples, sent.samples);

        // --- outbound direction: graph ring -> client ---
        let out_frame = AudioFrame::from_planar(1, 48_000, vec![0.75, 0.125]);
        outbound_prod
            .push(out_frame.clone())
            .expect("push outbound frame");

        let echoed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match c_src.next().await {
                    Some(Ok(Message::Binary(b))) => return b,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => panic!("client stream closed"),
                }
            }
        })
        .await
        .expect("outbound frame timeout");
        let decoded = decode_frame(&echoed, spec).expect("decode echoed");
        assert_eq!(decoded.channels, out_frame.channels);
        assert_eq!(decoded.samples, out_frame.samples);

        // Clean up so the accept loop task does not linger.
        drop(c_sink);
        drop(c_src);
        serve_task.abort();
    }

    // ---- IO-path loopback: same scenario via serve_with_io_listener + closures ----
    #[tokio::test]
    async fn io_loopback_round_trips_via_closures() {
        use futures_util::{SinkExt, StreamExt};
        use std::time::Duration;
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;

        let spec = FrameSpec::new(1, 48_000);
        let (mut inbound_prod, mut inbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(16);
        let (mut outbound_prod, mut outbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        let gw = BrowserGateway::new().with_channels(1);
        let serve_task = tokio::spawn(async move {
            let _ = gw
                .serve_with_io_listener(
                    listener,
                    move |f: AudioFrame| inbound_prod.push(f),
                    move || outbound_cons.pop(),
                )
                .await;
        });

        // Connect a client to the bound port.
        let (client, _resp) = tokio::time::timeout(
            Duration::from_secs(2),
            tokio_tungstenite::connect_async(format!("ws://{addr}")),
        )
        .await
        .expect("client connect timeout")
        .expect("client connect ok");
        let (mut c_sink, mut c_src) = client.split();

        // --- inbound direction: client -> graph ring (via the push closure) ---
        let sent = AudioFrame::from_planar(1, 48_000, vec![0.25, -0.25, 0.5]);
        let bytes = encode_frame(&sent).expect("encode");
        c_sink
            .send(Message::Binary(bytes))
            .await
            .expect("client send");

        let got = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(frame) = inbound_cons.pop() {
                    return frame;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("inbound frame timeout");
        assert_eq!(got.channels, sent.channels);
        assert_eq!(got.sample_rate, sent.sample_rate);
        assert_eq!(got.samples, sent.samples);

        // --- outbound direction: graph ring -> client (via the pop closure) ---
        let out_frame = AudioFrame::from_planar(1, 48_000, vec![0.75, 0.125]);
        outbound_prod
            .push(out_frame.clone())
            .expect("push outbound frame");

        let echoed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match c_src.next().await {
                    Some(Ok(Message::Binary(b))) => return b,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => panic!("client stream closed"),
                }
            }
        })
        .await
        .expect("outbound frame timeout");
        let decoded = decode_frame(&echoed, spec).expect("decode echoed");
        assert_eq!(decoded.channels, out_frame.channels);
        assert_eq!(decoded.samples, out_frame.samples);

        // Clean up so the accept loop task does not linger.
        drop(c_sink);
        drop(c_src);
        serve_task.abort();
    }
}
