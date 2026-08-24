//! Pure-Rust local-socket audio bridge — the testable half of the sonicbrew
//! ALSA PCM plugin (M11, P2).
//!
//! The [`lib.rs`](crate) FFI half owns only the libasound C ABI; everything
//! with actual behavior lives here so it can be unit-tested without libasound
//! (the Linux dev host has none). The bridge speaks a deliberately tiny
//! wire protocol to a sonicbrew-side counterpart server (that server is a
//! separate task; the protocol below is its contract):
//!
//! # Wire protocol (v1)
//!
//! * **Handshake** — 5 × `u32` native-little-endian, sent by the plugin and
//!   echoed verbatim by the server:
//!
//!   ```text
//!   [BRIDGE_MAGIC, BRIDGE_PROTO_VERSION, stream_dir, channels, rate]
//!   ```
//!
//!   `stream_dir` uses the ALSA convention `0 = playback, 1 = capture`. A
//!   wrong magic/version or an echoed mismatch (direction / channels / rate)
//!   aborts the open.
//!
//! * **Payload** — raw interleaved `f32` little-endian samples with no
//!   per-block framing. Playback: plugin → server. Capture: server → plugin.
//!   Block boundaries are implicit: each side sends exactly
//!   `frames × channels` samples per ALSA transfer.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Arbitrary marker for the v1 protocol ("SB" = sonicbrew bridge).
pub const BRIDGE_MAGIC: u32 = 0x5342_4E52;
/// Wire protocol revision understood by this build.
pub const BRIDGE_PROTO_VERSION: u32 = 1;
/// TCP fallback endpoint when the ALSA conf names neither `socket` nor
/// `server` (loopback sonicbrew default).
pub const DEFAULT_SERVER: &str = "127.0.0.1";
/// See [`DEFAULT_SERVER`].
pub const DEFAULT_TCP_PORT: u16 = 9001;
/// Default channel count when the ALSA conf does not override it (stereo).
pub const DEFAULT_CHANNELS: u32 = 2;
/// Default sample rate when the ALSA conf does not override it (48 kHz).
pub const DEFAULT_RATE: u32 = 48_000;
/// Upper bound accepted for the `channels` conf key.
pub const MAX_CHANNELS: u32 = 1024;
/// Upper bound accepted for the `rate` conf key (covers 768 kHz DXD).
pub const MAX_RATE: u32 = 768_000;

/// Stream direction, mirroring `snd_pcm_stream_t` wire values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDir {
    /// `SND_PCM_STREAM_PLAYBACK` = 0: plugin sends audio to the server.
    Playback = 0,
    /// `SND_PCM_STREAM_CAPTURE` = 1: server sends audio to the plugin.
    Capture = 1,
}

impl StreamDir {
    /// Decodes the handshake wire value; anything else is invalid.
    #[must_use]
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Playback),
            1 => Some(Self::Capture),
            _ => None,
        }
    }
}

/// The v1 handshake frame (see the module-level [wire protocol](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    /// Marker; always [`BRIDGE_MAGIC`] from this build.
    pub magic: u32,
    /// Protocol revision; always [`BRIDGE_PROTO_VERSION`] from this build.
    pub version: u32,
    /// Stream direction (ALSA wire convention).
    pub stream_dir: StreamDir,
    /// Channel count offered by the opener.
    pub channels: u32,
    /// Sample rate offered by the opener.
    pub rate: u32,
}

impl Handshake {
    /// Builds the handshake this plugin sends for the given stream setup.
    #[must_use]
    pub fn new(stream_dir: StreamDir, channels: u32, rate: u32) -> Self {
        Self {
            magic: BRIDGE_MAGIC,
            version: BRIDGE_PROTO_VERSION,
            stream_dir,
            channels,
            rate,
        }
    }
}

