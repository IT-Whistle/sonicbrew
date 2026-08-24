//! M11 (P2) — the libasound PCM plugin `.so`: `libasound_module_pcm_sonicbrew.so`.
//!
//! This crate builds the C-ABI shared object that libasound `dlopen`s for
//! ALSA PCM definitions like
//!
//! ```text
//! pcm.sonicbrew {
//!     type sonicbrew
//!     socket "/tmp/sonicbrew.sock"   # or: server "10.0.0.4" [port 9001]
//!     channels 2
//!     rate 48000
//! }
//! ```
//!
//! It is the live `.so` half that [`gw_alsa`]'s crate docs deferred. All
//! behavior lives in the pure-Rust [`bridge`] module (local-socket f32
//! stream, unit-tested without libasound); this file owns only the ioplug C
//! ABI: the `_snd_pcm_sonicbrew_open` entry point, the callback table, and
//! `#[repr(C)]` mirrors of alsa-lib's plugin SDK types.
//!
//! # Structure layout
//!
//! `snd_pcm_ioplug_t` / `snd_pcm_ioplug_callback_t` are hand-mirrored from
//! `alsa/pcm_ioplug.h` — verified field-for-field against **both** alsa-lib
//! v1.2.13 and current master (the layout is frozen since protocol 1.0.2,
//! 2006; `SND_PCM_IOPLUG_VERSION = 0x010002`). The `abi_layout_matches_alsa`
//! tests pin every offset so any drift fails loudly. Full runtime validation
//! happens on the FreeBSD build host, which has alsa-lib installed.
//!
//! # Build modes
//!
//! * **alsa located** (pkg-config, Linux/FreeBSD): links `-lasound` — the
//!   `.so` carries a `DT_NEEDED` on libasound exactly like alsa-plugins'
//!   own modules, so `snd_*` symbols resolve when libasound `dlopen`s us.
//! * **`no_alsa_link`** (dev host without libasound2-dev): the extern
//!   declarations compile to logging stubs. The `.so` still builds and still
//!   exports `_snd_pcm_sonicbrew_open` (checked with `nm -D`); opening a PCM
//!   fails gracefully with a logged error instead of the module failing to
//!   load.
//!
//! # Semantics (MVP)
//!
//! * Connection is eager: `snd_pcm_open` itself fails (`-EIO`) when no
//!   bridge server answers — the pulse/jack plugin convention.
//! * HW constraints are pinned to the conf values: `SND_PCM_ACCESS_RW_-
//!   INTERLEAVED`, `SND_PCM_FORMAT_FLOAT_LE`, the conf `channels`/`rate`.
//! * `transfer` converts ALSA channel areas ↔ interleaved LE f32 wire bytes
//!   (explicit byte order, host-independent). `pointer` is the transferred
//!   frame counter modulo the negotiated buffer size.
//! * Only blocking-synchronous I/O; async (`SND_PCM_NONBLOCK`) apps still
//!   work but block inside transfer, as with alsa-plugins' file/pipe plugins.
//!
//! [`gw_alsa`]: https://crates.io/crates/gw-alsa

mod bridge;

use std::ffi::{c_char, c_int, c_long, c_uint, c_ushort, c_void, CStr};
use std::mem::offset_of;

use bridge::{BridgeStream, ConfValue, StreamDir};

// ---------------------------------------------------------------------------
// libasound ABI mirrors (verified against alsa-lib pcm_ioplug.h, see crate
// docs; every field below matches the C declaration order exactly).
// ---------------------------------------------------------------------------

/// Opaque `snd_pcm_t`.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct SndPcmT {
    _private: [u8; 0],
}

/// Opaque `snd_config_t`.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct SndConfigT {
    _private: [u8; 0],
}

/// Opaque `snd_pcm_hw_params_t` (accepted by the optional `hw_params`
/// callback; this plugin does not implement it).
#[repr(C)]
#[derive(Debug)]
pub(crate) struct SndPcmHwParamsT {
    _private: [u8; 0],
}

/// Opaque `snd_pcm_sw_params_t`.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct SndPcmSwParamsT {
    _private: [u8; 0],
}

/// Opaque `snd_output_t`.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct SndOutputT {
    _private: [u8; 0],
}

/// `snd_pcm_channel_area_t` — one channel's slice of the user's interleaved
/// buffer (`addr` = base pointer, `first`/`step` in bits).
#[repr(C)]
#[derive(Debug)]
pub(crate) struct SndPcmChannelArea {
    addr: u64,
    first: c_uint,
    step: c_uint,
}

