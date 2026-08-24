//! Real netmap(4) ring I/O — the zero-copy backend behind the `netmap`
//! feature (`cfg(all(unix, feature = "netmap"))`).
//!
//! **Scope (this step):** open a netmap port ([`NetmapTransport::open`] =
//! `NIOCREGIF` + `mmap`) and move raw packets through the first TX/RX ring
//! ([`NetmapTransport::send_raw`] / [`NetmapTransport::recv_raw`] +
//! [`NetmapTransport::txsync`] / [`NetmapTransport::rxsync`]). RTP framing on
//! top of the rings is the *next* step, so the
//! [`NetworkAudioTransport`](crate::NetworkAudioTransport) impl deliberately
//! still returns [`TransportError::Unimplemented`] — use
//! [`UdpTransport`](super::UdpTransport) for framed RTP meanwhile.
//!
//! Simplification (deliberate): only TX ring 0 and RX ring 0 are exposed.
//! For the VALE software ports this backend targets (`vale1:1`: 1 TX + 1 RX
//! ring, measured on FreeBSD 15.1) that is the complete port; NIC ports with
//! multiple queues get a ring-set API later if needed.
//!
//! The code compiles on every Unix (Linux included — it is only *runnable*
//! where `/dev/netmap` exists), which keeps the Linux CI host able to
//! lint and test everything except live ring traffic. All kernel-shared
//! memory accesses (`head`/`cur`/`tail`, slot descriptors) are volatile:
//! the kernel writes them concurrently between sync ioctls.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::ptr;

use super::netmap_layout::{
    rx_ring0_index, NetmapIf, NetmapRingHeader, NetmapSlot, Nmreq, IFNAMSIZ, NETMAP_API,
    NETMAP_IF_RING_OFS_OFFSET, NETMAP_RING_HEADER_SIZE, NIOCREGIF, NIOCRXSYNC, NIOCTXSYNC,
};
use crate::{Codec, NetworkAudioTransport, Result, TransportError};

/// One registered netmap port: the `/dev/netmap` fd, the mmapped region and
/// the cached first TX/RX ring views.
struct Inner {
    /// Owning fd; dropping it unregisters the port in the kernel.
    dev: File,
    /// `mmap` base of the shared region (`nr_memsize` bytes).
    base: *mut u8,
    /// Region size in bytes.
    map_len: usize,
    /// First TX ring.
    tx: Ring,
    /// First RX ring.
    rx: Ring,
}

/// A cached view of one ring: the header pointer plus the immutable geometry
/// snapshot taken at registration time. Slot/buffer pointer arithmetic uses
/// only the snapshot constants plus the live `head`/`cur`/`tail`.
struct Ring {
    /// Address of the ring header (== ring start; buffers are addressed
    /// relative to it via `buf_ofs`).
    hdr: *mut NetmapRingHeader,
    /// `num_slots` snapshot.
    num_slots: u32,
    /// `nr_buf_size` snapshot.
    buf_size: u32,
    /// `buf_ofs` snapshot (offset of buffer 0 from the ring start).
    buf_ofs: i64,
}

impl Ring {
    /// Snapshots the immutable geometry of the ring at `hdr`.
    fn from_hdr(hdr: *mut NetmapRingHeader) -> Result<Self> {
        // SAFETY: `hdr` points at a ring header inside the mmapped region
        // filled by the kernel before NIOCREGIF returned. The geometry
        // fields are never written again, but volatile reads keep the
        // optimizer from tearing the loads.
        let num_slots = unsafe { (&raw const (*hdr).num_slots).read_volatile() };
        let buf_size = unsafe { (&raw const (*hdr).nr_buf_size).read_volatile() };
        let buf_ofs = unsafe { (&raw const (*hdr).buf_ofs).read_volatile() };
        if num_slots == 0 || buf_size == 0 || buf_ofs < 0 {
            return Err(TransportError::Network(format!(
                "invalid netmap ring geometry (num_slots={num_slots}, buf_size={buf_size}, buf_ofs={buf_ofs})"
            )));
        }
        Ok(Self {
            hdr,
            num_slots,
            buf_size,
            buf_ofs,
        })
    }

