//! FreeBSD netmap(4) ABI re-declaration — the single source of truth shared by
//! the [`netmap_backend`](super::netmap_backend) FFI and the
//! `netmap_probe` example.
//!
//! The Linux build host has no `/usr/include/net/netmap.h`, so every type and
//! ioctl number below is re-declared from the FreeBSD 15.1 headers
//! (`net/netmap.h` + `net/netmap_legacy.h`) as dependency-free `repr(C)`
//! items. Because they carry no FFI calls, this module compiles on every
//! platform and with **no cargo feature**, which lets the layout snapshot
//! tests in this file run in the default test suite — the constants and the
//! struct sizes cannot drift silently.
//!
//! NOTE(netmap-fixup): if FreeBSD verification ever returns `ENOTTY` on
//! `NIOCGINFO`/`NIOCREGIF` (nmreq size/layout mismatch) or on the sync
//! ioctls, re-derive these declarations against the real header via
//! `cc -E <net/netmap.h>` on the FreeBSD box.

/// `IFNAMSIZ` from `<net/if.h>`.
pub const IFNAMSIZ: usize = 16;

/// netmap API version this crate speaks (accepted range 11..=14). The kernel
/// overwrites `nr_version` with its own version, which callers may report.
pub const NETMAP_API: u32 = 14;

/// `struct netmap_slot` — one buffer descriptor inside a ring
/// (`net/netmap.h`, 16 bytes, align 8).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NetmapSlot {
    /// Index of the buffer this slot points at (kernel-managed).
    pub buf_idx: u32,
    /// Packet length in bytes (TX: written by user; RX: written by kernel).
    pub len: u16,
    /// Slot flags (`NS_*`; e.g. `NS_MOREFRAG` on fragmented packets).
    pub flags: u16,
    /// Reserved for future use (e.g. physical addresses).
    pub ptr: u64,
}

/// `struct nmreq` — exact legacy layout from FreeBSD 15.1
/// `net/netmap_legacy.h` (60 bytes: 16B name + 6×u32 + 6×u16 + 3×u32),
/// verified field-by-field against the header and proven at runtime by the
/// `netmap_probe` example (NIOCGINFO answered API 14 on FreeBSD 15.1).
#[repr(C)]
#[derive(Default)]
pub struct Nmreq {
    /// in: interface name ("vmx0", "vale1:1", ...). Empty = first port.
    pub nr_name: [u8; IFNAMSIZ],
    /// in: requested API version; out: kernel netmap version.
    pub nr_version: u32,
    /// out: `netmap_if` offset inside the mmapped region.
    pub nr_offset: u32,
    /// out: size of the netmap shared region (bytes).
    pub nr_memsize: u32,
    /// out: slots in each tx ring.
    pub nr_tx_slots: u32,
    /// out: slots in each rx ring.
    pub nr_rx_slots: u32,
    /// out: number of tx rings.
    pub nr_tx_rings: u16,
    /// out: number of rx rings.
    pub nr_rx_rings: u16,
    /// in: ring selection for NIOCREGIF.
    pub nr_ringid: u16,
    /// in: bridge/registration sub-command (0 for plain registration).
    pub nr_cmd: u16,
    /// in: extra arg 1 (unused).
    pub nr_arg1: u16,
    /// in: allocator id (unused).
    pub nr_arg2: u16,
    /// in: extra buffers request for NIOCREGIF (0 = none).
    pub nr_arg3: u32,
    /// in: registration flags `NR_REG_*` (0 = all rings, default port type).
    pub nr_flags: u32,
    /// reserved.
    pub nr_spare2: [u32; 1],
}

