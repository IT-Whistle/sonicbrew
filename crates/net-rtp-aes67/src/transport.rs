//! Network transports for [`NetworkAudioTransport`].
//!
//! [`UdpTransport`] is the portable `std::net::UdpSocket` implementation that
//! builds and tests on Linux (and any Unix). The zero-copy **netmap** path
//! is FreeBSD-runtime-only and lives behind the `netmap` feature:
//! [`netmap_layout`] re-declares the kernel ABI (always compiled, layout
//! snapshot-tested) and [`netmap_backend`] implements the raw ring I/O
//! (`NIOCREGIF` + `mmap`, `send_raw`/`recv_raw` + sync ioctls). RTP framing
//! over the rings is deferred — the trait impl on the netmap transport
//! stays `Unimplemented` (see the backend module docs).

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    codec_to_payload_type, timestamp_advance, Codec, NetworkAudioTransport, Result, RtpPacket,
    TransportError,
};

/// Default per-socket receive timeout so [`UdpTransport::recv_rtp`] never blocks
/// forever in a test or a worker that should poll.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Standard-socket RTP transport over `std::net::UdpSocket`.
///
/// Portable: works on Linux for tests and on the FreeBSD runtime. The
/// zero-copy netmap path is separate (see [`netmap_backend`]).
///
/// The transport owns an RTP send clock: each [`NetworkAudioTransport::send_rtp`]
/// call increments the sequence number by one and advances the timestamp by the
/// per-channel frame count derived from the payload length and codec.
pub struct UdpTransport {
    sock: std::net::UdpSocket,
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    channels: u16,
}

impl UdpTransport {
    /// Binds a UDP socket to `local` and returns a transport with a random-ish
    /// SSRC derived from wall-clock nanoseconds (no `rand` dependency) and a
    /// stereo (2-channel) default.
    pub fn bind(local: SocketAddr) -> Result<Self> {
        let sock = std::net::UdpSocket::bind(local).map_err(io_to_network)?;
        // A short read timeout means accidental callers poll instead of
        // blocking; tests rely on it. Setters below can extend it.
        sock.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))
            .map_err(io_to_network)?;
        Ok(Self {
            sock,
            seq: 0,
            timestamp: 0,
            ssrc: system_ssrc(),
            channels: 2,
        })
    }

    /// Builder: override the stereo default used for payload-type selection.
    #[must_use]
    pub fn with_channels(mut self, channels: u16) -> Self {
        self.channels = channels;
        self
    }

    /// Builder: override the receive timeout.
    #[must_use]
    pub fn with_read_timeout(self, timeout: Duration) -> Self {
        let _ = self.sock.set_read_timeout(Some(timeout));
        self
    }

    /// Connects the socket to a single peer so [`NetworkAudioTransport::send_rtp`]
    /// and [`NetworkAudioTransport::recv_rtp`] target only that address.
    pub fn set_peer(&self, peer: SocketAddr) -> Result<()> {
        self.sock.connect(peer).map_err(io_to_network)
    }

    /// The locally-bound address (useful for loopback wiring after `bind(:0)`).
    #[must_use]
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.sock.local_addr().ok()
    }

    /// The SSRC in use.
    #[must_use]
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Receive one RTP packet, returning `(sequence_number, payload_bytes)`.
    ///
    /// Unlike the [`NetworkAudioTransport::recv_rtp`] trait method (which
    /// returns payload only), this retains the seq so the receive path can
    /// feed a [`crate::JitterBuffer`] for reorder.
    pub fn recv_rtp_with_seq(&mut self) -> Result<(u16, Vec<u8>)> {
        let mut buf = [0u8; 65_535];
        let n = self.sock.recv(&mut buf).map_err(io_to_network)?;
        let packet = RtpPacket::parse(&buf[..n])?;
        Ok((packet.header.seq, packet.payload))
    }
}