/// `snd_pcm_sframes_t` (signed long).
type SndPcmSframes = i64;
/// `snd_pcm_uframes_t` (unsigned long).
type SndPcmUframes = u64;

/// `struct snd_pcm_ioplug_callback` — the ioplug callback table. Field order
/// and signatures mirror `pcm_ioplug.h` v1.2.13 == master (protocol 1.0.2).
#[repr(C)]
#[derive(Debug)]
pub(crate) struct SndPcmIoplugCallback {
    start: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> c_int>,
    stop: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> c_int>,
    pointer: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> SndPcmSframes>,
    transfer: Option<
        unsafe extern "C" fn(
            *mut SndPcmIoplug,
            *const SndPcmChannelArea,
            SndPcmUframes,
            SndPcmUframes,
        ) -> SndPcmSframes,
    >,
    close: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> c_int>,
    hw_params: Option<unsafe extern "C" fn(*mut SndPcmIoplug, *mut SndPcmHwParamsT) -> c_int>,
    hw_free: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> c_int>,
    sw_params: Option<unsafe extern "C" fn(*mut SndPcmIoplug, *mut SndPcmSwParamsT) -> c_int>,
    prepare: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> c_int>,
    drain: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> c_int>,
    pause: Option<unsafe extern "C" fn(*mut SndPcmIoplug, c_int) -> c_int>,
    resume: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> c_int>,
    poll_descriptors_count: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> c_int>,
    poll_descriptors: Option<unsafe extern "C" fn(*mut SndPcmIoplug, *mut c_void, c_uint) -> c_int>,
    poll_revents: Option<
        unsafe extern "C" fn(*mut SndPcmIoplug, *mut c_void, c_uint, *mut c_ushort) -> c_int,
    >,
    dump: Option<unsafe extern "C" fn(*mut SndPcmIoplug, *mut SndOutputT)>,
    delay: Option<unsafe extern "C" fn(*mut SndPcmIoplug, *mut SndPcmSframes) -> c_int>,
    query_chmaps: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> *mut *mut c_void>,
    get_chmap: Option<unsafe extern "C" fn(*mut SndPcmIoplug) -> *mut c_void>,
    set_chmap: Option<unsafe extern "C" fn(*mut SndPcmIoplug, *const c_void) -> c_int>,
}

/// `struct snd_pcm_ioplug` — the ioplug handle. Field order mirrors
/// `pcm_ioplug.h` v1.2.13 == master (protocol 1.0.2); offsets are pinned by
/// the `abi_layout_matches_alsa` test.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct SndPcmIoplug {
    version: c_uint,
    name: *const c_char,
    flags: c_uint,
    poll_fd: c_int,
    poll_events: c_uint,
    mmap_rw: c_uint,
    callback: *const SndPcmIoplugCallback,
    private_data: *mut c_void,
    pcm: *mut SndPcmT,
    stream: c_int,
    state: c_int,
    appl_ptr: SndPcmUframes,
    hw_ptr: SndPcmUframes,
    nonblock: c_int,
    access: c_int,
    format: c_int,
    channels: c_uint,
    rate: c_uint,
    period_size: SndPcmUframes,
    buffer_size: SndPcmUframes,
}

// ---------------------------------------------------------------------------
// Constants (values shared by Linux and FreeBSD alsa-lib).
// ---------------------------------------------------------------------------

/// `SND_PCM_IOPLUG_VERSION` = 1.0.2.
const SND_PCM_IOPLUG_VERSION: c_uint = 0x010_002;
/// `SND_PCM_IOPLUG_HW_ACCESS`.
const HW_ACCESS: c_int = 0;
/// `SND_PCM_IOPLUG_HW_FORMAT`.
const HW_FORMAT: c_int = 1;
/// `SND_PCM_IOPLUG_HW_CHANNELS`.
const HW_CHANNELS: c_int = 2;
/// `SND_PCM_IOPLUG_HW_RATE`.
const HW_RATE: c_int = 3;
/// `SND_PCM_ACCESS_RW_INTERLEAVED`.
const SND_PCM_ACCESS_RW_INTERLEAVED: c_uint = 3;
/// `SND_PCM_FORMAT_FLOAT_LE` (matches `gw_alsa::format::AlsaFormat`).
const SND_PCM_FORMAT_FLOAT_LE: c_uint = 14;
/// `SND_PCM_STREAM_PLAYBACK`.
const SND_PCM_STREAM_PLAYBACK: c_int = 0;
/// `SND_PCM_STREAM_CAPTURE`.
const SND_PCM_STREAM_CAPTURE: c_int = 1;
/// `poll(2)` event bits (identical on Linux and FreeBSD).
const POLLIN: c_uint = 0x001;
/// `poll(2)` `POLLOUT`.
const POLLOUT: c_uint = 0x004;
/// `errno` values used as negative ALSA return codes.
const EIO: c_int = 5;
/// `EINVAL`.
const EINVAL: c_int = 22;
/// `ENODEV`.
const ENODEV: c_int = 19;
/// `ENOENT` (snd_config_search "key absent").
const ENOENT: c_int = 2;

