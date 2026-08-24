//! M11 — ALSA gateway.
//!
//! **P2 module** (ROADMAP §P2). This crate ships a pure-Rust domain model for
//! ALSA PCM hardware parameters — an [`AlsaFormat`] subset of
//! `snd_pcm_format_t` ([`format`]) plus a deterministic `hw_params`
//! [`negotiate`]ion ([`params`]) — and an [`AlsaGateway`] that wires into the
//! audio graph using `audio-graph-bsd`'s [`RingSource`] / [`RingSink`]
//! (the same pattern as the PulseAudio gateway, M10, and the browser gateway,
//! M12).
//!
//! The **live libasound `.so` PCM plugin** (`snd_pcm_t` open/read/write via
//! `extern "C"` FFI) is intentionally deferred behind a default-off
//! [`alsa`](#features) feature — no `libasound` / `alsa-sys` dependency is
//! pulled in at this layer, so the crate builds and tests cleanly on the Linux
//! dev host with no `libasound2-dev` installed. [`Gateway::run`] therefore
//! wires the graph via [`AlsaGateway::register`] and then returns
//! [`GatewayError::Unimplemented`]; the future `alsa` feature will add the
//! serve loop that drives a real `snd_pcm_t` device.
//!
//! # Features
//!
//! - `alsa` *(default off)* — reserved for the libasound `.so` C-ABI PCM
//!   plugin (see [`plugin`]). Not yet implemented.
//!
//! [`RingSource`]: audio_graph_bsd::RingSource
//! [`RingSink`]: audio_graph_bsd::RingSink

use std::net::SocketAddr;

use audio_core_bsd::AudioFrame;
use audio_graph_bsd::{RingSink, RingSource};

pub mod format;
pub mod params;

// `pub use` also brings these names into scope for the signatures below.
pub use audio_graph_bsd::{Graph, NodeId};
pub use format::{to_core_sample_format, AlsaFormat};
pub use params::{negotiate, HwConstraints, HwParams, NegotiateError};

/// Reserved for the libasound `.so` C-ABI PCM plugin (`snd_pcm_*`).
///
/// **Deferred.** When the `alsa` feature lands, this module will hold the
/// `extern "C"` declarations against `libasound.so` (via `dlopen`/`dlsym` or a
/// generated `alsa-sys` binding) plus a `SonicbrewPcm` handle wrapping a real
/// `snd_pcm_t`. Today it is an empty placeholder so the public surface is
/// stable when the feature is wired up; with the default-off feature it links
/// nothing.
#[cfg(feature = "alsa")]
pub mod plugin {
    //! libasound `.so` PCM plugin — deferred (no `extern "C"` yet).
}

/// Worker-thread ring capacity (in frames). Matches the PulseAudio gateway:
/// large enough to absorb jitter without dropping, small enough to keep
/// latency low.
const RING_CAPACITY: usize = 128;

/// Errors returned by the ALSA gateway.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// Operation has not been implemented yet (stub).
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
    /// ALSA interaction failure.
    #[error("alsa: {0}")]
    Alsa(String),
}

/// Convenience `Result` alias.
pub type Result<T> = std::result::Result<T, GatewayError>;

/// The producer end of the inbound ring: a worker pushes frames captured
/// from an ALSA capture device here, and the graph-side [`RingSource`]
/// drains them.
///
/// [`RingSource`]: audio_graph_bsd::RingSource
pub type InboundHandle = rtrb::Producer<AudioFrame>;

/// The consumer end of the outbound ring: the graph-side [`RingSink`]
/// (flushed by the audio engine) pushes graph output here, and a worker pops
/// frames to write to an ALSA playback device.
///
/// [`RingSink`]: audio_graph_bsd::RingSink
pub type OutboundHandle = rtrb::Consumer<AudioFrame>;

/// A gateway connects an ALSA device to the audio graph.
pub trait Gateway: Send {
    /// Drive the gateway against the given graph.
    fn run(&mut self, graph: &mut Graph) -> Result<()>;
}

/// ALSA(L) PCM-plugin gateway — config struct + builder, mirroring
/// [`gw_pulse::PulseGateway`] / the browser gateway.
///
/// Holds only configuration; all runtime state lives in the `rtrb` rings
/// handed back by [`register`](Self::register). The live libasound serve loop
/// is deferred (see the [crate docs](self)).
///
/// [`gw_pulse::PulseGateway`]: ../../gw_pulse/struct.PulseGateway.html
#[derive(Debug, Clone, Copy)]
pub struct AlsaGateway {
    listen_addr: SocketAddr,
    channels: u16,
    sample_rate: u32,
    num_frames: usize,
}

impl AlsaGateway {
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

    /// Sets the listen address (builder).
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
    /// handles — exactly the PulseAudio-gateway M10 pattern.
    ///
    /// The caller drives the graph: `Graph::process_cycle` lets the
    /// [`RingSource`] drain inbound frames, and `RingSink::flush` (called by
    /// the audio engine between cycles) ships stashed output into the
    /// outbound ring for a worker to forward to the ALSA device.
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

impl Default for AlsaGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Gateway for AlsaGateway {
    fn run(&mut self, graph: &mut Graph) -> Result<()> {
        // Wire the graph (useful by itself and exercises the ring nodes), then
        // bail with the documented P2 stub. The async serve loop that drives a
        // real ALSA `snd_pcm_t` device is deferred behind the default-off
        // `alsa` feature; the dropped ring handles are intentionally
        // underscore-prefixed (they own Arc state with a non-trivial Drop).
        let (_src, _sink, _inbound, _outbound) = self.register(graph)?;
        Err(GatewayError::Unimplemented(
            "live libasound .so plugin deferred",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_wires_two_nodes() {
        let mut graph = Graph::new();
        let before = graph.node_count();
        let gw = AlsaGateway::new();
        let (src, sink, _inbound, _outbound) = gw.register(&mut graph).expect("register ok");

        assert_eq!(graph.node_count(), before + 2);
        // Port directions/channels/formats must be compatible: linking the
        // source's single output to the sink's single input must succeed.
        assert!(graph.link((src, 0), (sink, 0)).is_ok());
    }

    #[test]
    fn builders_override_defaults() {
        let gw = AlsaGateway::new()
            .with_channels(1)
            .with_sample_rate(44_100)
            .with_num_frames(64);
        assert_eq!(gw.channels(), 1);
        assert_eq!(gw.sample_rate(), 44_100);
        assert_eq!(gw.num_frames(), 64);
    }

    #[test]
    fn default_is_stereo_48k_loopback() {
        let gw = AlsaGateway::new();
        assert_eq!(gw.channels(), 2);
        assert_eq!(gw.sample_rate(), 48_000);
        assert_eq!(gw.num_frames(), 256);
        assert!(gw.listen_addr().ip().is_loopback());
    }

    #[test]
    fn gateway_trait_is_object_safe() {
        // Confirms the trait can be used dynamically (a real serve loop will
        // eventually sit behind `dyn Gateway`).
        let gw = AlsaGateway::new();
        let _: Box<dyn Gateway> = Box::new(gw);
    }

    #[test]
    fn run_wires_graph_then_returns_unimplemented() {
        let mut graph = Graph::new();
        let before = graph.node_count();
        let mut gw = AlsaGateway::new();
        let err = gw.run(&mut graph).expect_err("P2 run is a stub");
        assert!(matches!(err, GatewayError::Unimplemented(_)));
        // register still ran as a side effect.
        assert_eq!(graph.node_count(), before + 2);
    }
}