    fn head(&self) -> u32 {
        // SAFETY: `hdr` is inside the live mapping; `head` is kernel-shared.
        unsafe { (&raw const (*self.hdr).head).read_volatile() }
    }

    fn cur(&self) -> u32 {
        // SAFETY: see `head`.
        unsafe { (&raw const (*self.hdr).cur).read_volatile() }
    }

    fn tail(&self) -> u32 {
        // SAFETY: see `head`.
        unsafe { (&raw const (*self.hdr).tail).read_volatile() }
    }

    fn set_head(&self, v: u32) {
        // SAFETY: see `head`; publishing the release point must not be
        // reordered or cached.
        unsafe { (&raw mut (*self.hdr).head).write_volatile(v) };
    }

    fn set_cur(&self, v: u32) {
        // SAFETY: see `head`.
        unsafe { (&raw mut (*self.hdr).cur).write_volatile(v) };
    }

    /// Free TX slots as seen by userspace: `tail - head` (wrapping int32
    /// difference, like netmap's `nm_ring_space`), clamped at 0.
    /// Diagnostic snapshot: (ringid, dir, head, cur, tail, num_slots).
    /// `dir`: 0 = TX, 1 = RX. For diagnostics/examples only.
    pub fn debug_state(&self) -> (u16, u16, u32, u32, u32, u32) {
        // SAFETY: read-only volatile loads from the live mapping.
        unsafe {
            (
                (&raw const (*self.hdr).ringid).read_volatile(),
                (&raw const (*self.hdr).dir).read_volatile(),
                self.head(),
                self.cur(),
                self.tail(),
                self.num_slots,
            )
        }
    }

    fn slots_free(&self) -> usize {
        // netmap TX accounting (live-verified vale57/60): after each txsync
        // the kernel sets `tail` to the slot AFTER the last consumed one
        // (head=1, tail=0 means 1 in flight, NOT full). Free slots are
        // num_slots minus the in-flight span (head - tail, mod num_slots):
        //   head=1 tail=0     -> in-flight 1  -> free num_slots-1
        //   head=8 tail=7     -> in-flight 1  -> free num_slots-1
        //   head==tail        -> ambiguous: 0 (truly full) on a used ring,
        //                        num_slots on a fresh ring (head=tail=0) —
        //                        callers treat 0 as "sync and retry", which
        //                        self-corrects on the next poll.
        let n = self.num_slots;
        if n == 0 {
            return 0;
        }
        let inflight = self.head().wrapping_sub(self.tail()) % n;
        (n - inflight) as usize
    }

    /// Pointer to slot `i` — callers must have checked `i < num_slots`.
    fn slot(&self, i: u32) -> *mut NetmapSlot {
        // SAFETY: `i < num_slots` is the caller's invariant; the slot array
        // begins at NETMAP_RING_HEADER_SIZE past the ring start (locked by
        // the layout snapshot tests).
        unsafe {
            self.hdr
                .cast::<u8>()
                .add(NETMAP_RING_HEADER_SIZE)
                .cast::<NetmapSlot>()
                .add(i as usize)
        }
    }

    /// Buffer address for buffer index `idx` — netmap's `NETMAP_BUF`:
    /// `ring_start + buf_ofs + idx * nr_buf_size`. `idx` is kernel-managed
    /// and trusted (a hostile kernel breaks far more than this crate).
    fn buffer(&self, idx: u32) -> *mut u8 {
        // SAFETY: computed from the immutable geometry snapshot; the buffer
        // is at least `buf_size` bytes.
        unsafe {
            self.hdr
                .cast::<u8>()
                .add(self.buf_ofs as usize + idx as usize * self.buf_size as usize)
        }
    }
}

/// Zero-copy netmap transport for one port (FreeBSD runtime; see the module
/// docs for the current raw-ring scope).
///
/// Constructed via [`NetmapTransport::open`]; [`NetmapTransport::new`]
/// returns a *detached* handle retained for compatibility with the P1 stub
/// surface (raw operations on it fail with [`TransportError::Network`]).
pub struct NetmapTransport {
    ifname: String,
    inner: Option<Inner>,
    /// Memory-allocator id the kernel assigned (read back after NIOCREGIF).
    memid: u16,
    /// RTP send clock (matches UdpTransport's framing state).
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    /// Channel count used for the timestamp advance (set at open; default 2).
    channels: u16,
}