/// `io.name` for the PCM handle.
static PLUGIN_NAME: &CStr = c"sonicbrew local-socket bridge PCM";

// ---------------------------------------------------------------------------
// FFI: real symbols when libasound was located, logging stubs otherwise.
// ---------------------------------------------------------------------------

#[cfg(not(no_alsa_link))]
mod sys {
    use super::{SndConfigT, SndPcmIoplug};
    use std::ffi::{c_char, c_int, c_long, c_uint};

    unsafe extern "C" {
        pub fn snd_pcm_ioplug_create(
            io: *mut SndPcmIoplug,
            name: *const c_char,
            stream: c_int,
            mode: c_int,
        ) -> c_int;
        pub fn snd_pcm_ioplug_set_param_list(
            io: *mut SndPcmIoplug,
            type_: c_int,
            num_list: c_uint,
            list: *const c_uint,
        ) -> c_int;
        pub fn snd_config_search(
            config: *mut SndConfigT,
            key: *const c_char,
            result: *mut *mut SndConfigT,
        ) -> c_int;
        pub fn snd_config_get_string(config: *const SndConfigT, val: *mut *const c_char) -> c_int;
        pub fn snd_config_get_integer(config: *const SndConfigT, val: *mut c_long) -> c_int;
    }
}

/// `no_alsa_link` stand-ins: same signatures, always fail (and say why on
/// stderr). This keeps the `.so` linkable — and its exported entry symbol
/// verifiable — on hosts without libasound2-dev.
#[cfg(no_alsa_link)]
mod sys {
    use super::{SndConfigT, SndPcmIoplug};
    use std::ffi::{c_char, c_int, c_long, c_uint};

    pub unsafe fn snd_pcm_ioplug_create(
        io: *mut SndPcmIoplug,
        name: *const c_char,
        stream: c_int,
        mode: c_int,
    ) -> c_int {
        let _ = (io, name, stream, mode);
        eprintln!("alsa-plugin-sonicbrew: built with stub FFI (no libasound); a live PCM open is unavailable");
        -super::ENODEV
    }

    pub unsafe fn snd_pcm_ioplug_set_param_list(
        io: *mut SndPcmIoplug,
        type_: c_int,
        num_list: c_uint,
        list: *const c_uint,
    ) -> c_int {
        let _ = (io, type_, num_list, list);
        -super::ENODEV
    }

    pub unsafe fn snd_config_search(
        config: *mut SndConfigT,
        key: *const c_char,
        result: *mut *mut SndConfigT,
    ) -> c_int {
        let _ = (config, key, result);
        -super::ENOENT // behave as "key absent" → defaults apply
    }

    pub unsafe fn snd_config_get_string(
        config: *const SndConfigT,
        val: *mut *const c_char,
    ) -> c_int {
        let _ = (config, val);
        -super::EINVAL
    }

    pub unsafe fn snd_config_get_integer(config: *const SndConfigT, val: *mut c_long) -> c_int {
        let _ = (config, val);
        -super::EINVAL
    }
}

// ---------------------------------------------------------------------------
// Plugin state + callbacks.
// ---------------------------------------------------------------------------

/// The plugin's per-PCM state. `io` MUST stay the first field: `cb_close`
/// reclaims the whole box from the `io` pointer libasound hands back.
#[repr(C)]
struct SonicbrewPcm {
    io: SndPcmIoplug,
    bridge: BridgeStream,
    channels: usize,
    /// Total frames transferred across the wire (monotonic; the callback
    /// pointer derives `hw_frames % buffer_size` from it).
    hw_frames: SndPcmUframes,
    /// Scratch for one transfer: `size × channels` interleaved samples.
    samples: Vec<f32>,
}

