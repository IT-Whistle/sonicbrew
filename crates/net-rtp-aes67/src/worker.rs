//! Worker-thread socket loops that bridge a [`crate::UdpTransport`] and the
//! ring-buffer graph handles from [`crate::register_rtp_nodes`].
//!
//! These loops run on **plain `std::thread`s** — this crate deliberately has
//! **no tokio dependency** (verify with `cargo tree -p net-rtp-aes67`). The
//! pattern mirrors the existing gateway `spawn_bt_to_ring` workers:
//!
//! - named threads (visible in a debugger / `htop`),
//! - **drop-on-full** pushes (a full inbound ring means the graph underran — a
//!   dropped frame is the correct back-pressure response, never a block),
//! - an **idle backoff sleep** so a timeout-only socket never pins a core.
//!
//! # Shutdown
//!
//! - [`spawn_rtp_recv_loop`] exits when the inbound consumer is dropped.
//!   Because `recv_rtp` only returns a payload to push *after* a successful
//!   read, and rtrb 0.3's `PushError` has no `Disconnected` variant, the loop
//!   polls [`rtrb::Producer::is_abandoned`] each idle cycle for prompt,
//!   deterministic termination.
//! - [`spawn_rtp_send_loop`] exits when the outbound producer is dropped,
//!   detected via [`rtrb::Consumer::is_abandoned`] each cycle (rtrb 0.3's
//!   `PopError` has only `Empty`, so `pop` never blocks).

use std::thread::JoinHandle;
use std::time::Duration;

use crate::{
    decode_l16, encode_l16, AudioFrame, Codec, InboundHandle, JitterBuffer, NetworkAudioTransport,
    OutboundHandle, UdpTransport,
};

/// Sleep applied on an idle recv timeout / transient error, or an empty
/// outbound ring, so a worker never hot-spins.
const IDLE_BACKOFF: Duration = Duration::from_millis(5);

/// Drain in-order payloads from `jitter`, decoding each to an [`AudioFrame`].
///
/// Drains the contiguous in-order run via `pop`; if a gap remains with packets
/// buffered ahead, `skip_gap` declares the missing seq(s) lost and draining
/// resumes. A no-progress guard breaks the loop so it can't spin. Minimal-latency
/// policy: a gap is skipped as soon as a later packet is buffered (acceptable
/// loss concealment for low-latency audio; a time-based playout delay is future).
pub fn drain_jitter(
    jitter: &mut JitterBuffer<Vec<u8>>,
    channels: u16,
    sample_rate: u32,
) -> Vec<AudioFrame> {
    let mut frames = Vec::new();
    loop {
        // Drain the contiguous in-order run.
        while let Some((_seq, payload)) = jitter.pop() {
            let samples = decode_l16(&payload, channels);
            frames.push(AudioFrame::from_planar(channels, sample_rate, samples));
        }
        // Run ended: gap (packets buffered ahead) or empty.
        if jitter.is_empty() {
            break;
        }
        // Skip ONE gap (loss), then loop to drain the next run. Guard: if
        // skip_gap made no progress, stop to avoid an infinite loop.
        if jitter.skip_gap() == 0 {
            break;
        }
    }
    frames
}

