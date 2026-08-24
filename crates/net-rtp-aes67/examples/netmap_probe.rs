//! netmap capability probe + TX-ring smoke sender (FreeBSD runtime; compiles
//! nowhere else).
//!
//! Two modes:
//!
//! - `netmap_probe [ifname ...]` — opens `/dev/netmap` and issues `NIOCGINFO`
//!   for each interface name (no args: netmap's first port + the `vale1:1`
//!   software port), reporting ring/slot geometry.
//! - `netmap_probe send <ifname> [count]` — registers the port via the real
//!   [`netmap_backend`](net_rtp_aes67::transport::netmap_backend), stages
//!   `count` (default 10) RTP-sized packets in the TX ring, syncs, and
//!   prints slot statistics. Requires building with `--features netmap`.
//!
//! The netmap ABI (legacy `struct nmreq`, ioctl numbers) comes from the
//! always-compiled `transport::netmap_layout` module — the Linux build host
//! has no `/usr/include/net/netmap.h`, so nothing here may depend on a
//! system header. On Linux only the stub `main` compiles; the FreeBSD
//! branch is type-checked on the dev host via
//! `cargo check -p net-rtp-aes67 --example netmap_probe --target
//! x86_64-unknown-freebsd`.
//!
//! Run on FreeBSD:
//! `cargo run -p net-rtp-aes67 --example netmap_probe -- [ifname ...]`
//! `cargo run -p net-rtp-aes67 --example netmap_probe --features netmap -- send vale1:1`

#[cfg(target_os = "freebsd")]
mod freebsd {
    use net_rtp_aes67::transport::netmap_layout::{Nmreq, IFNAMSIZ, NETMAP_API, NIOCGINFO};
    use std::ffi::CStr;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::{AsRawFd, RawFd};
    use std::process::ExitCode;

    /// Probe one interface: fill `nr_name` + `nr_version`, NIOCGINFO, print
    /// the reported geometry.
    pub(super) fn probe_one(fd: RawFd, name: &str) -> io::Result<()> {
        let bytes = name.as_bytes();
        if bytes.len() >= IFNAMSIZ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "interface name {name:?} longer than IFNAMSIZ-1 (= {})",
                    IFNAMSIZ - 1
                ),
            ));
        }
        let mut req = Nmreq::default();
        req.nr_name[..bytes.len()].copy_from_slice(bytes);
        req.nr_version = NETMAP_API;

        // SAFETY: `req` is a valid, exclusively owned `repr(C)` buffer whose
        // size is the one encoded in `NIOCGINFO`; `fd` is an open fd. The
        // kernel reads/writes only `sizeof(nmreq)` bytes through the pointer.
        let rc = unsafe { libc::ioctl(fd, NIOCGINFO as libc::c_ulong, &mut req as *mut Nmreq) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        let label = if name.is_empty() {
            "<first port>"
        } else {
            name
        };
        println!("iface        : {label}");
        if let Ok(answered) = CStr::from_bytes_until_nul(&req.nr_name) {
            if !answered.is_empty() {
                println!("kernel name  : {}", answered.to_string_lossy());
            }
        }
        println!(
            "netmap api   : requested {NETMAP_API}, kernel {}",
            req.nr_version
        );
        println!(
            "region       : {} bytes at offset {}",
            req.nr_memsize, req.nr_offset
        );
        println!(
            "rings        : tx {} rx {}",
            req.nr_tx_rings, req.nr_rx_rings
        );
        println!(
            "slots/ring   : tx {} rx {}",
            req.nr_tx_slots, req.nr_rx_slots
        );
        Ok(())
    }

    /// Open `/dev/netmap`, probe each requested interface (or the default
    /// probe set), and report. Succeeds only if every probe answered.
    pub(super) fn run(args: impl Iterator<Item = String>) -> ExitCode {
        let names: Vec<String> = args.collect();
        let targets: Vec<String> = if names.is_empty() {
            // Default probe set per netmap semantics: empty nr_name asks the
            // kernel for the first port; vale1:1 exercises a software port.
            vec![String::new(), "vale1:1".to_string()]
        } else {
            names
        };

        let dev: File = match OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/netmap")
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("netmap_probe: open /dev/netmap failed: {e}");
                match e.raw_os_error() {
                    Some(libc::ENOENT) => {
                        eprintln!("  hint: netmap(4) not loaded — kldload netmap");
                    }
                    Some(libc::EPERM) | Some(libc::EACCES) => {
                        eprintln!(
                            "  hint: permission denied — check /dev/netmap access or run as root"
                        );
                    }
                    _ => {}
                }
                return ExitCode::FAILURE;
            }
        };

        let fd = dev.as_raw_fd();
        let mut ok = true;
        for name in &targets {
            if let Err(e) = probe_one(fd, name) {
                ok = false;
                eprintln!(
                    "netmap_probe: NIOCGINFO {name:?} failed: {e} (errno {})",
                    e.raw_os_error().unwrap_or(0)
                );
                match e.raw_os_error() {
                    Some(libc::ENOTTY) => eprintln!(
                        "  hint: ioctl rejected — struct nmreq layout mismatch? see NOTE(netmap-fixup) on Nmreq"
                    ),
                    Some(libc::ENXIO) | Some(libc::ENODEV) => {
                        eprintln!("  hint: no netmap adapter for this name");
                    }
                    Some(libc::EINVAL) => {
                        eprintln!("  hint: kernel may reject netmap API version {NETMAP_API}");
                    }
                    _ => {}
                }
            }
        }
        if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}