/// `struct netmap_if` — the port descriptor at `nr_offset` inside the mmapped
/// region (`net/netmap.h`). The variable-length `ring_ofs[]` table follows the
/// fixed part, at [`NETMAP_IF_RING_OFS_OFFSET`].
#[repr(C)]
pub struct NetmapIf {
    /// Name of the interface (NUL-terminated).
    pub ni_name: [u8; IFNAMSIZ],
    /// API version of the registration.
    pub ni_version: u32,
    /// Port flags (`NI_*`).
    pub ni_flags: u32,
    /// Number of NIC TX rings.
    pub ni_tx_rings: u32,
    /// Number of NIC RX rings.
    pub ni_rx_rings: u32,
    /// Head index of the extra-buffer freelist (unused here).
    pub ni_bufs_head: u32,
    /// Number of host (stack) TX rings.
    pub ni_host_tx_rings: u32,
    /// Number of host (stack) RX rings.
    pub ni_host_rx_rings: u32,
    /// Reserved.
    pub ni_spare1: [u32; 3],
}

/// Offset of the variable-length `ring_ofs[]` table (entries: `ssize_t`,
/// i.e. ring offsets relative to the `netmap_if` address) past the start of
/// `struct netmap_if`.
///
/// Derivation: `sizeof(netmap_if)` fixed part = 16 (name) + 7×4 (u32 fields)
/// + 4 (bufs_head) — all `u32`-aligned — + 3×4 (spare) = 56, and 56 is
///   already 8-aligned so the first `ssize_t` lands exactly there.
pub const NETMAP_IF_RING_OFS_OFFSET: usize = 56;

/// `ring_ofs[]` index of RX ring 0.
///
/// The table order is: all TX rings first (NIC TX rings, then host TX rings),
/// then all RX rings (NIC RX rings, then host RX rings):
/// `tx0..tx(ni_tx_rings) host_tx.. rx0 at ni_tx_rings + ni_host_tx_rings ..`.
pub const fn rx_ring0_index(ni_tx_rings: u32, ni_host_tx_rings: u32) -> usize {
    (ni_tx_rings + ni_host_tx_rings) as usize
}

/// FreeBSD `struct timeval` on 64-bit: two `i64` members (16 bytes, align 8).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NetmapTimeval {
    /// Seconds.
    pub tv_sec: i64,
    /// Microseconds.
    pub tv_usec: i64,
}

/// `struct netmap_ring` fixed header (`net/netmap.h`). The variable-length
/// `slot[]` array starts at [`NETMAP_RING_HEADER_SIZE`] (= 256) — the header
/// size itself.
///
/// Field-offset derivation (x86_64, natural `repr(C)` layout):
/// `buf_ofs`(i64,0) `num_slots`(u32,8) `nr_buf_size`(u32,12) `ringid`(u16,16)
/// `dir`(u16,18) `head`(u32,20) `cur`(u32,24) `tail`(u32,28) `flags`(u32,32)
/// +4 pad → `ts`(16B,40) `offset_mask`(u64,56) `buf_align`(u64,64..72),
/// pad(56B,72..128) so `sem` starts 64-aligned (matching the C header's
/// `__attribute__((__aligned__(NM_CACHE_ALIGN)))`), `sem[128]`(128..256).
/// sizeof == 256, live-verified on FreeBSD 15.1 amd64.
#[repr(C)]
pub struct NetmapRingHeader {
    /// Offset of buffer 0 from the ring start (`NETMAP_BUF` base).
    pub buf_ofs: i64,
    /// Number of slots in the ring.
    pub num_slots: u32,
    /// Size of each buffer (bytes).
    pub nr_buf_size: u32,
    /// Ring id.
    pub ringid: u16,
    /// 0 = TX ring, 1 = RX ring.
    pub dir: u16,
    /// (kernel←user) first buffer to release + first free buffer to fill.
    pub head: u32,
    /// (user) next slot to process (cursor between `head` and `tail`).
    pub cur: u32,
    /// (kernel→user) last released buffer + 1 (TX: free watermark; RX: fill
    /// watermark).
    pub tail: u32,
    /// Ring flags.
    pub flags: u32,
    /// Timestamp of the last sync.
    pub ts: NetmapTimeval,
    /// Offset mask for transmit timing.
    pub offset_mask: u64,
    /// Buffer alignment requirement.
    pub buf_align: u64,
    /// Explicit padding so `sem` starts at a 64-byte boundary, mirroring
    /// the C header's `__attribute__((__aligned__(NM_CACHE_ALIGN)))` on
    /// `sem` (fields end at 72; padded to 128).
    _pad_to_cache_align: [u8; 56],
    /// Reserved for kernel use (128 bytes at offset 128..256).
    pub sem: [u8; 128],
}