/// Spawn a worker thread that drains `transport` (RTP receive) and pushes
/// decoded L16 [`AudioFrame`]s into `inbound`.
///
/// Each received packet (seq + payload, via [`UdpTransport::recv_rtp_with_seq`])
/// is pushed into a [`JitterBuffer`] and drained in-order by [`drain_jitter`],
/// which decodes each contiguous run with [`decode_l16`]. A recv timeout /
/// transient I/O error is treated as **idle** (keep looping) — never fatal.
///
/// Runs until `inbound` is abandoned (its consumer dropped), detected by
/// [`rtrb::Producer::is_abandoned`] on an idle cycle. (rtrb 0.3's `PushError`
/// has no `Disconnected` variant, so abandonment is the sole shutdown signal.)
///
/// # Drop-on-full
///
/// If the inbound ring is full mid-drain, the current frame is **dropped** and
/// the drain pauses (remaining buffered frames wait for the next recv cycle);
/// this is the intended back-pressure behaviour — the worker never blocks.
#[must_use]
pub fn spawn_rtp_recv_loop(
    mut transport: UdpTransport,
    mut inbound: InboundHandle,
    channels: u16,
    sample_rate: u32,
) -> JoinHandle<()> {
    let mut jitter = JitterBuffer::<Vec<u8>>::new(64);
    std::thread::Builder::new()
        .name("sonicbrew-rtp-recv".into())
        .spawn(move || loop {
            // Shutdown: if the graph side (consumer) is gone, don't keep the
            // socket thread alive. rtrb 0.3's PushError has no Disconnected
            // variant, so is_abandoned is the sole shutdown signal — checked
            // each cycle so the loop can't outlive the consumer by more than
            // one read-timeout.
            if inbound.is_abandoned() {
                tracing::info!("rtp-recv: inbound abandoned, exiting");
                break;
            }
            match transport.recv_rtp_with_seq() {
                Ok((seq, payload)) => {
                    jitter.push(seq, payload);
                    for frame in drain_jitter(&mut jitter, channels, sample_rate) {
                        match inbound.push(frame) {
                            Ok(()) => {}
                            // rtrb 0.3's PushError has only `Full`; ring-full
                            // means the graph underran — drop this frame and
                            // pause the drain (resume next recv cycle).
                            Err(rtrb::PushError::Full(_)) => {
                                tracing::warn!(
                                    "rtp-recv: inbound ring full, dropped a frame; pausing drain"
                                );
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    // recv timeout / transient I/O — idle, keep looping. The
                    // brief sleep avoids hot-spinning on a timeout-only socket.
                    tracing::debug!(error = %e, "rtp-recv: idle (recv timeout or transient I/O)");
                    std::thread::sleep(IDLE_BACKOFF);
                }
            }
        })
        .expect("spawn sonicbrew-rtp-recv thread")
}

/// Spawn a worker thread that drains `outbound` ([`AudioFrame`]s produced by the
/// graph) and sends them as RTP/L16 via `transport`.
///
/// Idle-sleeps on an empty ring to avoid pinning a core. Runs until `outbound`
/// is abandoned (its producer dropped), detected by [`rtrb::Consumer::is_abandoned`]
/// each cycle. (rtrb 0.3's `PopError` has only `Empty`; `pop` never blocks.)
#[must_use]
pub fn spawn_rtp_send_loop(
    mut transport: UdpTransport,
    mut outbound: OutboundHandle,
    channels: u16,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("sonicbrew-rtp-send".into())
        .spawn(move || loop {
            // Shutdown: rtrb 0.3's PopError has no Disconnected variant, so the
            // consumer detects a dropped producer via is_abandoned each cycle.
            if outbound.is_abandoned() {
                tracing::info!("rtp-send: outbound abandoned, exiting");
                break;
            }
            match outbound.pop() {
                Ok(frame) => {
                    let payload = encode_l16(&frame.samples, channels);
                    if let Err(e) = transport.send_rtp(&payload, Codec::PcmL16) {
                        tracing::warn!(error = %e, "rtp-send: send_rtp failed");
                        std::thread::sleep(IDLE_BACKOFF);
                    }
                }
                Err(rtrb::PopError::Empty) => std::thread::sleep(IDLE_BACKOFF),
            }
        })
        .expect("spawn sonicbrew-rtp-send thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Two loopback `UdpTransport`s pointed at each other (unicast, no
    /// multicast — CI/sandbox-safe).
    fn loopback_pair() -> (UdpTransport, UdpTransport) {
        let a = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
            .expect("bind A")
            .with_channels(1);
        let b = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
            .expect("bind B")
            .with_channels(1);
        let a_local = a.local_addr().expect("A local addr");
        let b_local = b.local_addr().expect("B local addr");
        a.set_peer(b_local).expect("A -> B");
        b.set_peer(a_local).expect("B -> A");
        (a, b)
    }

    #[test]
    fn recv_loop_decodes_and_pushes() {
        let (mut a, b) = loopback_pair();
        // Short read timeout so the recv loop's idle cycle (and thus its
        // shutdown) is prompt and deterministic.
        let b = b.with_read_timeout(Duration::from_millis(100));

        let (inbound_prod, mut inbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(16);
        let (channels, sample_rate) = (1u16, 48_000u32);
        let recv_handle = spawn_rtp_recv_loop(b, inbound_prod, channels, sample_rate);

        // A mono frame of 256 non-zero samples, shipped directly from A.
        let samples: Vec<f32> = (0..256).map(|i| 0.4 + 0.1 * (i as f32) / 256.0).collect();
        let payload = encode_l16(&samples, channels);
        a.send_rtp(&payload, Codec::PcmL16).expect("A sends");

        // Poll the inbound consumer with a bounded total timeout (~2s).
        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = loop {
            match inbound_cons.pop() {
                Ok(f) => break f,
                Err(rtrb::PopError::Empty) => {
                    if Instant::now() >= deadline {
                        panic!("recv loop did not deliver a frame within 2s");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };

        assert_eq!(frame.channels, channels);
        assert_eq!(frame.sample_rate, sample_rate);
        assert_eq!(frame.samples.len(), 256, "256 mono samples round-trip");
        for (i, s) in frame.samples.iter().enumerate() {
            let want = 0.4 + 0.1 * (i as f32) / 256.0;
            assert!(
                (s - want).abs() < 1e-3,
                "sample {i} mismatch: got {s}, want {want}"
            );
        }

        // Shutdown: drop the consumer → recv loop sees the abandoned ring on
        // its next idle cycle (≤ read timeout) and exits; join.
        drop(inbound_cons);
        recv_handle.join().expect("recv loop joined cleanly");
    }

    #[test]
    fn send_loop_drains_and_sends() {
        let (a, b) = loopback_pair();
        let mut b = b.with_read_timeout(Duration::from_millis(100));

        let (mut out_prod, out_cons) = rtrb::RingBuffer::<AudioFrame>::new(16);
        let channels = 1u16;
        let send_handle = spawn_rtp_send_loop(a, out_cons, channels);

        // Produce a mono frame and hand it to the send loop via the ring.
        let samples: Vec<f32> = (0..128).map(|i| 0.3 + 0.05 * (i as f32) / 128.0).collect();
        let frame = AudioFrame::from_planar(channels, 48_000, samples);
        out_prod.push(frame).expect("push to outbound");

        // B should receive the RTP payload produced by the send loop.
        let deadline = Instant::now() + Duration::from_secs(2);
        let payload = loop {
            match b.recv_rtp() {
                Ok(p) => break p,
                Err(_) => {
                    if Instant::now() >= deadline {
                        panic!("send loop did not deliver an RTP packet within 2s");
                    }
                }
            }
        };

        assert!(!payload.is_empty(), "payload must be non-empty");
        let decoded = decode_l16(&payload, channels);
        assert_eq!(decoded.len(), 128, "128 mono samples round-trip");
        for (i, s) in decoded.iter().enumerate() {
            let want = 0.3 + 0.05 * (i as f32) / 128.0;
            assert!(
                (s - want).abs() < 1e-3,
                "decoded sample {i} mismatch: got {s}, want {want}"
            );
        }

        // Shutdown: drop the producer → send loop sees outbound abandoned on
        // its next cycle and exits; join.
        drop(out_prod);
        send_handle.join().expect("send loop joined cleanly");
    }

    #[test]
    fn recv_rtp_with_seq_returns_seq_and_payload() {
        let (mut a, b) = loopback_pair();
        let mut b = b.with_read_timeout(Duration::from_millis(1000));

        // A mono frame shipped directly from A. A's seq starts at 0.
        let samples: Vec<f32> = (0..64).map(|i| 0.2 + 0.01 * (i as f32)).collect();
        let payload = encode_l16(&samples, 1);
        a.send_rtp(&payload, Codec::PcmL16).expect("A sends");

        let (seq, recv_payload) = b.recv_rtp_with_seq().expect("B recv_rtp_with_seq");
        assert_eq!(seq, 0, "first packet seq is 0");

        let decoded = decode_l16(&recv_payload, 1);
        assert_eq!(decoded.len(), samples.len());
        for (got, want) in decoded.iter().zip(samples.iter()) {
            assert!(
                (got - want).abs() < 1e-3,
                "sample mismatch: got {got}, want {want}"
            );
        }
    }

    #[test]
    fn drain_jitter_reorders_out_of_order() {
        let mut j = JitterBuffer::<Vec<u8>>::new(16);
        // Push out of order: 0, 2, 1. Each payload encodes a distinct sample
        // value so we can confirm emission order.
        let p0 = encode_l16(&[0.1; 4], 1);
        let p1 = encode_l16(&[0.2; 4], 1);
        let p2 = encode_l16(&[0.3; 4], 1);
        j.push(0, p0);
        j.push(2, p2);
        j.push(1, p1);

        let frames = drain_jitter(&mut j, 1, 48_000);
        assert_eq!(frames.len(), 3, "all three frames emitted in order");
        // Frame 0 → 0.1, frame 1 → 0.2, frame 2 → 0.3.
        assert!(
            (frames[0].samples[0] - 0.1).abs() < 1e-3,
            "frame 0 is seq 0 (0.1)"
        );
        assert!(
            (frames[1].samples[0] - 0.2).abs() < 1e-3,
            "frame 1 is seq 1 (0.2)"
        );
        assert!(
            (frames[2].samples[0] - 0.3).abs() < 1e-3,
            "frame 2 is seq 2 (0.3)"
        );
        assert!(j.is_empty(), "jitter buffer fully drained");
    }

    #[test]
    fn drain_jitter_skips_loss_gap() {
        let mut j = JitterBuffer::<Vec<u8>>::new(16);
        // Push 0 then 2 (seq 1 is lost). Distinct sample values tag each seq.
        let p0 = encode_l16(&[0.1; 4], 1);
        let p2 = encode_l16(&[0.3; 4], 1);
        j.push(0, p0);
        j.push(2, p2);

        let frames = drain_jitter(&mut j, 1, 48_000);
        assert_eq!(frames.len(), 2, "seq 0 and seq 2 emitted; seq 1 lost");
        assert!(
            (frames[0].samples[0] - 0.1).abs() < 1e-3,
            "frame 0 is seq 0 (0.1)"
        );
        assert!(
            (frames[1].samples[0] - 0.3).abs() < 1e-3,
            "frame 1 is seq 2 (0.3)"
        );
        // The lost seq 1 (which would decode to 0.2) must NOT appear.
        for f in &frames {
            assert!(
                (f.samples[0] - 0.2).abs() >= 1e-3,
                "lost seq 1 (0.2) must not be present"
            );
        }
        assert!(j.is_empty(), "jitter buffer fully drained");
    }
}