/// Errors from reading (and validating) a handshake frame.
#[derive(Debug)]
pub enum HandshakeError {
    /// Socket-level failure while reading the 20 header bytes.
    Io(io::Error),
    /// Peer does not speak the sonicbrew bridge protocol.
    BadMagic(u32),
    /// Peer speaks a protocol revision this build cannot handle.
    UnsupportedVersion(u32),
    /// Handshake carried an out-of-range field (direction/channels/rate).
    Invalid(&'static str),
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "handshake i/o: {e}"),
            Self::BadMagic(m) => write!(f, "bad handshake magic 0x{m:08x}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported bridge version {v}"),
            Self::Invalid(what) => write!(f, "invalid handshake field: {what}"),
        }
    }
}

impl std::error::Error for HandshakeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for HandshakeError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Serializes `hs` as the v1 wire frame (5 × `u32` LE).
///
/// # Errors
///
/// Fails only if the underlying writer does (see [`io::Write`]).
pub fn write_handshake<W: Write>(w: &mut W, hs: &Handshake) -> io::Result<()> {
    let mut buf = [0u8; 20];
    for (i, v) in [
        hs.magic,
        hs.version,
        hs.stream_dir as u32,
        hs.channels,
        hs.rate,
    ]
    .into_iter()
    .enumerate()
    {
        buf[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    w.write_all(&buf)
}

/// Reads and validates one v1 wire frame.
///
/// # Errors
///
/// [`HandshakeError`] on short reads, bad magic, unsupported version, or an
/// out-of-range direction/channels/rate field.
pub fn read_handshake<R: Read>(r: &mut R) -> Result<Handshake, HandshakeError> {
    let mut buf = [0u8; 20];
    r.read_exact(&mut buf).map_err(HandshakeError::Io)?;
    let word =
        |i: usize| u32::from_le_bytes([buf[i * 4], buf[i * 4 + 1], buf[i * 4 + 2], buf[i * 4 + 3]]);
    let magic = word(0);
    if magic != BRIDGE_MAGIC {
        return Err(HandshakeError::BadMagic(magic));
    }
    let version = word(1);
    if version != BRIDGE_PROTO_VERSION {
        return Err(HandshakeError::UnsupportedVersion(version));
    }
    let stream_dir = StreamDir::from_u32(word(2)).ok_or(HandshakeError::Invalid("stream_dir"))?;
    let channels = word(3);
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(HandshakeError::Invalid("channels"));
    }
    let rate = word(4);
    if rate == 0 || rate > MAX_RATE {
        return Err(HandshakeError::Invalid("rate"));
    }
    Ok(Handshake {
        magic,
        version,
        stream_dir,
        channels,
        rate,
    })
}

/// Sends `samples` as raw interleaved little-endian `f32` payload.
///
/// The byte encoding is explicit (`f32::to_bits().to_le_bytes()`), so the
/// wire format does not depend on the host byte order.
///
/// # Errors
///
/// Fails only if the underlying writer does (see [`io::Write`]).
pub fn send_frames<W: Write>(w: &mut W, samples: &[f32]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        buf.extend_from_slice(&s.to_bits().to_le_bytes());
    }
    w.write_all(&buf)
}

/// Receives exactly `out.len()` little-endian `f32` samples into `out`.
///
/// # Errors
///
/// Fails on short reads (see [`io::Read::read_exact`]).
pub fn recv_frames_into<R: Read>(r: &mut R, out: &mut [f32]) -> io::Result<()> {
    let mut bytes = vec![0u8; out.len() * 4];
    r.read_exact(&mut bytes)?;
    for (dst, src) in out.iter_mut().zip(bytes.chunks_exact(4)) {
        *dst = f32::from_le_bytes([src[0], src[1], src[2], src[3]]);
    }
    Ok(())
}

/// Where the bridge counterpart lives: a Unix domain socket path or a TCP
/// host/port pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Unix domain socket (ALSA conf `socket "…"`).
    Unix(PathBuf),
    /// TCP host + port (ALSA conf `server` / `port`). The host is kept as a
    /// string so names like `localhost` resolve at connect time.
    Tcp {
        /// Host name or bracketed IPv6 literal (without brackets).
        host: String,
        /// Remote port.
        port: u16,
    },
}