/// TX-ring smoke send via the real backend: stages `count` RTP-sized
/// packets, syncs once, prints slot statistics. Requires `--features
/// netmap` (the backend is feature-gated).
#[cfg(all(target_os = "freebsd", feature = "netmap"))]
fn send_mode(mut args: impl Iterator<Item = String>) -> std::process::ExitCode {
    use net_rtp_aes67::transport::netmap_backend::NetmapTransport;
    use std::process::ExitCode;

    let Some(ifname) = args.next() else {
        eprintln!("usage: netmap_probe send <ifname> [count]");
        return ExitCode::FAILURE;
    };
    let count: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);

    let mut t = match NetmapTransport::open(&ifname) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("netmap_probe: send: open {ifname:?} failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "send         : {ifname} (tx slots {}, buf size {} B)",
        t.tx_num_slots(),
        t.tx_buf_size()
    );

    // RTP-sized packet: 12 B header + 256 stereo L16 frames = 1036 B.
    let packet = [0xA5u8; 12 + 2 * 2 * 256];
    let free_before = t.tx_ring_slots_free();
    let mut staged = 0usize;
    let mut full = false;
    for _ in 0..count {
        match t.send_raw(&packet) {
            Ok(_) => staged += 1,
            Err(e) => {
                eprintln!("netmap_probe: send: staged {staged}/{count}, then: {e}");
                full = true;
                break;
            }
        }
    }
    if let Err(e) = t.txsync() {
        eprintln!("netmap_probe: send: txsync failed: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "result       : staged {staged}/{count} packets of {} B, txsync OK",
        packet.len()
    );
    println!(
        "tx slots     : {} free before, {} free after sync",
        free_before,
        t.tx_ring_slots_free()
    );
    if let Err(e) = t.close() {
        eprintln!("netmap_probe: send: close failed: {e}");
        return ExitCode::FAILURE;
    }
    if full || staged != count {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `send` was requested but the example was built without the `netmap`
/// feature, so the backend module is absent.
#[cfg(all(target_os = "freebsd", not(feature = "netmap")))]
fn send_mode(_args: impl Iterator<Item = String>) -> std::process::ExitCode {
    eprintln!("netmap_probe: send mode requires building with --features netmap");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "freebsd")]
fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("send") => send_mode(&mut args),
        // No "send" subcommand: fall back to probe mode, re-including the
        // first arg when it was an interface name.
        first => freebsd::run(first.map(str::to_string).into_iter().chain(args)),
    }
}

#[cfg(not(target_os = "freebsd"))]
fn main() {
    // Touch the shared ABI module so this file cannot drift from the
    // backend's declarations (type-checks the dependency on the Linux host).
    let _: net_rtp_aes67::transport::netmap_layout::Nmreq =
        net_rtp_aes67::transport::netmap_layout::Nmreq::default();
    eprintln!("netmap_probe: FreeBSD-only (netmap(4) is a FreeBSD kernel feature)");
    eprintln!("usage (FreeBSD): netmap_probe [ifname ...] | netmap_probe send <ifname> [count]");
}