/// The callback table handed to libasound via `io.callback`.
static CALLBACKS: SndPcmIoplugCallback = SndPcmIoplugCallback {
    start: Some(cb_start),
    stop: Some(cb_stop),
    pointer: Some(cb_pointer),
    transfer: Some(cb_transfer),
    close: Some(cb_close),
    hw_params: None,
    hw_free: None,
    sw_params: None,
    prepare: None,
    drain: None,
    pause: None,
    resume: None,
    poll_descriptors_count: None,
    poll_descriptors: None,
    poll_revents: None,
    dump: None,
    delay: None,
    query_chmaps: None,
    get_chmap: None,
    set_chmap: None,
};

unsafe extern "C" fn cb_start(_io: *mut SndPcmIoplug) -> c_int {
    // The wire is armed by the eager connect+handshake at open time.
    0
}

unsafe extern "C" fn cb_stop(_io: *mut SndPcmIoplug) -> c_int {
    // MVP: the socket stays open across stop/start cycles (drain-free).
    0
}

unsafe extern "C" fn cb_pointer(io: *mut SndPcmIoplug) -> SndPcmSframes {
    let io = unsafe { &*io };
    let sb = unsafe { &*(io.private_data as *const SonicbrewPcm) };
    let buffer = io.buffer_size;
    if buffer == 0 {
        // Called before hw_params negotiated a buffer (should not happen).
        return 0;
    }
    (sb.hw_frames % buffer) as SndPcmSframes
}

unsafe extern "C" fn cb_transfer(
    io: *mut SndPcmIoplug,
    areas: *const SndPcmChannelArea,
    offset: SndPcmUframes,
    size: SndPcmUframes,
) -> SndPcmSframes {
    let io = unsafe { &*io };
    let sb = unsafe { &mut *(io.private_data as *mut SonicbrewPcm) };
    match unsafe { transfer_frames(sb, io.stream, areas, offset, size) } {
        Ok(n) => n as SndPcmSframes,
        Err(e) => {
            eprintln!("alsa-plugin-sonicbrew: transfer failed: {e}");
            -(EIO as SndPcmSframes)
        }
    }
}

unsafe extern "C" fn cb_close(io: *mut SndPcmIoplug) -> c_int {
    // `io` is the first field of SonicbrewPcm (see the struct's comment).
    let sb = unsafe { Box::from_raw(io as *mut SonicbrewPcm) };
    drop(sb);
    0
}

/// Moves `size` frames between the ALSA channel areas and the bridge socket.
///
/// The per-sample address formula is the generic ioplug one:
/// `addr + (first + (offset + frame) * step) / 8` — exact for
/// `SND_PCM_ACCESS_RW_INTERLEAVED` (all channels share `addr`; `first` is the
/// channel slot, `step` the frame stride).
unsafe fn transfer_frames(
    sb: &mut SonicbrewPcm,
    stream: c_int,
    areas: *const SndPcmChannelArea,
    offset: SndPcmUframes,
    size: SndPcmUframes,
) -> std::io::Result<SndPcmUframes> {
    let count = usize::try_from(size.checked_mul(sb.channels as SndPcmUframes).ok_or_else(
        || std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame count overflow"),
    )?)
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame count overflow"))?;
    sb.samples.resize(count, 0.0);

    if stream == SND_PCM_STREAM_PLAYBACK {
        unsafe { gather_interleaved(&mut sb.samples, areas, offset, size, sb.channels) };
        bridge::send_frames(&mut sb.bridge, &sb.samples)?;
    } else {
        bridge::recv_frames_into(&mut sb.bridge, &mut sb.samples)?;
        unsafe { scatter_interleaved(&sb.samples, areas, offset, size, sb.channels) };
    }
    sb.hw_frames = sb.hw_frames.wrapping_add(size);
    Ok(size)
}

