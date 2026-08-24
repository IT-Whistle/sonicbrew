//! VALE switch loopback: RTP in, RTP out, through the kernel bridge.
//!
//! The verified protocol (live-tested via the C matrix programs, 2026-08-18):
//! 1. open the RECEIVER first and read its kernel-assigned **memid**;
//! 2. open the SENDER pinned to that memid (`open_with_memid`) — VALE only
//!    switches between ports sharing a memory allocator;
//! 3. `prime_rx()` on the receiver — a fresh VALE RX ring's slot
//!    descriptors are stale garbage until acknowledged;
//! 4. `send_rtp` on the sender (frames + stages + txsync per packet);
//! 5. poll `recv_rtp` on the receiver (rxsync + slot read + RTP parse).
//!
//! FreeBSD runtime only (`--features netmap`).

#[cfg(all(unix, feature = "netmap"))]
fn main() -> std::process::ExitCode {
    use net_rtp_aes67::transport::netmap_backend::NetmapTransport;
    use net_rtp_aes67::{Codec, NetworkAudioTransport};

    // 1. Receiver first: it creates the switch and owns the memid.
    let mut rx = match NetmapTransport::open("vale60:r") {
        Ok(t) => t,
        Err(e) => {
            println!("open vale60:r FAILED: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    println!("receiver open (vale60:r), memid={}", rx.memid());

    // 3. Drain the stale slot descriptors before any traffic exists.
    if let Err(e) = rx.prime_rx() {
        println!("prime_rx FAILED: {e}");
        return std::process::ExitCode::FAILURE;
    }
    println!("receiver primed (stale slots acknowledged)");

    // 2. Sender pinned to the receiver's memid, its SEND ring primed too
    //    (a fresh VALE port's ofs[0] starts with stale descriptors).
    let mut tx = match NetmapTransport::open_with_memid("vale60:w", Some(rx.memid())) {
        Ok(t) => t,
        Err(e) => {
            println!("open vale60:w FAILED: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    println!(
        "sender open (vale60:w), memid={} tx={:?}",
        tx.memid(),
        tx.tx_debug_state()
    );

    // RTP/L16 stereo payload: 48 frames of a recognizable pattern.
    let frames: usize = 48;
    let mut payload = Vec::with_capacity(frames * 2 * 2);
    for i in 0..frames {
        let v = (i % 256) as u16;
        payload.extend_from_slice(&v.to_le_bytes());
        payload.extend_from_slice(&(v ^ 0xFFFF).to_le_bytes());
    }

    const COUNT: usize = 8;
    for _ in 0..COUNT {
        if let Err(e) = tx.send_rtp(&payload, Codec::PcmL16) {
            println!("send_rtp FAILED: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }
    println!("sent {COUNT} RTP packets through vale60:w");

    // 5. Drain the receiver.
    let mut received = 0_usize;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while received < COUNT && std::time::Instant::now() < deadline {
        match rx.recv_rtp() {
            Ok(p) if p == payload => received += 1,
            Ok(other) => {
                println!(
                    "received {}-byte packet with WRONG payload (expected {})",
                    other.len(),
                    payload.len()
                );
                return std::process::ExitCode::FAILURE;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }

    if received == COUNT {
        println!("received {received}/{COUNT} RTP packets intact via vale60:r");
        println!("LOOPBACK_PASS");
        std::process::ExitCode::SUCCESS
    } else {
        println!("LOOPBACK_TIMEOUT: only {received}/{COUNT} arrived");
        std::process::ExitCode::FAILURE
    }
}

#[cfg(not(all(unix, feature = "netmap")))]
fn main() {
    eprintln!("vale_loopback: Unix + --features netmap required");
}