impl Endpoint {
    /// Human-readable form for log lines.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Unix(p) => format!("unix:{}", p.display()),
            Self::Tcp { host, port } => format!("tcp:{host}:{port}"),
        }
    }

    /// Opens a raw socket to this endpoint (no handshake).
    ///
    /// # Errors
    ///
    /// [`io::Error`] when the socket cannot be created or connected (e.g.
    /// nothing listens at the endpoint yet).
    pub fn connect(&self) -> io::Result<BridgeStream> {
        match self {
            Self::Unix(path) => Ok(BridgeStream {
                inner: Inner::Unix(UnixStream::connect(path)?),
            }),
            Self::Tcp { host, port } => {
                let stream = TcpStream::connect((host.as_str(), *port))?;
                stream.set_nodelay(true).ok();
                Ok(BridgeStream {
                    inner: Inner::Tcp(stream),
                })
            }
        }
    }
}

#[derive(Debug)]
enum Inner {
    Unix(UnixStream),
    Tcp(TcpStream),
}

/// A connected bridge socket (Unix or TCP) speaking the v1 protocol.
///
/// The handshake is driven by [`connect`](Self::connect); afterwards the
/// stream is a plain byte pipe — use [`send_frames`] / [`recv_frames_into`].
#[derive(Debug)]
pub struct BridgeStream {
    inner: Inner,
}

impl BridgeStream {
    /// Connects to `endpoint`, sends the v1 handshake, and validates the
    /// server's echo. Returns the echoed handshake (== the agreed setup).
    ///
    /// Connecting eagerly at ALSA open time (rather than lazily at `start`)
    /// matches the pulse/jack plugin convention: a down server fails the
    /// `snd_pcm_open` call itself.
    ///
    /// # Errors
    ///
    /// [`OpenError`] on connect failure, a malformed/rejected handshake, or
    /// an echo that disagrees with what was offered.
    pub fn connect(
        endpoint: &Endpoint,
        dir: StreamDir,
        channels: u32,
        rate: u32,
    ) -> Result<(Self, Handshake), OpenError> {
        let mut stream = endpoint.connect().map_err(OpenError::Io)?;
        let offered = Handshake::new(dir, channels, rate);
        write_handshake(&mut stream, &offered).map_err(OpenError::Io)?;
        let echoed = read_handshake(&mut stream).map_err(OpenError::Handshake)?;
        if echoed.stream_dir != offered.stream_dir
            || echoed.channels != offered.channels
            || echoed.rate != offered.rate
        {
            return Err(OpenError::Mismatch { offered, echoed });
        }
        Ok((stream, echoed))
    }

    /// The poll(2) fd for the ALSA ioplug `poll_fd` field.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        match &self.inner {
            Inner::Unix(s) => s.as_raw_fd(),
            Inner::Tcp(s) => s.as_raw_fd(),
        }
    }

    /// See [`std::net::TcpStream::set_read_timeout`] (tests use this to
    /// fail fast instead of hanging on a wedged peer).
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the platform rejects the duration.
    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            Inner::Unix(s) => s.set_read_timeout(d),
            Inner::Tcp(s) => s.set_read_timeout(d),
        }
    }
}

impl Read for BridgeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Unix(s) => s.read(buf),
            Inner::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for BridgeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Unix(s) => s.write(buf),
            Inner::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Inner::Unix(s) => s.flush(),
            Inner::Tcp(s) => s.flush(),
        }
    }
}

/// Errors from [`BridgeStream::connect`].
#[derive(Debug)]
pub enum OpenError {
    /// Socket-level failure (connect / read / write).
    Io(io::Error),
    /// Peer sent a malformed or incompatible handshake.
    Handshake(HandshakeError),
    /// Peer's echo disagreed with the offered setup.
    Mismatch {
        /// What this plugin offered.
        offered: Handshake,
        /// What the server echoed.
        echoed: Handshake,
    },
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "bridge connect: {e}"),
            Self::Handshake(e) => write!(f, "bridge handshake: {e}"),
            Self::Mismatch { offered, echoed } => write!(
                f,
                "bridge echo mismatch: offered {offered:?}, echoed {echoed:?}"
            ),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Handshake(e) => Some(e),
            Self::Mismatch { .. } => None,
        }
    }
}