/// Copies ALSA channel areas → interleaved scratch (`samples.len() =
/// size × channels`).
///
/// # Safety
///
/// `areas` must point to `channels` valid `snd_pcm_channel_area_t` entries
/// describing a buffer that holds `offset + size` frames.
unsafe fn gather_interleaved(
    samples: &mut [f32],
    areas: *const SndPcmChannelArea,
    offset: SndPcmUframes,
    size: SndPcmUframes,
    channels: usize,
) {
    for frame in 0..size {
        for ch in 0..channels {
            let area = unsafe { &*areas.add(ch) };
            let bits = u64::from(area.first) + (offset + frame) * u64::from(area.step);
            let src = (area.addr as usize + (bits / 8) as usize) as *const f32;
            let idx = frame as usize * channels + ch;
            samples[idx] = unsafe { src.read_unaligned() };
        }
    }
}

/// Copies interleaved scratch → ALSA channel areas (capture direction).
///
/// # Safety
///
/// Same contract as [`gather_interleaved`].
unsafe fn scatter_interleaved(
    samples: &[f32],
    areas: *const SndPcmChannelArea,
    offset: SndPcmUframes,
    size: SndPcmUframes,
    channels: usize,
) {
    for frame in 0..size {
        for ch in 0..channels {
            let area = unsafe { &*areas.add(ch) };
            let bits = u64::from(area.first) + (offset + frame) * u64::from(area.step);
            let dst = (area.addr as usize + (bits / 8) as usize) as *mut f32;
            let idx = frame as usize * channels + ch;
            unsafe { dst.write_unaligned(samples[idx]) };
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point: SND_PCM_PLUGIN_DEFINE_FUNC(sonicbrew).
// ---------------------------------------------------------------------------

/// Why [`open_pcm`] failed; mapped to a negative errno at the ABI boundary.
#[derive(Debug)]
enum OpenFail {
    /// Bad conf contents (unknown key, bad value).
    Config(bridge::ConfigError),
    /// Socket/handshake failure against the bridge server.
    Bridge(bridge::OpenError),
    /// libasound rejected a setup call; carries its negative return code.
    Alsa(c_int),
    /// `stream` was neither playback nor capture.
    BadStream,
}

impl std::fmt::Display for OpenFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "conf: {e}"),
            Self::Bridge(e) => write!(f, "bridge: {e}"),
            Self::Alsa(code) => write!(f, "alsa call failed with {code}"),
            Self::BadStream => write!(f, "stream is neither playback nor capture"),
        }
    }
}

impl std::error::Error for OpenFail {}

/// ALSA plugin ABI-version companion symbol required by `snd_dlsym_verify`
/// (alsa's `SND_PCM_PLUGIN_SYMBOL` / `SND_DLSYM_BUILD_VERSION` in `global.h`
/// expand to exactly this name: `_` + `_snd_pcm_sonicbrew_open` +
/// `_dlsym_pcm_001`). libasound only checks the symbol EXISTS via `dlsym`
/// before trusting the plugin — it is never called. Without it,
/// `snd_pcm_open` fails with "unable to verify version for symbol".
#[no_mangle]
#[allow(non_upper_case_globals)]
pub static __snd_pcm_sonicbrew_open_dlsym_pcm_001: u8 = 0;

/// The `SND_PCM_PLUGIN_ENTRY(sonicbrew)` — libasound resolves the symbol
/// `_snd_pcm_sonicbrew_open` after `dlopen`ing `libasound_module_pcm_sonicbrew.so`.
///
/// Fills `*pcmp` with a PCM backed by the local-socket bridge on success.
///
/// # Safety
///
/// C ABI contract: called by libasound only, with `pcmp` valid for writing,
/// `name` a valid NUL-terminated string or null, and `conf`/`root` valid
/// config nodes owned by libasound.
#[no_mangle]
pub unsafe extern "C" fn _snd_pcm_sonicbrew_open(
    pcmp: *mut *mut SndPcmT,
    name: *const c_char,
    _root: *mut SndConfigT,
    conf: *mut SndConfigT,
    stream: c_int,
    mode: c_int,
) -> c_int {
    match unsafe { open_pcm(pcmp, name, conf, stream, mode) } {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("alsa-plugin-sonicbrew: open failed: {e}");
            match &e {
                OpenFail::Config(_)
                | OpenFail::Bridge(bridge::OpenError::Handshake(_))
                | OpenFail::Bridge(bridge::OpenError::Mismatch { .. })
                | OpenFail::BadStream => -EINVAL,
                OpenFail::Bridge(bridge::OpenError::Io(_)) => -EIO,
                OpenFail::Alsa(code) => *code,
            }
        }
    }
}

