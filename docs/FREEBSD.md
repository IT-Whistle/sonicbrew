# FreeBSD Technology Integration

> **Document type:** Design document on how sonicbrew consumes FreeBSD kernel
> technologies — netmap(4) zero-copy packet I/O, OSS `/dev/dsp` audio output,
> kqueue(2) event polling — plus the deployment-era plan for ZFS, jails and
> capsicum, and the dedicated-machine regression story that keeps all of it
> verified. Claims about kernel behavior below are **live-verified** on the
> FreeBSD 15.1-RELEASE-p2 (amd64) test machine unless explicitly marked
> otherwise; §5 is entirely FUTURE work.
>
> **Baseline:** 2026-08-18 · netmap API 14 · 573/573 native tests GREEN.
> Related: [ARCHITECTURE](./ARCHITECTURE.md) · [PROGRESS](./PROGRESS.md) §6 ·
> [TEST-LAYERS](./TEST-LAYERS.md) · [KNOWLEDGE](./KNOWLEDGE.md).

---

## 1. Why FreeBSD — Strategy

sonicbrew is a **FreeBSD-based** high-performance distributed audio server
([ARCHITECTURE](./ARCHITECTURE.md) §1). FreeBSD is not merely a deployment
target among equals: the kernel is treated as a component of the audio path,
programmed directly where it wins (netmap), and hidden behind portable
abstractions everywhere else.

### 1.1 The P01 "integration layer gap" thesis

The founding survey (P01, *FreeBSD audio ecosystem*) reached the conclusion
that shaped the whole project: FreeBSD's audio "weakness" is **not a
single-component fault but the absence of an integration layer**. The
individual components — the `snd(4)`/`pcm(4)` kernel stack, `sndio`,
`virtual_oss`, PulseAudio's OSS backend, JACK — are each sound in themselves,
but there is no coordinating daemon, session manager, or hotplug consumer
tying them together. That gap is the opportunity sonicbrew fills: not "another
DSP toolkit", but the missing integration layer on top of a kernel that
already ships world-class primitives.

### 1.2 The kernel primitives we build on

| Technology | What it gives sonicbrew | Layer | Status |
|---|---|---|---|
| **netmap(4) / VALE** | Zero-copy RTP transport: shared-memory rings + in-kernel software switch | `net-rtp-aes67` | verified live (loopback 8/8) |
| **OSS `/dev/dsp`** | Local audio output through `snd(4)`/`pcm(4)`/`snd_hda(4)` | `audio-io-bsd` (upstream toolkit, cpal gate) | blocked on audio hardware in the VM |
| **kqueue(2)** | Single-syscall multiplexing of timers + fd readiness for the monitor | `monitor` | design intent; gated stub shipped |
| **jail(8)** | Per-module privilege separation at deployment time | ops | FUTURE (§5) |
| **ZFS** | Snapshot-based preset/state backup | ops | FUTURE (§5) |
| **capsicum(4)** | Capability sandbox for the control plane | ops | FUTURE (§5) |

The division of labor: **netmap is the flagship** — the one kernel technology
sonicbrew programs byte-for-byte at the ABI level (§2) — while OSS and kqueue
are consumed through existing Rust layers, and jails/ZFS/capsicum are
operational wrappers applied when scaling out.

---

## 2. netmap(4): Zero-Copy Packet I/O

This is the flagship FreeBSD integration: the `net-rtp-aes67` crate's
`netmap` feature moves RTP audio through kernel shared-memory rings with no
socket copies at all.

### 2.1 What netmap is

netmap(4) is FreeBSD's framework for **kernel-bypass packet I/O**. Instead of
the socket path — where every `send(2)`/`recv(2)` crosses the syscall boundary
and copies the payload between user buffers and kernel mbufs — netmap exposes
each NIC (or virtual port) as:

- a set of **rings** (TX and RX), each an array of fixed-size **slots**,
- plus a pool of fixed-size **buffers** the slots point into,
- all mapped into user space via `mmap`, shared directly with the kernel.

Userspace writes a packet into the next free TX slot's buffer, updates two
ring cursor words, and issues one `NIOCTXSYNC` ioctl; the kernel (or, for a
NIC, the hardware DMA engine) transmits straight from that buffer. Reception
is symmetric: after `NIOCRXSYNC` the freshly arrived packets are already
visible in user space. The syscall count drops from one-per-packet to
one-per-batch, and the copies drop to zero. For an audio transport pushing
small RTP packets on a hard real-time deadline, both properties are exactly
what is needed.