impl From<io::Error> for OpenError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A conf value as extracted by the FFI layer from `snd_config_t` — exactly
/// the two `snd_config_get_*` shapes the plugin keys use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfValue {
    /// `snd_config_get_string` result.
    Str(String),
    /// `snd_config_get_integer` result.
    Int(i64),
}

/// A validated plugin configuration (ALSA conf keys → bridge endpoint +
/// stream setup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfig {
    /// Where to connect.
    pub endpoint: Endpoint,
    /// Channel count forced via hw constraints.
    pub channels: u32,
    /// Sample rate forced via hw constraints.
    pub rate: u32,
}

/// Errors from [`config_from_pairs`].
#[derive(Debug)]
pub enum ConfigError {
    /// A conf key this plugin does not understand.
    UnknownKey(String),
    /// A known key with the wrong value type (string vs integer).
    TypeMismatch(String),
    /// Keys that cannot be combined (`socket` + `server`, embedded port +
    /// `port`, `port` without `server`).
    Conflict(String),
    /// A value failed range/syntax validation.
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey(k) => write!(f, "unknown conf key `{k}`"),
            Self::TypeMismatch(k) => write!(f, "conf key `{k}` has the wrong type"),
            Self::Conflict(msg) => write!(f, "conf conflict: {msg}"),
            Self::Invalid(msg) => write!(f, "invalid conf value: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Keys libasound itself puts in a plugin conf node; understood and ignored.
const IGNORED_KEYS: [&str; 3] = ["type", "comment", "hint"];

/// Reduces the extracted conf pairs to a [`BridgeConfig`].
///
/// Accepted keys: `socket` (string), `server` (string, `"host"` or
/// `"host:port"`; IPv6 literals must be bracketed), `port` (integer,
/// default 9001, only with a bare-host `server`), `channels` (integer,
/// default 2), `rate` (integer, default 48 000). With neither `socket` nor
/// `server` the bridge defaults to `tcp:127.0.0.1:9001`. `type`, `comment`
/// and `hint` are ignored; any other key is rejected.
///
/// # Errors
///
/// [`ConfigError`] for unknown keys, type mismatches, key conflicts, or
/// out-of-range values.
pub fn config_from_pairs(pairs: &[(String, ConfValue)]) -> Result<BridgeConfig, ConfigError> {
    let mut socket: Option<&str> = None;
    let mut server: Option<&str> = None;
    let mut port: Option<u16> = None;
    let mut channels: Option<i64> = None;
    let mut rate: Option<i64> = None;

    for (key, value) in pairs {
        match key.as_str() {
            "socket" => {
                socket = Some(
                    value
                        .as_str()
                        .ok_or_else(|| ConfigError::TypeMismatch(key.clone()))?,
                );
            }
            "server" => {
                server = Some(
                    value
                        .as_str()
                        .ok_or_else(|| ConfigError::TypeMismatch(key.clone()))?,
                );
            }
            "port" => {
                let v = value
                    .as_int()
                    .ok_or_else(|| ConfigError::TypeMismatch(key.clone()))?;
                port = Some(
                    u16::try_from(v)
                        .map_err(|_| ConfigError::Invalid("port out of range".into()))?,
                );
            }
            "channels" => {
                channels = Some(
                    value
                        .as_int()
                        .ok_or_else(|| ConfigError::TypeMismatch(key.clone()))?,
                );
            }
            "rate" => {
                rate = Some(
                    value
                        .as_int()
                        .ok_or_else(|| ConfigError::TypeMismatch(key.clone()))?,
                );
            }
            k if IGNORED_KEYS.contains(&k) => {}
            k => return Err(ConfigError::UnknownKey(k.to_owned())),
        }
    }

    if socket.is_some() && server.is_some() {
        return Err(ConfigError::Conflict(
            "`socket` and `server` are exclusive".into(),
        ));
    }
    if port.is_some() && server.is_none() {
        return Err(ConfigError::Conflict("`port` requires `server`".into()));
    }

    let endpoint = if let Some(path) = socket {
        Endpoint::Unix(PathBuf::from(path))
    } else {
        let (server, defaulted) = match server {
            Some(s) => (s, false),
            None => (DEFAULT_SERVER, true),
        };
        // An absent `server` implies the default endpoint, so the default TCP
        // port applies when the user supplied neither `server` nor `port`.
        let port = port.or(if defaulted {
            Some(DEFAULT_TCP_PORT)
        } else {
            None
        });
        let (host, port) = parse_server(server, port)?;
        Endpoint::Tcp { host, port }
    };

    let channels = channels.unwrap_or(i64::from(DEFAULT_CHANNELS));
    let rate = rate.unwrap_or(i64::from(DEFAULT_RATE));
    let channels = u32::try_from(channels)
        .ok()
        .filter(|c| (1..=MAX_CHANNELS).contains(c))
        .ok_or_else(|| ConfigError::Invalid(format!("channels must be 1..={MAX_CHANNELS}")))?;
    let rate = u32::try_from(rate)
        .ok()
        .filter(|r| (1..=MAX_RATE).contains(r))
        .ok_or_else(|| ConfigError::Invalid(format!("rate must be 1..={MAX_RATE}")))?;

    Ok(BridgeConfig {
        endpoint,
        channels,
        rate,
    })
}

/// Splits an ALSA `server` value into host + port, applying `port_key` only
/// when the value has no embedded `:port` suffix.
fn parse_server(server: &str, port_key: Option<u16>) -> Result<(String, u16), ConfigError> {
    if let Some((host, port_str)) = server.rsplit_once(':') {
        if port_key.is_some() {
            return Err(ConfigError::Conflict(
                "`server` already carries :port; drop the `port` key".into(),
            ));
        }
        // Bracketed IPv6 literals look like "[::1]:9001" after rsplit.
        if !host.starts_with('[') && host.contains(':') {
            return Err(ConfigError::Invalid(
                "bracket IPv6 literals as [addr]:port".into(),
            ));
        }
        let port = port_str
            .parse::<u16>()
            .map_err(|_| ConfigError::Invalid(format!("bad port `{port_str}`")))?;
        if port == 0 {
            return Err(ConfigError::Invalid("port must be >= 1".into()));
        }
        Ok((host.to_owned(), port))
    } else {
        // No colon at all (a bare IPv6 literal like "::1" would have one).
        let port = port_key
            .filter(|p| *p != 0)
            .ok_or_else(|| ConfigError::Invalid("port must be >= 1".into()))?;
        Ok((server.to_owned(), port))
    }
}

impl ConfValue {
    /// The string payload, if this is [`ConfValue::Str`].
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            Self::Int(_) => None,
        }
    }