unsafe fn open_pcm(
    pcmp: *mut *mut SndPcmT,
    name: *const c_char,
    conf: *mut SndConfigT,
    stream: c_int,
    mode: c_int,
) -> Result<(), OpenFail> {
    let dir = match stream {
        SND_PCM_STREAM_PLAYBACK => StreamDir::Playback,
        SND_PCM_STREAM_CAPTURE => StreamDir::Capture,
        _ => return Err(OpenFail::BadStream),
    };

    let pairs = unsafe { extract_conf_pairs(conf) }.map_err(OpenFail::Alsa)?;
    let cfg = bridge::config_from_pairs(&pairs).map_err(OpenFail::Config)?;

    // Eager connect + handshake so a down server fails snd_pcm_open itself.
    let (sock, echoed) = BridgeStream::connect(&cfg.endpoint, dir, cfg.channels, cfg.rate)
        .map_err(OpenFail::Bridge)?;
    eprintln!(
        "alsa-plugin-sonicbrew: {} via {} ({}ch {}Hz, dir {:?})",
        if dir == StreamDir::Playback {
            "playback"
        } else {
            "capture"
        },
        cfg.endpoint.describe(),
        echoed.channels,
        echoed.rate,
        echoed.stream_dir,
    );

    let mut sb = Box::new(SonicbrewPcm {
        io: SndPcmIoplug {
            version: SND_PCM_IOPLUG_VERSION,
            name: PLUGIN_NAME.as_ptr(),
            flags: 0,
            poll_fd: sock.as_raw_fd(),
            poll_events: if dir == StreamDir::Playback {
                POLLOUT
            } else {
                POLLIN
            },
            mmap_rw: 0,
            callback: &CALLBACKS,
            private_data: std::ptr::null_mut(),
            pcm: std::ptr::null_mut(),
            stream,
            state: 0,
            appl_ptr: 0,
            hw_ptr: 0,
            nonblock: 0,
            access: 0,
            format: 0,
            channels: cfg.channels,
            rate: cfg.rate,
            period_size: 0,
            buffer_size: 0,
        },
        bridge: sock,
        channels: cfg.channels as usize,
        hw_frames: 0,
        samples: Vec::new(),
    });

    // Hand ownership of the box to libasound: the close callback reclaims it.
    sb.io.private_data = (&mut *sb) as *mut SonicbrewPcm as *mut c_void;
    let name_arg = if name.is_null() {
        PLUGIN_NAME.as_ptr()
    } else {
        name
    };
    let r = unsafe { sys::snd_pcm_ioplug_create(&mut sb.io, name_arg, stream, mode) };
    if r < 0 {
        // Box drops here → socket closes; nothing was published.
        return Err(OpenFail::Alsa(r));
    }

    // Pin the hw constraints AFTER create(): set_param_list stores into the
    // ioplug-private constraint lists that snd_pcm_ioplug_create allocates —
    // calling it before create dereferences unallocated state (SIGSEGV, the
    // pattern every reference plugin — pulse/jack — follows: create first,
    // then set_param_list).
    unsafe { pin_hw(&mut sb.io, cfg.channels, cfg.rate) }.map_err(OpenFail::Alsa)?;

    unsafe { *pcmp = sb.io.pcm };
    std::mem::forget(sb);
    Ok(())
}

/// Registers the single-element hw constraint lists. Called before
/// `snd_pcm_ioplug_create`; returns the libasound code on failure.
unsafe fn pin_hw(io: &mut SndPcmIoplug, channels: u32, rate: u32) -> Result<(), c_int> {
    let access = [SND_PCM_ACCESS_RW_INTERLEAVED];
    let formats = [SND_PCM_FORMAT_FLOAT_LE];
    let chans = [channels];
    let rates = [rate];
    unsafe {
        for (hw, list) in [
            (HW_ACCESS, &access[..]),
            (HW_FORMAT, &formats[..]),
            (HW_CHANNELS, &chans[..]),
            (HW_RATE, &rates[..]),
        ] {
            let r = sys::snd_pcm_ioplug_set_param_list(io, hw, list.len() as c_uint, list.as_ptr());
            if r < 0 {
                return Err(r);
            }
        }
    }
    Ok(())
}