The cost is that *we* become responsible for everything the socket layer used
to do: ring accounting, buffer lifetimes, and — for the VALE software switch —
the switching setup itself. Sections 2.3–2.6 document the ABI and the three
behaviors that had to be discovered experimentally before any packet flowed.

### 2.2 The wire protocol we implement

The userspace side of netmap is a tiny protocol over one file descriptor
(`netmap_backend.rs`, `NetmapTransport::open_with_memid`):

1. `open("/dev/netmap", O_RDWR)` — one fd for every port registration.
2. `ioctl(NIOCGINFO)` — capability probe: fill `nmreq.nr_name` +
   `nr_version = 14`; the kernel answers its own API version and the port's
   ring/slot geometry **without registering** anything.
3. `ioctl(NIOCREGIF)` — register the port: kernel returns `nr_offset`
   (offset of the `netmap_if` descriptor) and `nr_memsize` (region size), and
   assigns the memory allocator id in `nr_arg2` (the *memid* — §2.5 secret 1).
4. `mmap(nr_memsize, MAP_SHARED)` on the same fd — the shared region.
5. Walk `netmap_if` → `ring_ofs[]` → ring headers → slot arrays; buffer `i`
   of a ring lives at `ring_start + buf_ofs + i * nr_buf_size`.
6. `ioctl(NIOCTXSYNC)` / `ioctl(NIOCRXSYNC)` to hand off / fetch batches.
7. Dropping the fd unregisters the port (the backend's `Drop` also unmaps).

All kernel-shared words (`head`/`cur`/`tail`, slot descriptors) are accessed
with volatile reads/writes because the kernel writes them concurrently between
syncs. The transport is `Send` (single worker thread ownership) and
deliberately **not** `Sync`.

### 2.3 The ABI, live-verified on FreeBSD 15.1

The Linux dev host has no `/usr/include/net/netmap.h`, so
`crates/net-rtp-aes67/src/transport/netmap_layout.rs` re-declares every type
from the FreeBSD 15.1 headers (`net/netmap.h` + `net/netmap_legacy.h`) as
dependency-free `repr(C)` items. The module carries **no cargo feature**, so
its layout snapshot tests run in the default suite on every host — the
constants cannot drift silently. Everything below was additionally proven at
runtime by the `netmap_probe` example (NIOCGINFO answered API 14).

**Legacy request struct** — `struct nmreq`, 60 bytes, align 4
(`nmreq_layout_is_60_bytes`):

| Offset | Field | Type | Role |
|---:|---|---|---|
| 0 | `nr_name` | `[u8; 16]` | in: port name (`"vmx0"`, `"vale60:w"`) |
| 16 | `nr_version` | `u32` | in: requested API; out: kernel version |
| 20 | `nr_offset` | `u32` | out: `netmap_if` offset in the region |
| 24 | `nr_memsize` | `u32` | out: region size (bytes) |
| 28 | `nr_tx_slots` | `u32` | out: slots per TX ring |
| 32 | `nr_rx_slots` | `u32` | out: slots per RX ring |
| 36 | `nr_tx_rings` | `u16` | out: TX ring count |
| 38 | `nr_rx_rings` | `u16` | out: RX ring count |
| 40 | `nr_ringid` | `u16` | in: ring selection for NIOCREGIF |
| 42 | `nr_cmd` | `u16` | in: bridge/registration sub-command |
| 44 | `nr_arg1` | `u16` | in: spare |
| 46 | `nr_arg2` | `u16` | in/out: **memory allocator id (memid)** |
| 48 | `nr_arg3` | `u32` | in: extra buffers request |
| 52 | `nr_flags` | `u32` | in: `NR_REG_*` registration flags |
| 56 | `nr_spare2` | `[u32; 1]` | reserved |

**Slot descriptor** — `struct netmap_slot`, 16 bytes, align 8:

| Offset | Field | Type | Who writes |
|---:|---|---|---|
| 0 | `buf_idx` | `u32` | kernel (buffer index into the ring's pool) |
| 4 | `len` | `u16` | TX: user; RX: kernel |
| 6 | `flags` | `u16` | kernel (`NS_*`) |
| 8 | `ptr` | `u64` | reserved |

**Port descriptor** — `struct netmap_if` fixed part is 56 bytes (16-byte name
+ seven `u32` fields + three `u32` spares); the variable-length `ring_ofs[]`
table of `ssize_t` entries starts at offset 56, already 8-aligned. Table
order: **all TX rings first (NIC TX, then host TX), then all RX rings (NIC RX,
then host RX)** — RX ring 0 sits at index `ni_tx_rings + ni_host_tx_rings`
(`rx_ring0_index`). Verified geometries: `vmx0` 4 TX + 4 RX rings × 512
slots (plus a host TX pair), ephemeral `vale1:1` a single ring pair × 1024
slots.

**Ring header** — `struct netmap_ring` fixed header is **256 bytes** on
amd64, live-verified with `cc sizeof(struct netmap_ring)` on the FreeBSD box.
The naive field sum is 200; the C header's `sem` member carries
`__attribute__((__aligned__(NM_CACHE_ALIGN)))` (64), which pads the struct to
256. That header size is also the offset of `slot[0]` — get it wrong and
every slot access lands in garbage:

| Offset | Field | Type |
|---:|---|---|
| 0 | `buf_ofs` | `i64` (buffer 0 offset from ring start) |
| 8 | `num_slots` | `u32` |
| 12 | `nr_buf_size` | `u32` |
| 16 | `ringid` | `u16` |
| 18 | `dir` | `u16` (nominally 0 = TX, 1 = RX — but see §2.6) |
| 20 | `head` | `u32` (kernel reads: release point / first free) |
| 24 | `cur` | `u32` (user cursor between `head` and `tail`) |
| 28 | `tail` | `u32` (kernel writes: watermark) |
| 32 | `flags` | `u32` |
| 36 | pad | 4 B |
| 40 | `ts` | `struct timeval` (2 × `i64` on 64-bit) |
| 56 | `offset_mask` | `u64` |
| 64 | `buf_align` | `u64` |
| 72 | pad | 56 B (so `sem` starts 64-aligned at 128) |
| 128 | `sem[128]` | kernel-reserved |

**Ioctl number derivation.** FreeBSD `<sys/ioccom.h>` puts the direction in
the top nibble directly (unlike Linux's `dir << 30`), and the sync ioctls are
declared `_IO` — void, **no size bits at all**; a size-bearing encoding makes
the kernel return `ENOTTY` because it switches on the full command word:

```c
// _IOWR(g, n, t) = IOC_INOUT | ((sizeof(t) & 0x1fff) << 16) | (g << 8) | n
// _IO(g, n)      = IOC_VOID  | (g << 8) | n          // argument ignored
// IOC_INOUT = 0xc0000000, IOC_VOID = 0x20000000, group 'i' = 0x69

NIOCGINFO  = _IOWR('i', 145, struct nmreq)  // 0xC03C6991 = INOUT | 60<<16 | 'i'<<8 | 145
NIOCREGIF  = _IOWR('i', 146, struct nmreq)  // 0xC03C6992
NIOCTXSYNC = _IO('i', 148)                  // 0x20006994 (void: no size nibble)
NIOCRXSYNC = _IO('i', 149)                  // 0x20006995
```

In Rust the `_IOWR` constants are derived from `size_of::<Nmreq>()` itself, so
the struct and the encoded ioctl number cannot drift apart; a snapshot test
pins all four values. Version negotiation: the crate speaks **API 14**
(accepted range 11..=14); the kernel overwrites `nr_version` with its own
value, which the probe reports — the dedicated machine answered `kernel 14`.
One hardening kept from the §6 incident postmortem: the sync ioctls pass `0`
as the third argument, never a null pointer, matching C callers
byte-for-byte.

### 2.4 VALE: the in-kernel software switch

VALE is netmap's built-in **software switch**: an arbitrary number of virtual
ports, created on demand, that the kernel bridges between each other at
native speed. sonicbrew uses it for two purposes:

- **Test topology without hardware** — any number of loopback/relay paths
  inside one machine (the verified loopback of §2.8 runs entirely through a
  VALE switch; no NIC is involved).
- **Future fan-out** — a VALE port attached to a real NIC port bridges
  zero-copy userspace traffic onto the wire.

The naming scheme is `valeN:PPP` — switch `valeN`, port `PPP` — and ports are
**ephemeral**: registering `vale60:r` with NIOCREGIF creates the switch and
the port on the spot, and closing the fd tears both down. The switch bridges
semantics are what you would expect: a packet sent on any port's send ring is
delivered to every other port's delivery ring (subject to §2.5 secret 1).

### 2.5 The three secrets of VALE switching

Three behaviors are documented nowhere we could find, and each had to be
unlocked by systematic C-matrix experimentation (a set of small C reference
programs run against the same switch, varying one parameter at a time — the
`vale_diag` example is the surviving Rust diagnostic from that campaign).
They are now encoded in the backend API; each subsection states the failure
mode as we first observed it, the root cause, and the fix.

#### Secret 1 — memid/allocator pinning

- **Failure mode:** sender txsyncs 8 packets, receiver rxsyncs forever —
  **zero deliveries**, no error anywhere.
- **Root cause:** the kernel assigns **different memory allocators to
  independently registered ports**. Live-observed: reader got memid 2, writer
  got memid 3. VALE only switches between ports that share a memory
  allocator; with different memids the packets silently go nowhere.
- **Fix:** all ports of one switch must be pinned to the **first port's**
  `nr_arg2`. Open the receiver *first* (it creates the switch and owns the
  memid), read `NetmapTransport::memid()`, then open every peer via
  `open_with_memid(name, Some(first_memid))`.

#### Secret 2 — fresh-ring priming

- **Failure mode:** every poll on a freshly opened delivery ring "receives"
  packets — garbage lengths, garbage payload — with no traffic sent at all.
- **Root cause:** a fresh VALE RX/delivery ring's slot descriptors are
  **stale garbage** (they describe buffers of previous mappings) until the
  kernel sees them acknowledged. Unacknowledged, every sync misreads the
  stale slots as deliveries. On the 1024-slot rings we used, a fresh port's
  `tail` starts at `num_slots − 1` pointing at that garbage.
- **Fix:** `NetmapTransport::prime_rx()` — set `head = cur = tail`, then one
  `rxsync` — exactly once after `open`, before the first receive. Both sides
  of the loopback prime their rings before any traffic exists.

#### Secret 3 — TX slot accounting

- **Failure mode:** an early "4/8 deliveries" result that looked like the
  switch dropping half the packets. It was an accounting artifact: the test
  misread the post-sync ring state, stopped sending early, and blamed the
  kernel.
- **Root cause:** after `NIOCTXSYNC` the kernel reports `tail` = **the slot
  after the last one it consumed** — `head = 1, tail = 0` means *1 slot in
  flight*, not "ring full". Free slots are
  `num_slots − (head − tail mod num_slots)`. `head == tail` is ambiguous —
  a used ring at that state is truly full, a fresh ring (`head = tail = 0`)
  is empty — so `Ring::slots_free` treats 0 as "sync and retry", which
  self-corrects on the next poll. Live-verified on the vale57/60 runs.
- **Companion rule:** `cur` must follow `head` on TX — `cur` is the kernel's
  wakeup point. On `vale6:b`, 8 staged packets with `cur` left behind left
  `tail` frozen at 1023 while the kernel consumed nothing; advancing `cur`
  with every `head` update makes the kernel drain the ring. The earlier
  C-matrix "4/8" artifact traced back to the same misunderstanding.

### 2.6 Ring roles on a VALE port

A netmap port's `ring_ofs[]` table orders rings NIC-TX, host-TX, NIC-RX,
host-RX. For a **VALE port** the roles are — live-verified via each ring
header's `ringid`/`dir` fields (`debug_state`):

| Role | Location | `dir` field | Direction of packets |
|---|---|---|---|
| **SEND ring** | `ring_ofs[0]` | 1 | user writes, switch reads |
| **DELIVERY ring** | `ring_ofs[ni_tx_rings + ni_host_tx_rings]` | 0 | switch writes, user reads |

Note the inversion: the `dir` field nominally means 0 = TX / 1 = RX, but on a
VALE port the send ring at `ofs[0]` carries `dir = 1` and the delivery ring
carries `dir = 0` — do not trust the field name, trust the live values.
Host rings (which mediate traffic with the host IP stack) sit between the NIC
TX and RX entries in the table; the ephemeral VALE ports we measured report
no host rings of their own (`vale1:1`: 1 TX + 0 host TX + 1 RX; the backend's
verified comment records `tx + host_tx = 2` on the vale ports used in the
loopback runs; `vmx0`: 4 TX + 1 host TX puts RX 0 at index 5).

One operational consequence: the sync ioctls operate on *rings by role*, so
calling `NIOCRXSYNC` on the **sender's** fd would drain its **send** ring
(`ofs[0]` carries `dir = 1`) and stall subsequent sends. `send_rtp` therefore
issues TX syncs only.

### 2.7 RTP framing on the rings

`NetmapTransport` implements the crate's `NetworkAudioTransport` trait on top
of the raw ring API (`netmap_backend.rs`):

```text
send_rtp(payload, codec):
  RtpPacket::encode (RFC 3550 header: pt, seq++, timestamp += advance, ssrc)
    -> send_raw: length checks (<= u16::MAX, <= nr_buf_size, slot free)
                 copy into slot[head]'s buffer, slot.len = n,
                 head = cur = head+1  (cur must follow head: secret 3)
    -> txsync (NIOCTXSYNC): one sync per packet; callers that prefer
       batching use send_raw + a single txsync directly

recv_rtp():
  rxsync (NIOCRXSYNC)  -> kernel fills free slots, updates tail
  recv_raw: while cur != tail: read slot[cur].len/buf_idx,
            copy out, cur = head = cur+1  (release back to kernel)
  RtpPacket::parse     -> non-RTP traffic surfaces as a parse error,
                          exactly like a UDP socket receiving a stray datagram
```

`join_multicast` is `Unimplemented` **by design**: a netmap port is not an IP
socket; multicast membership belongs to the external network path, not to the
ring. Current scope: one TX/RX ring pair per port (TX 0 / RX 0) — the
complete port for VALE, which has a single pair; a ring-set API for
multi-queue NICs is future work.

### 2.8 The verified loopback (vale60)

`crates/net-rtp-aes67/examples/vale_loopback.rs` is the end-to-end proof —
regression suite section 8 (`LOOPBACK_PASS` required):

1. open the receiver `vale60:r` first, read its kernel-assigned memid;
2. `prime_rx()` on the receiver (secret 2);
3. open the sender `vale60:w` pinned to that memid (secret 1);
4. `send_rtp` × 8 — 48-frame L16 stereo payload with a recognizable pattern
   (`i % 256` and its complement per frame);
5. poll `recv_rtp` on the receiver (5 s deadline).

Result on the dedicated machine (2026-08-18): **8/8 packets received with
bit-exact payload** — the complete zero-copy path (RTP encode → TX slot →
kernel VALE switch → RX slot → RTP parse) live-verified.

### 2.9 What remains

- **NIC ports / multi-queue**: the backend exposes ring pair 0 only; real
  `vmx0` traffic with RSS queues needs the ring-set API.
- **Real multicast**: AES67-style multicast over netmap needs an external
  path design (the ring is not an IP socket).
- **Audio worker integration**: today the loopback is example-level; the
  next step is wiring `send_rtp`/`recv_rtp` into the recv/send worker loops
  the same way `UdpTransport` is wired, behind the RT graph.

---

## 3. OSS `/dev/dsp` Audio Output

### 3.1 The /dev/dsp model

FreeBSD's audio output stack is the OSS model: `snd(4)`/`pcm(4)` provide the
framework, hardware drivers like `snd_hda(4)` plug into it, and applications
see plain character devices `/dev/dsp*` — `open`, `write` interleaved PCM,
`ioctl` for format/rate/channels. No daemon sits in the way; the mixing that
Linux does in PulseAudio/PipeWire happens in-kernel via `snd(4)`'s virtual
channels. This simplicity is why `audio-io-bsd` (part of
audio-toolkit) treats OSS as the native local-output path.

### 3.2 Why the test VM cannot exercise it

The dedicated regression machine is a VM **without an audio PCI device**:
there is no `snd_hda(4)`-attached hardware, hence no `/dev/dsp` to open. Real
output (p11 §8.4 demo + latency measurement) is deferred until hardware —
physical or passed-through — exists. Until then the `audio-io-bsd` cpal gate
stays closed on that machine, and playback-adjacent paths are verified at the
layers above (gw-pulse `--play` streams 144,000 frames to a real PulseAudio
17.0 daemon over the Unix socket; gw-alsa streams 24,000 f32 frames
end-to-end through `aplay`).

### 3.3 The cpal gate and the ALSA detour

`cpal` has **no FreeBSD OSS backend**. On FreeBSD it reaches the hardware via
its ALSA backend — and FreeBSD's ALSA (`audio/alsa-lib`) cannot touch
hardware directly either; it runs as an emulation layer over OSS. The chain
works but adds latency and emulation limits (no direct hardware access,
weaker concurrent-playback and hotplug behavior). The performance budget
([PERFORMANCE](./PERFORMANCE.md)): local playback target < 5 ms theoretical
(256-frame buffer at 48 kHz = 5.3 ms), with a measured < 10 ms allowance
while the via-ALSA overhead is tolerated.

### 3.4 Decision #5: the direct-OSS escape hatch

decision point (revisit after benchmarks): if measured
cpal-via-ALSA latency exceeds 10 ms, sonicbrew builds a **direct OSS
backend** — open `/dev/dsp` itself and drop the emulation detour. The
workspace already contains the pattern to follow: ADR-0002's Bluetooth input
reads a virtual OSS device (`/dev/dspBT`) with raw OSS reads in a worker,
converts, and feeds an rtrb ring — the same shape a direct output backend
would take, minus the virtual device.

---

## 4. kqueue(2) Event Polling

### 4.1 Design intent

FreeBSD's `kqueue(2)`/`kevent(2)` is the event-notification facility the
monitor module is designed around long-term: a single syscall that
multiplexes **timers and fd readiness** — one kqueue could drive the metrics
sampling timer and watch the audio-device fds together, replacing a poll
thread per interest. The module docs (`crates/monitor/src/lib.rs`, "kqueue
note") record this as a deliberate FreeBSD-only optimization for a later
phase.

### 4.2 Current state

What ships today is the portable core: `MetricsRecorder` (sliding-window
latency p50/p99, xrun counters, RT-safe O(1) locked push) plus `serve_metrics`
— a raw-HTTP `GET /metrics` endpoint exposing Prometheus text (default
`--metrics-addr 127.0.0.1:9003`). The kqueue path exists as a **gated stub**,
`spawn_kqueue_loop()`, which returns an explicit "not implemented yet"
error, and the crate carries **no `kqueue`/`nix` dependency** — those crates
do not build on the Linux dev host and would break the compile-anywhere
discipline (§7). Notably, p11's kernel-dependency table lists kqueue as *the
only* FreeBSD kernel dependency the MVP actually requires —
everything heavier was pushed to the scaling-out phase.

---

## 5. ZFS / Jails / Capsicum — Deployment Era (FUTURE)

> Everything in this section is **FUTURE** work, activated when sonicbrew
> scales out of the single-host MVP. During the MVP the binary is statically
> linked and runs without any of these facilities (KNOWLEDGE §8.4).

### 5.1 jails(8) — per-module privilege separation

The distributed design (P04 §6.4) isolates each sonicbrew module (session
consensus, RTP transport, control API) in its own jail: one compromised
gateway cannot reach the others' memory or credentials. GOVERNANCE assigns
jail/bhyve/carp infrastructure to the DevOps Lead role, keeps a staging tier
that is jail-isolated as a production mirror, and puts "netmap/Capsicum/jail
kernel feature dependency change" on the sign-off list. A second, earlier
use: KNOWLEDGE notes the libpulse LGPL dynamic-linking obligation is planned
to be contained by isolating the gateway in a separate .so/jail, preserving
the Rust-only core's licensing simplicity.

### 5.2 ZFS — snapshot state backup

The MVP persists topology in the redb WAL plus a JSON preset sidecar, on
local files; p11 explicitly scoped object storage out ("local files + ZFS
only" — MinIO deferred). The deployment-era plan is to put the state
directory on a ZFS dataset and snapshot it: preset/ WAL backup becomes an
atomic, space-efficient `zfs snapshot` instead of file-copying live state.
No code exists for this yet.

### 5.3 capsicum(4) — control-plane sandbox

The control API's authorization design (distilled in KNOWLEDGE)
pairs mTLS with Capsicum restriction: after startup the API process drops
into capability mode so that even a compromised handler cannot open arbitrary
paths or sockets. This is the deepest of the three integrations (it requires
auditing every fd the process holds at the restriction point) and is firmly
post-MVP.

---

## 6. Test Infrastructure: The Dedicated FreeBSD Machine

### 6.1 The machine

A dedicated physical-access test machine runs FreeBSD **15.1-RELEASE-p2,
amd64** (rust 1.96.1), with the source tree at `/root/sonicbrew`. It is the
only place the FreeBSD-specific layers of this document actually execute:
native build, native tests, live PulseAudio, and — the parts no other host
can run — `/dev/netmap` ring traffic.

### 6.2 The 8-section regression suite

`scripts/freebsd-regression.sh` (one-shot, exit code = failed-section count):

| § | Section | Gate |
|---|---|---|
| 1 | fmt + clippy | `cargo fmt --check`, `clippy -D warnings` |
| 2 | native build | `cargo build --workspace` |
| 3 | workspace tests | all green, passed > 400 |
| 4 | 5 binary self-tests | self/hot-reload/live-rebuild/engine/gateway |
| 5 | server-engine smoke | REST live reload + preset autosave **and restore after kill/restart** (waits out the redb lock release) |
| 6 | gw-pulse handshake | live PulseAudio daemon, `handshake OK` |
| 7 | netmap ring I/O | probe (API "requested 14, kernel 14") + `nm_port_test` on `vale1:1` → `PORT_TEST_PASS` |
| 8 | netmap RTP loopback | `vale_loopback` → `LOOPBACK_PASS` |

Sections 7–8 skip cleanly when `/dev/netmap` is absent. Result at
2026-08-18: **ALL GREEN, 573/573 tests** — TEST-LAYERS Layer 4 (FreeBSD
regression) for the whole workspace.

### 6.3 Nightly automation

`scripts/nightly-regression.sh` wraps the suite for **cron at 03:30**: full
run into `logs/regression-<timestamp>.log`, 14-day retention pruning, and a
`logs/LAST_FAILED` marker file written on failure (removed on success) so a
quick `test -e` answers "did last night break".

### 6.4 The VPN incident postmortem (resolved — not a panic)

During the VALE debugging campaign (2026-08-18) the machine became
"unreachable" mid-experiment, right when the wildest kernel-shared-memory
probing was underway. Postmortem after it returned (new DHCP lease,
192.168.62.107): `/var/crash` empty, no panic lines in `/var/log/messages`,
and the full regression suite passed unchanged — the outage was a
**client-side VPN dropout**, and the netmap experiments were never the
cause. One hardening was kept anyway: the sync ioctls now pass `0` rather
than a null pointer as the third argument, matching C callers exactly
(§2.3).

---

## 7. Cross-Platform Strategy: Linux Dev, FreeBSD Target

### 7.1 The constraint

The development host is Linux x86_64 without `/usr/include/net/netmap.h`,
libasound, libopus, or a usable cpal — and it must stay able to build, lint
(`clippy -D warnings`), and run the full unit/integration suite, because that
is where day-to-day development happens.

### 7.2 The mechanism: compile-anywhere, run-on-FreeBSD

- **`cfg` / feature gates** for every FreeBSD-only runtime: netmap lives
  behind `cfg(all(unix, feature = "netmap"))` — the backend *compiles* on
  every Unix and is only *runnable* where `/dev/netmap` exists; the ABI
  declarations (`netmap_layout.rs`) carry no feature at all, so their layout
  snapshot tests run in the default suite on every host. Bluetooth is behind
  the `bluetooth` feature, Opus behind `opus`, libpulse/libasound FFI behind
  their own gates.
- **No Linux-incompatible dependencies**: the monitor deliberately ships
  without `kqueue`/`nix` crates (§4).
- **Typecheck the target** from the dev host:
  `cargo check --workspace --target x86_64-unknown-freebsd`.
- **Guard tests instead of skipped tests**: opening `vale1:1` on a host
  without `/dev/netmap` must fail cleanly with a `Network` error (tested),
  over-long interface names are rejected before the device is even opened
  (tested against `IFNAMSIZ`), and a detached transport rejects every raw op
  (tested). The FreeBSD-only *success* paths then run on the dedicated
  machine as Layer 4 (§6).

The discipline in one sentence: **every line of FreeBSD-specific code is
written, linted, and layout-tested on Linux; only its execution is delegated
to the FreeBSD regression machine.**

---

**Related documents:** [ARCHITECTURE](./ARCHITECTURE.md) ·
[PROGRESS](./PROGRESS.md) §6 (verified FreeBSD achievements) ·
[TEST-LAYERS](./TEST-LAYERS.md) · [PERFORMANCE](./PERFORMANCE.md) ·
[KNOWLEDGE](./KNOWLEDGE.md) · [GOVERNANCE](./GOVERNANCE.md) ·
[RUNBOOK](./RUNBOOK.md)