/// Offset of the `slot[]` array from the ring start == header size.
///
/// **256** — live-verified via `cc sizeof(struct netmap_ring)` on FreeBSD
/// 15.1 amd64: `sem` carries `aligned(64)`, pushing the struct size from
/// the 200 field-sum to 256.
pub const NETMAP_RING_HEADER_SIZE: usize = core::mem::size_of::<NetmapRingHeader>();

/// FreeBSD `<sys/ioccom.h>`:
/// `_IOWR(g, n, t) = IOC_INOUT | ((sizeof(t) & IOCPARM_MASK) << 16) | (g << 8) | n`
/// with `IOC_INOUT = 0xc000_0000` and `IOCPARM_MASK = 0x1fff` — direction
/// occupies the top nibble directly (unlike Linux's `dir << 30`).
const fn iowr(group: u8, num: u8, len: usize) -> u64 {
    const IOC_INOUT: u64 = 0xc000_0000;
    const IOCPARM_MASK: usize = 0x1fff;
    IOC_INOUT | (((len & IOCPARM_MASK) as u64) << 16) | ((group as u64) << 8) | (num as u64)
}

/// FreeBSD `_IO(g, n) = IOC_VOID | (g << 8) | n` with `IOC_VOID = 0x2000_0000`
/// (no size field — the kernel ignores the argument pointer entirely).
const fn ioc_void(group: u8, num: u8) -> u64 {
    const IOC_VOID: u64 = 0x2000_0000;
    IOC_VOID | ((group as u64) << 8) | (num as u64)
}

/// `NIOCGINFO _IOWR('i', 145, struct nmreq)` — derived from
/// `size_of::<Nmreq>()` (60) so the constant and the struct cannot drift:
/// `IOC_INOUT(0xC000_0000) | (60<<16) | ('i'<<8) | 145` = `0xC03C_6991`.
/// Proven at runtime on FreeBSD 15.1 by the `netmap_probe` example.
pub const NIOCGINFO: u64 = iowr(b'i', 145, core::mem::size_of::<Nmreq>());

/// `NIOCREGIF _IOWR('i', 146, struct nmreq)` — same derivation as
/// [`NIOCGINFO`], request number 146: `0xC03C_6992`.
pub const NIOCREGIF: u64 = iowr(b'i', 146, core::mem::size_of::<Nmreq>());

/// `NIOCTXSYNC _IO('i', 148)` = `0x2000_6994`.
///
/// NOTE(netmap-fixup): FreeBSD declares the sync ioctls with `_IO` (void,
/// argument ignored), *not* `_IOW('i', 148, int)` — the kernel switches on
/// the full command word, so a size-bearing encoding would return `ENOTTY`.
/// Derived from `net/netmap.h`; if FreeBSD verification disagrees, re-check
/// with `cc -E` on the target box.
pub const NIOCTXSYNC: u64 = ioc_void(b'i', 148);