/// Pulls the plugin's conf keys out of `conf` via `snd_config_search`
/// (avoids modeling alsa's config iterator linked list). Returns the pairs
/// in a fixed order, ready for [`bridge::config_from_pairs`].
unsafe fn extract_conf_pairs(conf: *mut SndConfigT) -> Result<Vec<(String, ConfValue)>, c_int> {
    let mut pairs = Vec::new();
    for (key, is_str) in [
        (c"socket", true),
        (c"server", true),
        (c"port", false),
        (c"channels", false),
        (c"rate", false),
    ] {
        let mut node: *mut SndConfigT = std::ptr::null_mut();
        let r = unsafe { sys::snd_config_search(conf, key.as_ptr(), &mut node) };
        if r == -ENOENT {
            continue;
        }
        if r < 0 {
            return Err(r);
        }
        let value = if is_str {
            let mut s: *const c_char = std::ptr::null();
            let r = unsafe { sys::snd_config_get_string(node, &mut s) };
            if r < 0 {
                return Err(r);
            }
            ConfValue::Str(unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned())
        } else {
            let mut v: c_long = 0;
            let r = unsafe { sys::snd_config_get_integer(node, &mut v) };
            if r < 0 {
                return Err(r);
            }
            ConfValue::Int(v)
        };
        pairs.push((key.to_string_lossy().into_owned(), value));
    }
    Ok(pairs)
}

