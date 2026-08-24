//! Live netmap ring-I/O check (FreeBSD runtime; requires `--features
//! netmap`). Opens a VALE port, sends RTP-sized packets into the TX ring,
//! syncs, and reports slot accounting — the runtime proof of the
//! `transport::netmap_backend` registration + ring walk.

#[cfg(all(unix, feature = "netmap"))]
fn main() -> std::process::ExitCode {
    use net_rtp_aes67::transport::netmap_backend::NetmapTransport;

    match NetmapTransport::open("vale1:1") {
        Ok(mut p) => {
            println!(
                "open OK: tx_num_slots={} tx_buf_size={} tx_slots_free={}",
                p.tx_num_slots(),
                p.tx_buf_size(),
                p.tx_ring_slots_free()
            );
            let mut sent = 0;
            let pkt = [0xABu8; 160]; // RTP-sized dummy payload
            for _ in 0..10 {
                match p.send_raw(&pkt) {
                    Ok(n) if n > 0 => sent += 1,
                    Ok(_) => break,
                    Err(e) => {
                        println!("send_raw err: {e}");
                        break;
                    }
                }
            }
            println!("sent {sent} packets into TX ring");
            match p.txsync() {
                Ok(()) => println!("txsync OK"),
                Err(e) => println!("txsync err: {e}"),
            }
            println!("tx_slots_free after sync: {}", p.tx_ring_slots_free());
            if sent == 10 {
                println!("PORT_TEST_PASS");
                std::process::ExitCode::SUCCESS
            } else {
                println!("PORT_TEST_PARTIAL({sent})");
                std::process::ExitCode::FAILURE
            }
        }
        Err(e) => {
            println!("open FAILED: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(all(unix, feature = "netmap")))]
fn main() {
    eprintln!("nm_port_test: Unix + --features netmap required");
}
