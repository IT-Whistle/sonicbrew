//! M10 — PulseAudio gateway.
//!
//! **P1 module** (ROADMAP Phase 3). This crate ships a pure-Rust parser for
//! the subset of the PulseAudio native protocol needed to demonstrate parsing
//! a playback stream's sample spec and a length-prefixed string (see
//! [`codec`]), plus a [`PulseGateway`] that wires into the audio graph using
//! `audio-graph-bsd`'s [`RingSource`] / [`RingSink`] (the same pattern as the
//! browser gateway, M12).
//!
//! The **live daemon path** is [`daemon`] (unix): a pure-Rust, blocking
//! client that connects to the daemon's UNIX socket and performs the
//! native-protocol handshake — `AUTH` with cookie, `SET_CLIENT_NAME`, and a
//! `QUERY_INFO`(server) probe — with tagstruct serialization in [`tags`].
//! **No libpulse linkage by design**: the native protocol is a documented
//! binary format and the parser lives in this crate, which also keeps the
//! LGPL library out of the dependency graph (see docs/KNOWLEDGE §9.4). The
//! async serve loop that moves audio between the daemon and the graph
//! remains future work: [`Gateway::run`] therefore wires the graph via
//! [`PulseGateway::register`] and then returns [`GatewayError::Unimplemented`]
//! for that part.
//!
//! [`RingSource`]: audio_graph_bsd::RingSource
//! [`RingSink`]: audio_graph_bsd::RingSink

use std::net::SocketAddr;

use audio_core_bsd::AudioFrame;
use audio_graph_bsd::{Graph, NodeId, RingSink, RingSource};

pub mod codec;
#[cfg(unix)]
pub mod daemon;
pub mod tags;

pub use codec::{PacketHeader, PulseError, SampleFormat, SampleSpec};
#[cfg(unix)]
pub use daemon::{PulseDaemon, PulseDaemonError, ServerInfo};
pub use tags::{TagReader, TagWriter};

/// Worker-thread ring capacity (in frames). Matches the browser gateway: large
/// enough to absorb jitter without dropping, small enough to keep latency low.
const RING_CAPACITY: usize = 128;

/// Errors returned by the PulseAudio gateway.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// Operation has not been implemented yet (stub).
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
    /// libpulse interaction failure (also wraps codec [`PulseError`]).
    #[error("pulse: {0}")]
    Pulse(String),
}

impl From<PulseError> for GatewayError {
    fn from(err: PulseError) -> Self {
        Self::Pulse(err.to_string())
    }
}

/// Convenience `Result` alias.
pub type Result<T> = std::result::Result<T, GatewayError>;

/// The producer end of the inbound ring: a worker pushes decoded PulseAudio
/// frames here, and the graph-side [`RingSource`] drains them.
///
/// [`RingSource`]: audio_graph_bsd::RingSource
pub type InboundHandle = rtrb::Producer<AudioFrame>;

/// The consumer end of the outbound ring: the graph-side [`RingSink`] (flushed
/// by the audio engine) pushes graph output here, and a worker pops frames to
/// ship to the daemon.
///
/// [`RingSink`]: audio_graph_bsd::RingSink
pub type OutboundHandle = rtrb::Consumer<AudioFrame>;

/// A gateway connects a PulseAudio daemon to the audio graph.
pub trait Gateway: Send {
    /// Drive the gateway against the given graph.
    fn run(&mut self, graph: &mut Graph) -> Result<()>;
}

/// PulseAudio gateway configuration.
///
/// Holds only configuration; all runtime state lives in the `rtrb` rings
/// handed back by [`register`](Self::register). The live daemon serve loop is
/// deferred (see the [crate docs](self)).
#[derive(Debug, Clone, Copy)]
pub struct PulseGateway {
    listen_addr: SocketAddr,
    channels: u16,
    sample_rate: u32,
    num_frames: usize,
}