    /// The integer payload, if this is [`ConfValue::Int`].
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(*v),
            Self::Str(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(kvs: &[(&str, ConfValue)]) -> Vec<(String, ConfValue)> {
        kvs.iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    // ---- handshake ------------------------------------------------------------

    #[test]
    fn handshake_roundtrip_cursor() {
        let hs = Handshake::new(StreamDir::Capture, 8, 96_000);
        let mut buf = Vec::new();
        write_handshake(&mut buf, &hs).expect("write");
        assert_eq!(buf.len(), 20);
        // First word on the wire is the magic, little-endian.
        assert_eq!(&buf[0..4], &BRIDGE_MAGIC.to_le_bytes());
        let back = read_handshake(&mut &buf[..]).expect("read");
        assert_eq!(back, hs);
    }

    #[test]
    fn handshake_rejects_bad_magic() {
        let mut hs = Handshake::new(StreamDir::Playback, 2, 48_000);
        hs.magic = BRIDGE_MAGIC ^ 0xFFFF;
        let mut buf = Vec::new();
        write_handshake(&mut buf, &hs).unwrap();
        match read_handshake(&mut &buf[..]) {
            Err(HandshakeError::BadMagic(m)) => assert_eq!(m, hs.magic),
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn handshake_rejects_bad_version() {
        let mut hs = Handshake::new(StreamDir::Playback, 2, 48_000);
        hs.version = 99;
        let mut buf = Vec::new();
        write_handshake(&mut buf, &hs).unwrap();
        assert!(matches!(
            read_handshake(&mut &buf[..]),
            Err(HandshakeError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn handshake_rejects_invalid_stream_dir_wire_value() {
        // Handshake::new always builds valid directions, so craft raw bytes.
        let mut buf = vec![0u8; 20];
        buf[0..4].copy_from_slice(&BRIDGE_MAGIC.to_le_bytes());
        buf[4..8].copy_from_slice(&BRIDGE_PROTO_VERSION.to_le_bytes());
        buf[8..12].copy_from_slice(&7u32.to_le_bytes());
        buf[12..16].copy_from_slice(&2u32.to_le_bytes());
        buf[16..20].copy_from_slice(&48_000u32.to_le_bytes());
        assert!(matches!(
            read_handshake(&mut &buf[..]),
            Err(HandshakeError::Invalid("stream_dir"))
        ));
    }

    #[test]
    fn handshake_rejects_out_of_range_channels_and_rate() {
        let cases: [(fn(&mut Handshake), &str); 4] = [
            (|h: &mut Handshake| h.channels = 0, "channels"),
            (
                |h: &mut Handshake| h.channels = MAX_CHANNELS + 1,
                "channels",
            ),
            (|h: &mut Handshake| h.rate = 0, "rate"),
            (|h: &mut Handshake| h.rate = MAX_RATE + 1, "rate"),
        ];
        for (patch, field) in cases {
            let mut hs = Handshake::new(StreamDir::Playback, 2, 48_000);
            patch(&mut hs);
            let mut buf = Vec::new();
            write_handshake(&mut buf, &hs).unwrap();
            match read_handshake(&mut &buf[..]) {
                Err(HandshakeError::Invalid(what)) => assert_eq!(what, field),
                other => panic!("expected Invalid({field:?}), got {other:?}"),
            }
        }
    }

    // ---- payload --------------------------------------------------------------

    #[test]
    fn frame_bytes_are_explicit_little_endian() {
        let mut buf = Vec::new();
        send_frames(&mut buf, &[1.0_f32]).unwrap();
        // IEEE-754 1.0 = 0x3F800000 → LE bytes 00 00 80 3F.
        assert_eq!(buf, vec![0x00, 0x00, 0x80, 0x3F]);
    }

    #[test]
    fn send_recv_frames_roundtrip_cursor() {
        let samples: Vec<f32> = (-40_i32..40)
            .map(|i| i as f32 * 0.031_25 - 0.5)
            .chain([f32::NEG_INFINITY, f32::INFINITY])
            .collect();
        let mut buf = Vec::new();
        send_frames(&mut buf, &samples).unwrap();
        let mut back = vec![0.0; samples.len()];
        recv_frames_into(&mut &buf[..], &mut back).unwrap();
        // Bit-exact through explicit LE encoding, including infinities.
        let bits = |v: &[f32]| v.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&back), bits(&samples));
    }

    // ---- config ---------------------------------------------------------------

    #[test]
    fn config_defaults_when_empty() {
        let cfg = config_from_pairs(&pairs(&[])).unwrap();
        assert_eq!(
            cfg,
            BridgeConfig {
                endpoint: Endpoint::Tcp {
                    host: DEFAULT_SERVER.to_owned(),
                    port: DEFAULT_TCP_PORT,
                },
                channels: DEFAULT_CHANNELS,
                rate: DEFAULT_RATE,
            }
        );
    }

    #[test]
    fn config_socket_endpoint() {
        let cfg = config_from_pairs(&pairs(&[(
            "socket",
            ConfValue::Str("/tmp/sonicbrew.sock".into()),
        )]))
        .unwrap();
        assert_eq!(
            cfg.endpoint,
            Endpoint::Unix(PathBuf::from("/tmp/sonicbrew.sock"))
        );
        // channels/rate fall back to defaults.
        assert_eq!(cfg.channels, 2);
        assert_eq!(cfg.rate, 48_000);
    }

    #[test]
    fn config_server_and_port() {
        let cfg = config_from_pairs(&pairs(&[
            ("server", ConfValue::Str("10.0.0.4".into())),
            ("port", ConfValue::Int(9999)),
            ("channels", ConfValue::Int(8)),
            ("rate", ConfValue::Int(96_000)),
        ]))
        .unwrap();
        assert_eq!(
            cfg.endpoint,
            Endpoint::Tcp {
                host: "10.0.0.4".into(),
                port: 9999,
            }
        );
        assert_eq!(cfg.channels, 8);
        assert_eq!(cfg.rate, 96_000);
    }

    #[test]
    fn config_server_with_embedded_port() {
        let cfg = config_from_pairs(&pairs(&[(
            "server",
            ConfValue::Str("192.168.1.9:7777".into()),
        )]))
        .unwrap();
        assert_eq!(
            cfg.endpoint,
            Endpoint::Tcp {
                host: "192.168.1.9".into(),
                port: 7777,
            }
        );
        // Embedded port + explicit port key conflict.
        let err = config_from_pairs(&pairs(&[
            ("server", ConfValue::Str("192.168.1.9:7777".into())),
            ("port", ConfValue::Int(1)),
        ]))
        .unwrap_err();
        assert!(matches!(err, ConfigError::Conflict(_)));
    }

    #[test]
    fn config_key_conflicts_rejected() {
        // socket + server
        assert!(matches!(
            config_from_pairs(&pairs(&[
                ("socket", ConfValue::Str("/a".into())),
                ("server", ConfValue::Str("h".into())),
            ]))
            .unwrap_err(),
            ConfigError::Conflict(_)
        ));
        // port without server
        assert!(matches!(
            config_from_pairs(&pairs(&[("port", ConfValue::Int(1))])).unwrap_err(),
            ConfigError::Conflict(_)
        ));
        // unbracketed IPv6-looking host
        assert!(matches!(
            config_from_pairs(&pairs(&[("server", ConfValue::Str("::1".into()))])).unwrap_err(),
            ConfigError::Invalid(_)
        ));
        // bracketed IPv6 literal is fine
        let cfg =
            config_from_pairs(&pairs(&[("server", ConfValue::Str("[::1]:9005".into()))])).unwrap();
        assert_eq!(
            cfg.endpoint,
            Endpoint::Tcp {
                host: "[::1]".into(),
                port: 9005,
            }
        );
    }

    #[test]
    fn config_unknown_and_ignored_keys() {
        assert!(matches!(
            config_from_pairs(&pairs(&[("wat", ConfValue::Int(1))])).unwrap_err(),
            ConfigError::UnknownKey(k) if k == "wat"
        ));
        // libasound's own keys are tolerated.
        let cfg = config_from_pairs(&pairs(&[
            ("type", ConfValue::Str("sonicbrew".into())),
            ("comment", ConfValue::Str("hi".into())),
            ("hint", ConfValue::Str("{}".into())),
        ]))
        .unwrap();
        assert_eq!(cfg.channels, DEFAULT_CHANNELS);
    }

    #[test]
    fn config_type_and_range_validation() {
        // Wrong types.
        assert!(matches!(
            config_from_pairs(&pairs(&[("socket", ConfValue::Int(1))])).unwrap_err(),
            ConfigError::TypeMismatch(_)
        ));
        assert!(matches!(
            config_from_pairs(&pairs(&[("rate", ConfValue::Str("48k".into()))])).unwrap_err(),
            ConfigError::TypeMismatch(_)
        ));
        // Out-of-range values.
        for kvs in [
            vec![("channels", ConfValue::Int(0))],
            vec![("channels", ConfValue::Int(-2))],
            vec![("rate", ConfValue::Int(0))],
            vec![("rate", ConfValue::Int(i64::from(MAX_RATE) + 1))],
            vec![("server", ConfValue::Str("h:x".into()))],
            vec![("server", ConfValue::Str("h:0".into()))],
            vec![
                ("server", ConfValue::Str("h".into())),
                ("port", ConfValue::Int(0)),
            ],
        ] {
            assert!(
                config_from_pairs(&pairs(&kvs)).is_err(),
                "{kvs:?} should be rejected"
            );
        }
    }

    // ---- live socket doubles --------------------------------------------------

    #[test]
    fn tcp_playback_loopback_roundtrip() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let hs = read_handshake(&mut sock).expect("server handshake");
            write_handshake(&mut sock, &hs).expect("server echo");
            // Echo one block of raw payload back.
            let mut buf = [0u8; 64];
            sock.read_exact(&mut buf).expect("server payload");
            sock.write_all(&buf).expect("server echo payload");
            hs
        });

        let (mut stream, echoed) = BridgeStream::connect(
            &Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port: addr.port(),
            },
            StreamDir::Playback,
            2,
            48_000,
        )
        .expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        assert_eq!(echoed.stream_dir, StreamDir::Playback);
        assert_eq!(echoed.channels, 2);

        let sent: Vec<f32> = (0..16_i32).map(|i| i as f32 * 0.25 - 2.0).collect();
        send_frames(&mut stream, &sent).expect("send");
        let mut got = vec![0.0; sent.len()];
        recv_frames_into(&mut stream, &mut got).expect("recv");
        let bits = |v: &[f32]| v.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&got), bits(&sent));

        let hs = server.join().expect("server thread");
        assert_eq!(hs.channels, 2);
        assert_eq!(hs.rate, 48_000);
    }