// SAFETY: the transport exclusively owns the netmap fd and its mapping, and
// every mutating operation goes through `&mut self`. Moving the value to a
// single worker thread at a time (the intended use) is sound. It is
// deliberately NOT `Sync` — do not share `&NetmapTransport` across threads.
unsafe impl Send for NetmapTransport {}

impl fmt::Debug for NetmapTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("NetmapTransport");
        match &self.inner {
            None => s.field("state", &"detached").finish(),
            Some(inner) => s
                .field("ifname", &self.ifname)
                .field("map_len", &inner.map_len)
                .field("tx_num_slots", &inner.tx.num_slots)
                .field("rx_num_slots", &inner.rx.num_slots)
                .field("buf_size", &inner.tx.buf_size)
                .finish(),
        }
    }
}

impl NetmapTransport {
    /// Returns a *detached* transport (no port bound). Raw ring operations
    /// fail until [`NetmapTransport::open`] binds a port. Kept from the P1
    /// stub surface so callers written against the stub keep compiling.
    ///
    /// There is intentionally no `Default`: a real transport is bound to a
    /// port via `open` at construction time.
    #[must_use]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            ifname: String::new(),
            inner: None,
            memid: 0,
            seq: 0,
            timestamp: 0,
            ssrc: system_ssrc(),
            channels: 2,
        }
    }

    /// Registers the netmap port `ifname` (e.g. `"vale1:1"` or `"vmx0"`)
    /// with `NIOCREGIF` (all rings) and maps the shared region, caching the
    /// first TX and RX ring.
    pub fn open(ifname: &str) -> Result<Self> {
        Self::open_with_memid(ifname, None)
    }

    /// [`open`](Self::open) with an explicit memory-allocator id pin.
    ///
    /// **VALE switching requires all ports of one switch to share a memory
    /// allocator.** The kernel assigns DIFFERENT `nr_arg2` memids to
    /// independently registered ports (live-verified: reader memid=2,
    /// writer memid=3 → zero delivery), so a peer group must pin everyone
    /// to the FIRST port's memid. Call [`memid`](Self::memid) after opening
    /// the first port and pass it here for the rest.
    pub fn open_with_memid(ifname: &str, memid: Option<u16>) -> Result<Self> {
        let bytes = ifname.as_bytes();
        if bytes.len() >= IFNAMSIZ {
            return Err(TransportError::Network(format!(
                "interface name {ifname:?} longer than IFNAMSIZ-1 (= {})",
                IFNAMSIZ - 1
            )));
        }

        let dev = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/netmap")
            .map_err(|e| TransportError::Network(format!("open /dev/netmap: {e}")))?;

        let mut req = Nmreq::default();
        req.nr_name[..bytes.len()].copy_from_slice(bytes);
        req.nr_version = NETMAP_API;
        req.nr_ringid = 0; // first ring pair of the port
        req.nr_flags = 0; // default registration: all rings
        if let Some(id) = memid {
            req.nr_arg2 = id; // pin the memory allocator (VALE switching)
        }

        // SAFETY: `req` is a valid, exclusively owned repr(C) buffer whose
        // size is the one encoded in NIOCREGIF; the kernel reads/writes only
        // `sizeof(nmreq)` bytes through the pointer.
        let rc = unsafe {
            libc::ioctl(
                dev.as_raw_fd(),
                NIOCREGIF as libc::c_ulong,
                &mut req as *mut Nmreq,
            )
        };
        if rc != 0 {
            let e = io::Error::last_os_error();
            return Err(TransportError::Network(format!(
                "NIOCREGIF {ifname:?}: {e}"
            )));
        }

        let offset = req.nr_offset as usize;
        let map_len = req.nr_memsize as usize;
        if map_len == 0 {
            return Err(TransportError::Network(
                "NIOCREGIF returned a zero region size".into(),
            ));
        }

        // SAFETY: freshly registered netmap region; `map_len` bytes are
        // shared with the kernel for the lifetime of the fd (unmapped in
        // `teardown` before the fd closes).
        let map = unsafe {
            libc::mmap(
                ptr::null_mut::<libc::c_void>(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                dev.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            let e = io::Error::last_os_error();
            return Err(TransportError::Network(format!(
                "mmap netmap region ({map_len} B): {e}"
            )));
        }
        let base = map.cast::<u8>();

        let inner = (|| -> Result<Inner> {
            // SAFETY: offset is kernel-provided and points at the
            // `netmap_if` inside the mapping.
            let nif: *const NetmapIf = unsafe { base.add(offset).cast() };
            let ni_tx_rings = unsafe { (&raw const (*nif).ni_tx_rings).read_volatile() };
            let ni_host_tx_rings = unsafe { (&raw const (*nif).ni_host_tx_rings).read_volatile() };
            let ni_rx_rings = unsafe { (&raw const (*nif).ni_rx_rings).read_volatile() };
            if ni_tx_rings == 0 || ni_rx_rings == 0 {
                return Err(TransportError::Network(format!(
                    "port {ifname:?} has no usable ring pair (tx={ni_tx_rings}, rx={ni_rx_rings})"
                )));
            }

            // VALE ring layout (live-verified): ofs[0] (dir=1) is the SEND
            // ring; ofs[tx+host_tx] (= 2 for vale ports) is the DELIVERY
            // ring where the switch deposits packets.
            let tx_ofs = read_ring_ofs(base, offset, 0)?;
            let rx_ofs =
                read_ring_ofs(base, offset, rx_ring0_index(ni_tx_rings, ni_host_tx_rings))?;
            if tx_ofs <= 0 || rx_ofs <= 0 {
                return Err(TransportError::Network(format!(
                    "port {ifname:?} reported non-positive ring offsets (tx={tx_ofs}, rx={rx_ofs})"
                )));
            }

            // SAFETY: ring offsets are relative to the `netmap_if` address
            // and land inside the mapping.
            let tx_hdr = unsafe {
                base.add(offset + tx_ofs as usize)
                    .cast::<NetmapRingHeader>()
            };
            let rx_hdr = unsafe {
                base.add(offset + rx_ofs as usize)
                    .cast::<NetmapRingHeader>()
            };
            Ok(Inner {
                dev,
                base,
                map_len,
                tx: Ring::from_hdr(tx_hdr)?,
                rx: Ring::from_hdr(rx_hdr)?,
            })
        })();
        match inner {
            Ok(inner) => Ok(Self {
                ifname: ifname.to_string(),
                inner: Some(inner),
                memid: req.nr_arg2,
                seq: 0,
                timestamp: 0,
                ssrc: system_ssrc(),
                channels: 2,
            }),
            Err(e) => {
                // Best-effort unmap; `dev` drops and unregisters the port.
                // SAFETY: same successful single mapping as above.
                unsafe { libc::munmap(map, map_len) };
                Err(e)
            }
        }
    }

    /// The kernel-assigned memory-allocator id (0 when detached). Pin
    /// follow-up ports of the same VALE switch to this value via
    /// [`open_with_memid`](Self::open_with_memid).
    #[must_use]
    pub fn memid(&self) -> u16 {
        self.memid
    }

    /// Acknowledges every stale slot descriptor in the RX ring
    /// (`head = cur = tail`) and syncs — a FRESH VALE ring starts with
    /// `tail = num_slots-1` describing GARBAGE buffers (previous mappings);
    /// until drained, every poll misreads stale slots as deliveries.
    /// Call once after `open`, before the first `recv_raw`.
    pub fn prime_rx(&mut self) -> Result<()> {
        let Some(inner) = &self.inner else {
            return Err(not_open());
        };
        let tail = inner.rx.tail();
        inner.rx.set_head(tail);
        inner.rx.set_cur(tail);
        self.rxsync()
    }

    /// Diagnostic snapshot of the TX ring (see [`Ring::debug_state`]).
    #[must_use]
    pub fn tx_debug_state(&self) -> Option<(u16, u16, u32, u32, u32, u32)> {
        self.inner.as_ref().map(|i| i.tx.debug_state())
    }

    /// Diagnostic snapshot of the RX ring (see [`Ring::debug_state`]).
    #[must_use]
    pub fn rx_debug_state(&self) -> Option<(u16, u16, u32, u32, u32, u32)> {
        self.inner.as_ref().map(|i| i.rx.debug_state())
    }

    /// Free TX slots (`tail - head`); 0 when the ring is full **or** the
    /// transport is detached.
    pub fn tx_ring_slots_free(&self) -> usize {
        self.inner.as_ref().map_or(0, |i| i.tx.slots_free())
    }

    /// TX ring slot count (0 when detached).
    pub fn tx_num_slots(&self) -> u32 {
        self.inner.as_ref().map_or(0, |i| i.tx.num_slots)
    }

    /// TX/RX buffer size in bytes (0 when detached).
    pub fn tx_buf_size(&self) -> u32 {
        self.inner.as_ref().map_or(0, |i| i.tx.buf_size)
    }

    /// Writes one packet into the next free TX slot and advances `head`.
    /// Does **not** notify the kernel — call [`NetmapTransport::txsync`]
    /// afterwards (netmap batches many slots per sync).
    ///
    /// Returns the number of bytes staged (the full `packet.len()`).
    pub fn send_raw(&mut self, packet: &[u8]) -> Result<usize> {
        let Some(inner) = &self.inner else {
            return Err(not_open());
        };
        let tx = &inner.tx;
        if packet.len() > u16::MAX as usize {
            return Err(TransportError::Network(format!(
                "packet of {} B cannot fit a 16-bit slot length",
                packet.len()
            )));
        }
        if packet.len() > tx.buf_size as usize {
            return Err(TransportError::Network(format!(
                "packet of {} B exceeds the netmap buffer size of {} B",
                packet.len(),
                tx.buf_size
            )));
        }
        if tx.slots_free() == 0 {
            return Err(TransportError::Network(
                "netmap TX ring full (head == tail); txsync() before sending more".into(),
            ));
        }
        let head = tx.head();
        if head >= tx.num_slots {
            return Err(corrupt_ring("tx.head", head, tx.num_slots));
        }
        let slot = tx.slot(head);
        // SAFETY: `head < num_slots` checked above; `buf_idx` and the buffer
        // region are kernel-managed and `buf_size` bytes large; the length
        // check above bounds the copy.
        unsafe {
            let buf_idx = (&raw const (*slot).buf_idx).read_volatile();
            let dst = tx.buffer(buf_idx);
            ptr::copy_nonoverlapping(packet.as_ptr(), dst, packet.len());
            (&raw mut (*slot).len).write_volatile(packet.len() as u16);
        }
        tx.set_head((head + 1) % tx.num_slots);
        // netmap TX convention (see pkt-gen / the C reference): `cur` is the
        // kernel's wakeup point and MUST follow `head`. FreeBSD's VALE TX
        // path leaves packets unconsumed (tail frozen) when `cur` lags —
        // live-verified on vale6:b: 8 staged packets, tail 1023 → never moved
        // without this line; with it the kernel drains the ring.
        tx.set_cur((head + 1) % tx.num_slots);
        Ok(packet.len())
    }

    /// Tells the kernel the slots up to `head` are ready to transmit
    /// (`NIOCTXSYNC`).
    pub fn txsync(&mut self) -> Result<()> {
        self.sync_ioctl(NIOCTXSYNC, "NIOCTXSYNC")
    }

    /// Asks the kernel to move received packets into the RX ring
    /// (`NIOCRXSYNC`).
    pub fn rxsync(&mut self) -> Result<()> {
        self.sync_ioctl(NIOCRXSYNC, "NIOCRXSYNC")
    }

    fn sync_ioctl(&self, cmd: u64, name: &str) -> Result<()> {
        let Some(inner) = &self.inner else {
            return Err(not_open());
        };
        // SAFETY: `dev` is an open netmap fd; the `_IO` sync commands ignore
        // the third argument entirely (variadic C callers pass none — we
        // mirror that with a 0 value rather than a null pointer, matching
        // the C reference programs byte-for-byte).
        let rc = unsafe {
            libc::ioctl(
                inner.dev.as_raw_fd(),
                cmd as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        if rc != 0 {
            let e = io::Error::last_os_error();
            return Err(TransportError::Network(format!("{name}: {e}")));
        }
        Ok(())
    }

    /// Reads one received packet from the RX ring cursor (`cur`), copies at
    /// most `buf.len()` bytes into `buf` (longer slots are truncated) and
    /// releases the slot (`cur`/`head` advance). Call
    /// [`NetmapTransport::rxsync`] first to let the kernel fill the ring.
    ///
    /// Returns the number of bytes copied; **0 means nothing pending**
    /// (`cur == tail`).
    pub fn recv_raw(&mut self, buf: &mut [u8]) -> Result<usize> {
        let Some(inner) = &self.inner else {
            return Err(not_open());
        };
        let rx = &inner.rx;
        let cur = rx.cur();
        if cur >= rx.num_slots {
            return Err(corrupt_ring("rx.cur", cur, rx.num_slots));
        }
        if cur == rx.tail() {
            return Ok(0); // nothing pending — caller should rxsync()
        }
        let slot = rx.slot(cur);
        // SAFETY: `cur < num_slots` checked above; slot descriptors are
        // kernel-written for RX, so read them volatile and clamp defensively.
        let (len, buf_idx) = unsafe {
            (
                (&raw const (*slot).len).read_volatile(),
                (&raw const (*slot).buf_idx).read_volatile(),
            )
        };
        let n = (len as usize).min(buf.len()).min(rx.buf_size as usize);
        // SAFETY: `buf_idx` is kernel-managed; `n <= buf_size` bounds the
        // source copy and `n <= buf.len()` bounds the destination.
        unsafe {
            let src = rx.buffer(buf_idx);
            ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), n);
        }
        let next = (cur + 1) % rx.num_slots;
        rx.set_cur(next);
        rx.set_head(next); // release the slot back to the kernel
        Ok(n)
    }

    /// Unmaps the shared region and closes the fd (unregistering the port).
    /// Idempotent for detached handles; `Drop` performs the same cleanup
    /// best-effort.
    pub fn close(mut self) -> Result<()> {
        self.teardown()
    }

    fn teardown(&mut self) -> Result<()> {
        match self.inner.take() {
            None => Ok(()),
            Some(inner) => {
                // SAFETY: `base`/`map_len` came from the successful mmap in
                // `open` and are unmapped exactly once (the Option is taken).
                // The `File` drop that follows closes the fd and
                // unregisters the port.
                let rc = unsafe { libc::munmap(inner.base.cast::<libc::c_void>(), inner.map_len) };
                if rc != 0 {
                    let e = io::Error::last_os_error();
                    return Err(TransportError::Network(format!("munmap: {e}")));
                }
                Ok(())
            }
        }
    }
}

impl Drop for NetmapTransport {
    fn drop(&mut self) {
        // Best-effort: explicit `close()` reports errors; Drop ignores them.
        let _ = self.teardown();
    }
}

impl NetworkAudioTransport for NetmapTransport {
    fn send_rtp(&mut self, payload: &[u8], codec: Codec) -> Result<()> {
        // send_raw itself fails with `not_open()` when no port is bound.
        let payload_type = crate::codec_to_payload_type(codec, self.channels);
        let header = crate::RtpHeader::new(payload_type, self.seq, self.timestamp, self.ssrc);
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(crate::timestamp_advance(
            payload.len(),
            codec,
            self.channels,
        ));

        let packet = crate::RtpPacket {
            header,
            payload: payload.to_vec(),
        };
        let bytes = packet.encode();
        let staged = self.send_raw(&bytes)?;
        if staged < bytes.len() {
            return Err(TransportError::Network(format!(
                "TX ring slot too small for the RTP packet ({} of {} bytes staged)",
                staged,
                bytes.len()
            )));
        }
        // Push to the kernel immediately: one sync per packet. Callers that
        // prefer batching can use send_raw + txsync directly. TX only —
        // NIOCRXSYNC on this fd would drain the SEND ring (ofs[0] carries
        // dir=1) and stall subsequent sends.
        self.txsync()
    }

    fn recv_rtp(&mut self) -> Result<Vec<u8>> {
        // Pull any arrived packets, then take ONE from the RX ring and parse
        // it as RTP. Non-RTP traffic on the port surfaces as a parse error,
        // exactly like a UDP socket receiving a datagram from elsewhere.
        self.rxsync()?;
        let mut buf = vec![0u8; 65_535];
        let n = self.recv_raw(&mut buf)?;
        if n == 0 {
            return Err(TransportError::Network("no packet available".into()));
        }
        let packet = crate::RtpPacket::parse(&buf[..n])?;
        Ok(packet.payload)
    }

    fn join_multicast(&mut self, _group: Ipv4Addr) -> Result<()> {
        // A netmap port is not an IP socket; multicast membership belongs to
        // the external network path, not to the ring.
        Err(TransportError::Unimplemented(
            "netmap multicast join (not an IP socket; use UdpTransport)",
        ))
    }
}

/// Derives a best-effort random SSRC from wall-clock nanoseconds (same
/// policy as `UdpTransport`).
fn system_ssrc() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 as u32)
        .unwrap_or(0x0C0F_FEEE)
}

