//! VALE loopback diagnostic — raw ring level, EXACTLY mirroring the
//! working C reference program's sequence:
//!
//! 1. open TX port (`valeN:b`) — NIOCREGIF + mmap
//! 2. stage ONE packet into TX slot 0 (len, head advance, cur = head)
//! 3. open RX port (`valeN:a`) — registration AFTER staging, like the C
//! 4. NIOCTXSYNC (kernel consumes; TX tail moves)
//! 5. sleep 1 s
//! 6. NIOCRXSYNC → RX tail should have advanced → read the packet
//!
//! Any remaining failure vs the C program isolates a code-level difference
//! (not ordering/timing).

#[cfg(all(unix, feature = "netmap"))]
fn main() -> std::process::ExitCode {
    use net_rtp_aes67::transport::netmap_backend::NetmapTransport;

    // 1. TX port.
    let mut tx = match NetmapTransport::open("vale13:b") {
        Ok(t) => t,
        Err(e) => {
            println!("open vale13:b FAILED: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    println!("tx port open (vale13:b): {:?}", tx.tx_debug_state());

    // 2. Stage one packet BEFORE anything else (C order).
    let pkt = [0x5Au8; 64];
    match tx.send_raw(&pkt) {
        Ok(n) => println!("staged {n} bytes"),
        Err(e) => {
            println!("send_raw FAILED: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }
    println!("after stage: {:?}", tx.tx_debug_state());

    // 3. RX port registered AFTER staging.
    let mut rx = match NetmapTransport::open("vale13:a") {
        Ok(t) => t,
        Err(e) => {
            println!("open vale13:a FAILED: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    println!("rx port open (vale13:a): {:?}", rx.rx_debug_state());

    // 4. TX sync.
    tx.txsync().expect("txsync");
    println!("post-txsync: tx={:?}", tx.tx_debug_state());

    // 5. C sleeps 1 s here.
    std::thread::sleep(std::time::Duration::from_secs(1));

    // 6. RX sync + drain.
    rx.rxsync().expect("rxsync");
    println!("post-rxsync: rx={:?}", rx.rx_debug_state());

    let mut buf = vec![0u8; 2048];
    let mut got = 0;
    // Single drain pass exactly like the C (head != tail loop).
    while got < 1 {
        match rx.recv_raw(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                println!("rx packet: {n} bytes, first={:#x}", buf[0]);
                got += 1;
            }
            Err(e) => {
                println!("recv_raw err: {e}");
                break;
            }
        }
    }

    println!("RESULT: {got}/1 raw packets forwarded");
    if got == 1 {
        println!("RUST_FORWARDED");
        std::process::ExitCode::SUCCESS
    } else {
        println!("RUST_NOT_FORWARDED");
        std::process::ExitCode::FAILURE
    }
}

#[cfg(not(all(unix, feature = "netmap")))]
fn main() {
    eprintln!("vale_diag: Unix + --features netmap required");
}