impl NetworkAudioTransport for UdpTransport {
    fn send_rtp(&mut self, payload: &[u8], codec: Codec) -> Result<()> {
        let payload_type = codec_to_payload_type(codec, self.channels);
        let header = crate::RtpHeader::new(payload_type, self.seq, self.timestamp, self.ssrc);
        // Advance the RTP clock for the *next* packet by the per-channel frame
        // count this payload represents.
        self.seq = self.seq.wrapping_add(1);
        self.timestamp =
            self.timestamp
                .wrapping_add(timestamp_advance(payload.len(), codec, self.channels));

        let packet = RtpPacket {
            header,
            payload: payload.to_vec(),
        };
        let bytes = packet.encode();
        self.sock.send(&bytes).map_err(io_to_network)?;
        Ok(())
    }

    fn recv_rtp(&mut self) -> Result<Vec<u8>> {
        let mut buf = [0u8; 65_535];
        let n = self.sock.recv(&mut buf).map_err(io_to_network)?;
        let packet = RtpPacket::parse(&buf[..n])?;
        Ok(packet.payload)
    }

    fn join_multicast(&mut self, group: Ipv4Addr) -> Result<()> {
        self.sock
            .join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)
            .map_err(io_to_network)
    }
}

/// Maps a `std::io::Error` to [`TransportError::Network`].
fn io_to_network(err: std::io::Error) -> TransportError {
    TransportError::Network(err.to_string())
}

/// Derives a best-effort random SSRC from wall-clock nanoseconds. The value is
/// only used to label the stream; uniqueness is not cryptographically required.
fn system_ssrc() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 as u32)
        .unwrap_or(0x0C0F_FEEE)
}

// ---------------------------------------------------------------------------
// netmap zero-copy backend (FreeBSD runtime, feature-gated)
// ---------------------------------------------------------------------------

/// FreeBSD netmap(4) ABI re-declaration — always compiled, dependency-free,
/// and layout snapshot-tested in the default suite. Shared by
/// [`netmap_backend`] and the `netmap_probe` example.
pub mod netmap_layout;

/// Zero-copy netmap ring I/O — FreeBSD runtime only, gated behind the
/// `netmap` feature. The module compiles on every Unix (Linux CI host
/// included) and pulls in `libc` only as an optional dependency. Raw ring
/// operations are real; RTP framing over the rings is the next step, so the
/// [`NetworkAudioTransport`] impl still returns
/// [`TransportError::Unimplemented`].
#[cfg(all(unix, feature = "netmap"))]
pub mod netmap_backend;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_transport_loopback() {
        // Two ephemeral loopback sockets, pointed at each other. Unicast only —
        // no multicast, so this runs reliably in CI/sandboxes.
        let a = UdpTransport::bind("127.0.0.1:0".parse().unwrap()).expect("bind A");
        let b = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
            .expect("bind B")
            .with_read_timeout(Duration::from_millis(1000));

        let a_local = a.local_addr().expect("A local addr");
        let b_local = b.local_addr().expect("B local addr");
        let mut a = a;
        a.set_peer(b_local).expect("A -> B");
        let mut b = b;
        b.set_peer(a_local).expect("B -> A");

        let payload = vec![0x01, 0x02, 0x03, 0x04, 0x05];
        a.send_rtp(&payload, Codec::PcmL16).expect("send");

        let received = b.recv_rtp().expect("recv");
        assert_eq!(received, payload, "payload must survive the RTP round-trip");
    }

    #[test]
    fn udp_transport_increments_seq_and_timestamp() {
        let mut t = UdpTransport::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
        // Send into the void: connect to a discard port on loopback; send is
        // still a best-effort datagram we don't need to receive.
        t.set_peer("127.0.0.1:9".parse().unwrap()) // port 9 = discard
            .expect("connect");
        let payload = vec![0u8; 8]; // 2 stereo L16 frames = 2*2*2 bytes
        t.send_rtp(&payload, Codec::PcmL16).expect("send1");
        assert_eq!(t.seq, 1);
        assert_eq!(t.timestamp, 2); // 8 bytes / (2 bytes * 2 ch) = 2 frames
    }

    #[cfg(feature = "netmap")]
    #[test]
    fn netmap_stub_is_unimplemented() {
        use super::netmap_backend::NetmapTransport;
        let mut t = NetmapTransport::new();
        assert!(t.send_rtp(&[], Codec::PcmL16).is_err());
        assert!(t.recv_rtp().is_err());
        assert!(t.join_multicast("224.0.0.1".parse().unwrap()).is_err());
    }
}
