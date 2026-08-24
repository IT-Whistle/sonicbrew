//! M09 — Networked audio transport (RTP / AES67).
//!
//! **P1 module**: RFC 3550 RTP packet codec, L16/L24 PCM framing, and a
//! portable [`transport::UdpTransport`] (std `UdpSocket`, multicast-capable).
//! The zero-copy **netmap** path is FreeBSD-runtime-only and feature-gated
//! (`--features netmap`): [`transport::netmap_layout`] re-declares the kernel
//! ABI (always compiled, layout-tested) and
//! [`transport::netmap_backend`] implements raw ring I/O — RTP framing over
//! the rings is still deferred.
//!
//! This crate is **library-only**: it does not wire itself into the sonicbrew
//! binary RT loop. A future task wires `RtpSource`/`RtpSink` graph nodes into a
//! running engine, exactly like the gw-pulse / gw-browser gateways (see
//! [`register_rtp_nodes`]).
//!
//! # Scope (strict)
//!
//! **In:** RTP fixed-header codec, L16/L24 framing, UDP transport, netmap
//! raw ring I/O (feature `netmap`), ring-backed graph nodes. **Out
//! (deferred):** SAP/SDP, FEC, SRTP/DTLS, PTP/M16 alignment, RTP framing
//! over netmap rings.
//!
//! [`transport::UdpTransport`]: crate::transport::UdpTransport
//! [`transport::netmap_layout`]: crate::transport::netmap_layout
//! [`transport::netmap_backend`]: crate::transport::netmap_backend

pub mod codec;
pub mod jitter;
// netmap zero-copy core (NIOCREGIF + mmap + single-ring I/O): Unix-only
// source availability is not assumed — it compiles on Linux too and fails
// at runtime only where /dev/netmap is absent. No cargo feature, no libc
// dependency (unlike transport::netmap_backend).
#[cfg(unix)]
pub mod transport;
pub mod worker;

// Core types re-exported at the crate root for ergonomics.
pub use audio_core_bsd::AudioFrame;
pub use codec::{
    codec_to_payload_type, decode_l16, decode_l24, encode_l16, encode_l24, timestamp_advance,
    RtpHeader, RtpPacket, RTP_HEADER_LEN, RTP_VERSION,
};
pub use jitter::{forward_distance, JitterBuffer, PushOutcome};
pub use transport::UdpTransport;
pub use worker::{drain_jitter, spawn_rtp_recv_loop, spawn_rtp_send_loop};

use audio_graph_bsd::{Graph, NodeId};

// ---------------------------------------------------------------------------
// Stable transport trait contract (unchanged from the P1 stub)
// ---------------------------------------------------------------------------

/// Payload codec carried over RTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Linear 16-bit PCM (AES67 default).
    PcmL16,
    /// Linear 24-bit PCM.
    PcmL24,
    /// Opus (requires the `opus` feature on the codec gateway).
    Opus,
    /// User-defined RTP payload type (0–127).
    Custom(u8),
}

/// Errors returned by the network transport.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Operation has not been implemented yet (stub).
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
    /// Network I/O failure.
    #[error("network: {0}")]
    Network(String),
}

/// Convenience `Result` alias.
pub type Result<T> = std::result::Result<T, TransportError>;

/// Send/receive audio over a networked RTP/AES67 transport.
pub trait NetworkAudioTransport: Send {
    /// Send a single RTP packet with the given payload and codec.
    fn send_rtp(&mut self, payload: &[u8], codec: Codec) -> Result<()>;
    /// Receive the next RTP packet.
    fn recv_rtp(&mut self) -> Result<Vec<u8>>;
    /// Join an AES67 multicast group.
    fn join_multicast(&mut self, group: std::net::Ipv4Addr) -> Result<()>;
}

/// Stub transport. Real UDP/RTP wiring lands in P1.
pub struct StubTransport;

impl StubTransport {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkAudioTransport for StubTransport {
    fn send_rtp(&mut self, _payload: &[u8], _codec: Codec) -> Result<()> {
        Err(TransportError::Unimplemented("P1 module"))
    }

    fn recv_rtp(&mut self) -> Result<Vec<u8>> {
        Err(TransportError::Unimplemented("P1 module"))
    }

    fn join_multicast(&mut self, _group: std::net::Ipv4Addr) -> Result<()> {
        Err(TransportError::Unimplemented("P1 module"))
    }
}

// ---------------------------------------------------------------------------
// Graph integration (mirrors gw-pulse / gw-browser register())
// ---------------------------------------------------------------------------

/// Worker-thread ring capacity (in `AudioFrame`s). Large enough to absorb
/// network jitter without dropping, small enough to keep latency bounded —
/// the same value used by the browser and PulseAudio gateways.
const RING_CAPACITY: usize = 128;

/// The producer end of the inbound ring: an RTP receive worker pushes decoded
/// `AudioFrame`s here, and the graph-side [`audio_graph_bsd::RingSource`]
/// drains them on the RT thread.
pub type InboundHandle = rtrb::Producer<AudioFrame>;

/// The consumer end of the outbound ring: the graph-side
/// [`audio_graph_bsd::RingSink`] (flushed by the audio engine between cycles)
/// pushes graph output here, and an RTP send worker pops frames to ship them
/// out on the wire.
pub type OutboundHandle = rtrb::Consumer<AudioFrame>;

/// Wires a [`RingSource`](audio_graph_bsd::RingSource) (RTP→graph inbound) and
/// a [`RingSink`](audio_graph_bsd::RingSink) (graph→RTP outbound) into `graph`
/// and returns the node ids plus the two worker-thread ring handles.
///
/// This mirrors the gw-pulse / gw-browser `register()` pattern. The actual RTP
/// socket loop (driving [`UdpTransport`] on a worker thread, **never** the RT
/// thread) is wired by a future binary-integration task; this function only
/// registers the ring-backed graph nodes.
///
/// # Parameters
///
/// - `channels`, `sample_rate`, `num_frames`: describe the port format the
///   ring nodes expose to the graph (must match the rest of the topology).
#[must_use]
pub fn register_rtp_nodes(
    graph: &mut Graph,
    channels: u16,
    sample_rate: u32,
    num_frames: usize,
) -> (NodeId, NodeId, InboundHandle, OutboundHandle) {
    let (inbound_prod, inbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(RING_CAPACITY);
    let (outbound_prod, outbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(RING_CAPACITY);

    let src = graph.add_node(Box::new(audio_graph_bsd::RingSource::new(
        inbound_cons,
        channels,
        sample_rate,
        num_frames,
    )));
    let sink = graph.add_sink(Box::new(audio_graph_bsd::RingSink::new(
        outbound_prod,
        channels,
        sample_rate,
        num_frames,
    )));

    (src, sink, inbound_prod, outbound_cons)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_rtp_nodes_wires_two_nodes() {
        let mut graph = Graph::new();
        let before = graph.node_count();
        let (src, sink, _inbound, _outbound) = register_rtp_nodes(&mut graph, 2, 48_000, 256);

        assert_eq!(graph.node_count(), before + 2);
        // Port format must be compatible: linking the source's single output to
        // the sink's single input must succeed.
        assert!(graph.link((src, 0), (sink, 0)).is_ok());
    }

    #[test]
    fn stub_transport_is_still_unimplemented() {
        // Guard the inherited stub contract: it stays a documented no-op even
        // now that UdpTransport exists.
        let mut t = StubTransport::new();
        assert!(t.send_rtp(&[], Codec::PcmL16).is_err());
        assert!(t.recv_rtp().is_err());
        assert!(t.join_multicast("224.0.0.1".parse().unwrap()).is_err());
    }
}