/// Reads `ring_ofs[index]` (an `ssize_t` at
/// [`NETMAP_IF_RING_OFS_OFFSET`] + 8·index past the `netmap_if`).
fn read_ring_ofs(base: *mut u8, if_offset: usize, index: usize) -> Result<i64> {
    // SAFETY: the ring-offset table follows the fixed part of `netmap_if`
    // inside the mapping; entries are written by the kernel before
    // NIOCREGIF returns and never change afterwards.
    let ofs = unsafe {
        base.add(if_offset + NETMAP_IF_RING_OFS_OFFSET + index * core::mem::size_of::<i64>())
            .cast::<i64>()
            .read_volatile()
    };
    if ofs <= 0 {
        return Err(TransportError::Network(format!(
            "netmap_if.ring_ofs[{index}] is not a valid offset ({ofs})"
        )));
    }
    Ok(ofs)
}

fn not_open() -> TransportError {
    TransportError::Network("netmap port not open (bind one via NetmapTransport::open)".into())
}

fn corrupt_ring(field: &str, value: u32, num_slots: u32) -> TransportError {
    TransportError::Network(format!(
        "kernel reported {field}={value} outside the ring (num_slots={num_slots})"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "freebsd"))]
    #[test]
    fn open_fails_cleanly_without_netmap_device() {
        // /dev/netmap does not exist on non-FreeBSD hosts: open must return
        // a Network error — not panic, not Unimplemented.
        let err = NetmapTransport::open("vale1:1").expect_err("open must fail off-FreeBSD");
        assert!(matches!(err, TransportError::Network(_)), "got: {err:?}");
    }

    #[test]
    fn ifname_longer_than_ifnamsiz_is_rejected() {
        // Validated before the device is even opened, so this passes on
        // every host. "0123456789abcdef" is exactly IFNAMSIZ bytes — one
        // byte too many for a NUL terminator.
        let err = NetmapTransport::open("0123456789abcdef").expect_err("name too long");
        assert!(matches!(err, TransportError::Network(_)), "got: {err:?}");
    }

    #[test]
    fn detached_transport_rejects_raw_ops_and_rtp_framing() {
        let mut t = NetmapTransport::new();
        // Raw ring API: detached → Network errors (a usage bug).
        assert_eq!(t.tx_ring_slots_free(), 0);
        assert!(matches!(
            t.send_raw(&[1, 2, 3]),
            Err(TransportError::Network(_))
        ));
        let mut scratch = [0u8; 8];
        assert!(matches!(
            t.recv_raw(&mut scratch),
            Err(TransportError::Network(_))
        ));
        assert!(matches!(t.txsync(), Err(TransportError::Network(_))));
        assert!(matches!(t.rxsync(), Err(TransportError::Network(_))));
        // Framing API: detached → Network error too (send_raw's guard fires
        // before any RTP bytes are built). join_multicast stays Unimplemented
        // by design — a netmap port is not an IP socket.
        assert!(matches!(
            t.send_rtp(&[], Codec::PcmL16),
            Err(TransportError::Network(_))
        ));
        // Teardown of a detached handle is a clean no-op.
        assert!(t.close().is_ok());
    }
}