// ---------------------------------------------------------------------------
// ABI pinning tests — the hand-mirrored layouts, asserted against the
// documented alsa-lib offsets (LP64: Linux + FreeBSD x86_64/arm64).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_macro_matches_protocol_1_0_2() {
        assert_eq!(SND_PCM_IOPLUG_VERSION, (1 << 16) | (0 << 8) | 2);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn abi_layout_matches_alsa() {
        use std::mem::size_of;
        // struct snd_pcm_ioplug (pcm_ioplug.h, protocol 1.0.2).
        assert_eq!(size_of::<SndPcmIoplug>(), 120);
        assert_eq!(offset_of!(SndPcmIoplug, version), 0);
        assert_eq!(offset_of!(SndPcmIoplug, name), 8);
        assert_eq!(offset_of!(SndPcmIoplug, flags), 16);
        assert_eq!(offset_of!(SndPcmIoplug, poll_fd), 20);
        assert_eq!(offset_of!(SndPcmIoplug, poll_events), 24);
        assert_eq!(offset_of!(SndPcmIoplug, mmap_rw), 28);
        assert_eq!(offset_of!(SndPcmIoplug, callback), 32);
        assert_eq!(offset_of!(SndPcmIoplug, private_data), 40);
        assert_eq!(offset_of!(SndPcmIoplug, pcm), 48);
        assert_eq!(offset_of!(SndPcmIoplug, stream), 56);
        assert_eq!(offset_of!(SndPcmIoplug, state), 60);
        assert_eq!(offset_of!(SndPcmIoplug, appl_ptr), 64);
        assert_eq!(offset_of!(SndPcmIoplug, hw_ptr), 72);
        assert_eq!(offset_of!(SndPcmIoplug, nonblock), 80);
        assert_eq!(offset_of!(SndPcmIoplug, access), 84);
        assert_eq!(offset_of!(SndPcmIoplug, format), 88);
        assert_eq!(offset_of!(SndPcmIoplug, channels), 92);
        assert_eq!(offset_of!(SndPcmIoplug, rate), 96);
        assert_eq!(offset_of!(SndPcmIoplug, period_size), 104);
        assert_eq!(offset_of!(SndPcmIoplug, buffer_size), 112);

        // struct snd_pcm_ioplug_callback — 20 fn pointers in header order.
        assert_eq!(size_of::<SndPcmIoplugCallback>(), 160);
        assert_eq!(offset_of!(SndPcmIoplugCallback, start), 0);
        assert_eq!(offset_of!(SndPcmIoplugCallback, stop), 8);
        assert_eq!(offset_of!(SndPcmIoplugCallback, pointer), 16);
        assert_eq!(offset_of!(SndPcmIoplugCallback, transfer), 24);
        assert_eq!(offset_of!(SndPcmIoplugCallback, close), 32);
        assert_eq!(offset_of!(SndPcmIoplugCallback, hw_params), 40);
        assert_eq!(offset_of!(SndPcmIoplugCallback, hw_free), 48);
        assert_eq!(offset_of!(SndPcmIoplugCallback, sw_params), 56);
        assert_eq!(offset_of!(SndPcmIoplugCallback, prepare), 64);
        assert_eq!(offset_of!(SndPcmIoplugCallback, drain), 72);
        assert_eq!(offset_of!(SndPcmIoplugCallback, pause), 80);
        assert_eq!(offset_of!(SndPcmIoplugCallback, resume), 88);
        assert_eq!(offset_of!(SndPcmIoplugCallback, poll_descriptors_count), 96);
        assert_eq!(offset_of!(SndPcmIoplugCallback, poll_descriptors), 104);
        assert_eq!(offset_of!(SndPcmIoplugCallback, poll_revents), 112);
        assert_eq!(offset_of!(SndPcmIoplugCallback, dump), 120);
        assert_eq!(offset_of!(SndPcmIoplugCallback, delay), 128);
        assert_eq!(offset_of!(SndPcmIoplugCallback, query_chmaps), 136);
        assert_eq!(offset_of!(SndPcmIoplugCallback, get_chmap), 144);
        assert_eq!(offset_of!(SndPcmIoplugCallback, set_chmap), 152);

        // struct snd_pcm_channel_area { uframes addr; uint first; uint step; }
        assert_eq!(size_of::<SndPcmChannelArea>(), 16);
        assert_eq!(offset_of!(SndPcmChannelArea, addr), 0);
        assert_eq!(offset_of!(SndPcmChannelArea, first), 8);
        assert_eq!(offset_of!(SndPcmChannelArea, step), 12);

        // Opaque mirrors are honest ZSTs.
        assert_eq!(size_of::<SndPcmT>(), 0);
        assert_eq!(size_of::<SndConfigT>(), 0);
    }

    #[test]
    fn callback_table_sets_exactly_the_mvp_callbacks() {
        // Exactly the five implemented callbacks: start, stop, pointer,
        // transfer, close; every optional one stays unset.
        let n_set = [
            CALLBACKS.start.is_some(),
            CALLBACKS.stop.is_some(),
            CALLBACKS.pointer.is_some(),
            CALLBACKS.transfer.is_some(),
            CALLBACKS.close.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        assert_eq!(n_set, 5);
        let n_none = [
            CALLBACKS.hw_params.is_none(),
            CALLBACKS.hw_free.is_none(),
            CALLBACKS.sw_params.is_none(),
            CALLBACKS.prepare.is_none(),
            CALLBACKS.drain.is_none(),
            CALLBACKS.pause.is_none(),
            CALLBACKS.resume.is_none(),
            CALLBACKS.poll_descriptors_count.is_none(),
            CALLBACKS.poll_descriptors.is_none(),
            CALLBACKS.poll_revents.is_none(),
            CALLBACKS.dump.is_none(),
            CALLBACKS.delay.is_none(),
            CALLBACKS.query_chmaps.is_none(),
            CALLBACKS.get_chmap.is_none(),
            CALLBACKS.set_chmap.is_none(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        assert_eq!(n_none, 15);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn channel_area_address_math_matches_ioplug_convention() {
        // Interleaved stereo f32: first = ch*32 bits, step = channels*32 bits.
        // Sample (frame=1, ch=1) must land at byte offset
        // (32 + (0 + 1) * 64) / 8 = 12 = frame1 * 8 bytes + ch1 * 4 bytes.
        let buffer = [[0.25_f32, -0.25], [0.5, -0.5]];
        let areas = [
            SndPcmChannelArea {
                addr: buffer.as_ptr() as u64,
                first: 0,
                step: 64,
            },
            SndPcmChannelArea {
                addr: buffer.as_ptr() as u64,
                first: 32,
                step: 64,
            },
        ];
        let mut samples = vec![0.0; 4];
        unsafe { gather_interleaved(&mut samples, areas.as_ptr(), 0, 2, 2) };
        assert_eq!(samples, vec![0.25, -0.25, 0.5, -0.5]);

        // Round-trip back through scatter with a nonzero ALSA offset.
        let mut sink = [[0.0_f32; 2]; 4];
        let areas = [
            SndPcmChannelArea {
                addr: sink.as_ptr() as u64,
                first: 0,
                step: 64,
            },
            SndPcmChannelArea {
                addr: sink.as_ptr() as u64,
                first: 32,
                step: 64,
            },
        ];
        let src = [1.0_f32, -1.0, 0.5, -0.5];
        unsafe { scatter_interleaved(&src, areas.as_ptr(), 1, 2, 2) };
        assert_eq!(sink[1], [1.0, -1.0]);
        assert_eq!(sink[2], [0.5, -0.5]);
        assert_eq!(sink[0], [0.0, 0.0]); // untouched
    }
}