/// `NIOCRXSYNC _IO('i', 149)` = `0x2000_6995` — see [`NIOCTXSYNC`].
pub const NIOCRXSYNC: u64 = ioc_void(b'i', 149);

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact on-wire slot layout from `net/netmap.h`. If this ever
    /// changes the ring pointer arithmetic in the backend is wrong.
    #[test]
    fn slot_layout_is_16_bytes() {
        assert_eq!(core::mem::size_of::<NetmapSlot>(), 16);
        assert_eq!(core::mem::align_of::<NetmapSlot>(), 8);
        assert_eq!(core::mem::offset_of!(NetmapSlot, buf_idx), 0);
        assert_eq!(core::mem::offset_of!(NetmapSlot, len), 4);
        assert_eq!(core::mem::offset_of!(NetmapSlot, flags), 6);
        assert_eq!(core::mem::offset_of!(NetmapSlot, ptr), 8);
    }

    /// The 60-byte legacy `struct nmreq` accepted by FreeBSD 15.1 (runtime-
    /// proven by the probe). NIOCGINFO/NIOCREGIF encode this size.
    #[test]
    fn nmreq_layout_is_60_bytes() {
        assert_eq!(core::mem::size_of::<Nmreq>(), 60);
        assert_eq!(core::mem::align_of::<Nmreq>(), 4);
        assert_eq!(core::mem::offset_of!(Nmreq, nr_version), 16);
        assert_eq!(core::mem::offset_of!(Nmreq, nr_offset), 20);
        assert_eq!(core::mem::offset_of!(Nmreq, nr_memsize), 24);
        assert_eq!(core::mem::offset_of!(Nmreq, nr_tx_rings), 36);
        assert_eq!(core::mem::offset_of!(Nmreq, nr_ringid), 40);
        assert_eq!(core::mem::offset_of!(Nmreq, nr_arg3), 48);
        assert_eq!(core::mem::offset_of!(Nmreq, nr_flags), 52);
    }

    /// `struct netmap_if` fixed part is 56 bytes and `ring_ofs[]` starts
    /// right after it (already 8-aligned).
    #[test]
    fn netmap_if_fixed_part_is_56_bytes() {
        assert_eq!(core::mem::size_of::<NetmapIf>(), 56);
        assert_eq!(NETMAP_IF_RING_OFS_OFFSET, 56);
        assert_eq!(core::mem::offset_of!(NetmapIf, ni_tx_rings), 24);
        assert_eq!(core::mem::offset_of!(NetmapIf, ni_host_tx_rings), 36);
    }

    /// The ring header must end exactly where `slot[0]` begins: **256 bytes**
    /// on x86_64 — the 4-byte pad before `ts`, then the 56-byte pad that
    /// 64-aligns `sem` (C: `__attribute__((__aligned__(NM_CACHE_ALIGN)))`),
    /// then the 128-byte `sem`. Live-verified `sizeof(struct netmap_ring)
    /// == 256` via `cc` on FreeBSD 15.1 amd64.
    #[test]
    fn ring_header_is_256_bytes_with_slots_after() {
        assert_eq!(core::mem::size_of::<NetmapRingHeader>(), 256);
        assert_eq!(NETMAP_RING_HEADER_SIZE, 256);
        assert_eq!(core::mem::align_of::<NetmapRingHeader>(), 8);
        assert_eq!(core::mem::offset_of!(NetmapRingHeader, buf_ofs), 0);
        assert_eq!(core::mem::offset_of!(NetmapRingHeader, num_slots), 8);
        assert_eq!(core::mem::offset_of!(NetmapRingHeader, head), 20);
        assert_eq!(core::mem::offset_of!(NetmapRingHeader, cur), 24);
        assert_eq!(core::mem::offset_of!(NetmapRingHeader, tail), 28);
        assert_eq!(core::mem::offset_of!(NetmapRingHeader, ts), 40);
        assert_eq!(core::mem::offset_of!(NetmapRingHeader, sem), 128);
        assert_eq!(core::mem::size_of::<NetmapTimeval>(), 16);
    }

    /// Snapshot of every ioctl number against the FreeBSD 15.1 headers so a
    /// drifted `iowr`/`ioc_void` derivation fails loudly, not at runtime.
    #[test]
    fn ioctl_constants_snapshot() {
        assert_eq!(NIOCGINFO, 0xC03C_6991);
        assert_eq!(NIOCREGIF, 0xC03C_6992);
        // _IO('i', 148/149) — void ioctls, no size nibble (see NOTE on const).
        assert_eq!(NIOCTXSYNC, 0x2000_6994);
        assert_eq!(NIOCRXSYNC, 0x2000_6995);
    }

    /// RX ring 0 sits after all TX rings in the `ring_ofs[]` table.
    #[test]
    fn rx_ring0_index_skips_tx_and_host_tx_rings() {
        assert_eq!(rx_ring0_index(1, 0), 1); // vale1:1: 1 tx, 0 host tx, 1 rx
        assert_eq!(rx_ring0_index(4, 1), 5); // vmx0: 4 tx + 1 host tx
    }
}