impl PulseGateway {
    /// Creates a gateway with MVP defaults: loopback listen address, stereo,
    /// 48 kHz, 256-sample blocks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            listen_addr: "127.0.0.1:0"
                .parse()
                .expect("hard-coded loopback literal always parses"),
            channels: 2,
            sample_rate: 48_000,
            num_frames: 256,
        }
    }

    /// Sets the daemon listen address (builder).
    #[must_use]
    pub fn with_listen_addr(mut self, addr: SocketAddr) -> Self {
        self.listen_addr = addr;
        self
    }

    /// Sets the channel count (builder).
    #[must_use]
    pub fn with_channels(mut self, channels: u16) -> Self {
        self.channels = channels;
        self
    }

    /// Sets the sample rate, in Hz (builder).
    #[must_use]
    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = sample_rate;
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

    /// The configured channel count.
    #[must_use]
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// The configured sample rate, in Hz.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The configured per-block frame count.
    #[must_use]
    pub fn num_frames(&self) -> usize {
        self.num_frames
    }

    /// Wires a [`RingSource`] (inbound) and a [`RingSink`] (outbound) into
    /// `graph` and returns the node ids plus the two worker-thread ring
    /// handles — exactly the browser-gateway M12 pattern.
    ///
    /// The caller drives the graph: `Graph::process_cycle` lets the
    /// [`RingSource`] drain inbound frames, and `RingSink::flush` (called by
    /// the audio engine between cycles) ships stashed output into the
    /// outbound ring for a worker to forward to the daemon.
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
            self.channels,
            self.sample_rate,
            self.num_frames,
        )));
        let sink = graph.add_sink(Box::new(RingSink::new(
            outbound_prod,
            self.channels,
            self.sample_rate,
            self.num_frames,
        )));

        Ok((src, sink, inbound_prod, outbound_cons))
    }
}

impl Default for PulseGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Gateway for PulseGateway {
    fn run(&mut self, graph: &mut Graph) -> Result<()> {
        // Wire the graph (useful by itself and exercises the ring nodes), then
        // bail with the documented P1 stub. The async serve loop that talks to
        // a real PulseAudio daemon is deferred behind a future default-off
        // libpulse feature; this crate has no such feature today. The dropped
        // ring handles are intentionally underscore-prefixed (they own Arc
        // state with a non-trivial Drop).
        let (_src, _sink, _inbound, _outbound) = self.register(graph)?;
        Err(GatewayError::Unimplemented(
            "async daemon serve loop deferred (handshake: see `daemon`)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_adds_linkable_source_and_sink_nodes() {
        let mut graph = Graph::new();
        let before = graph.node_count();
        let gw = PulseGateway::new();
        let (src, sink, _inbound, _outbound) = gw.register(&mut graph).expect("register ok");

        assert_eq!(graph.node_count(), before + 2);
        // Port directions/channels/formats must be compatible: linking the
        // source's single output to the sink's single input must succeed.
        assert!(graph.link((src, 0), (sink, 0)).is_ok());
    }

    #[test]
    fn builders_override_defaults() {
        let gw = PulseGateway::new()
            .with_channels(1)
            .with_sample_rate(44_100)
            .with_num_frames(64);
        assert_eq!(gw.channels(), 1);
        assert_eq!(gw.sample_rate(), 44_100);
        assert_eq!(gw.num_frames(), 64);
    }

    #[test]
    fn default_is_stereo_48k_loopback() {
        let gw = PulseGateway::new();
        assert_eq!(gw.channels(), 2);
        assert_eq!(gw.sample_rate(), 48_000);
        assert_eq!(gw.num_frames(), 256);
        assert!(gw.listen_addr().ip().is_loopback());
    }

    #[test]
    fn gateway_trait_is_object_safe() {
        // Confirms the trait can be used dynamically (a real serve loop will
        // eventually sit behind `dyn Gateway`).
        let gw = PulseGateway::new();
        let _: Box<dyn Gateway> = Box::new(gw);
    }

    #[test]
    fn run_wires_graph_then_returns_unimplemented() {
        let mut graph = Graph::new();
        let before = graph.node_count();
        let mut gw = PulseGateway::new();
        let err = gw.run(&mut graph).expect_err("P1 run is a stub");
        assert!(matches!(err, GatewayError::Unimplemented(_)));
        // register still ran as a side effect.
        assert_eq!(graph.node_count(), before + 2);
    }
}