    #[test]
    fn tcp_connect_rejects_mismatched_echo() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut hs = read_handshake(&mut sock).expect("server handshake");
            hs.channels = 8; // disagree on purpose
            write_handshake(&mut sock, &hs).expect("server echo");
        });
        let err = BridgeStream::connect(
            &Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port: addr.port(),
            },
            StreamDir::Playback,
            2,
            48_000,
        )
        .expect_err("mismatch must fail");
        assert!(matches!(err, OpenError::Mismatch { .. }));
        server.join().expect("server thread");
    }

    #[test]
    fn unix_capture_roundtrip() {
        use std::os::unix::net::UnixListener;

        let path = std::env::temp_dir().join(format!(
            "sonicbrew-bridge-test-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let listener = UnixListener::bind(&path).expect("bind unix");
        let server_path = path.clone();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let hs = read_handshake(&mut sock).expect("server handshake");
            write_handshake(&mut sock, &hs).expect("server echo");
            let samples: Vec<f32> = (0..12_i32).map(|i| i as f32 / 12.0 - 0.5).collect();
            send_frames(&mut sock, &samples).expect("server payload");
        });

        let (mut stream, echoed) = BridgeStream::connect(
            &Endpoint::Unix(server_path.clone()),
            StreamDir::Capture,
            1,
            44_100,
        )
        .expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        assert_eq!(echoed.stream_dir, StreamDir::Capture);
        assert_eq!(echoed.rate, 44_100);

        let mut got = vec![0.0; 12];
        recv_frames_into(&mut stream, &mut got).expect("recv");
        let expect: Vec<f32> = (0..12_i32).map(|i| i as f32 / 12.0 - 0.5).collect();
        let bits = |v: &[f32]| v.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&got), bits(&expect));

        server.join().expect("server thread");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn endpoint_describe_is_log_friendly() {
        assert_eq!(
            Endpoint::Unix(PathBuf::from("/run/s.sock")).describe(),
            "unix:/run/s.sock"
        );
        assert_eq!(
            Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port: 9001
            }
            .describe(),
            "tcp:127.0.0.1:9001"
        );
    }
}
