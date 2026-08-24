//! M10 — live PulseAudio daemon handshake, in pure Rust (no libpulse).
//!
//! **Design decision:** the native protocol is a documented binary format
//! and this crate already owns a parser for it ([`crate::codec`]), so the
//! handshake talks to the daemon socket directly instead of binding to the
//! LGPL `libpulse`. That keeps the crate dependency-free on the wire layer,
//! builds identically on Linux and FreeBSD, and lets parser and daemon share
//! one protocol implementation.
//!
//! Connection target resolution (first existing candidate wins):
//!
//! 1. `$PULSE_SERVER` — honored only when it carries a `unix:` prefix
//!    (a `tcp:`/host value is ignored and the defaults apply);
//! 2. `$XDG_RUNTIME_DIR/pulse/native` — the standard Linux location;
//! 3. `/var/run/pulse/native` — FreeBSD system-mode daemon default,
//!    then `/usr/local/var/run/pulse/native` (package prefix install),
//!    then `/tmp/pulse-native`.
//!
//! Handshake sequence (native protocol, this client speaks version 35):
//!
//! 1. `AUTH` (command 8): tagstruct `U32` protocol version, optionally an
//!    `ARBITRARY` 256-byte cookie read from `$PULSE_COOKIE`,
//!    `~/.config/pulse/cookie` or `/usr/local/etc/pulse/cookie`. Without a
//!    cookie the handshake only succeeds against servers with cookie auth
//!    disabled — the server's refusal surfaces as [`PulseDaemonError::Server`].
//! 2. `SET_CLIENT_NAME` (command 9): an `application.*` proplist; the reply
//!    carries the assigned client index (read leniently).
//! 3. `QUERY_INFO`(server) (opcode 34) → `SERVER_INFO` (35) via
//!    [`PulseDaemon::server_info`] for the daemon version and default
//!    sink/source names.
//!
//! **Playback** (post-handshake): `CREATE_PLAYBACK_STREAM` (command 3) via
//! [`PulseDaemon::create_playback_stream`] sends the sample spec, channel
//! map, sink selection, default buffer metrics and the version-gated flag
//! blocks exactly as libpulse lays them out for v35; audio ships through
//! [`PulseDaemon::write_audio`] as pstream **memblock** frames (descriptor
//! channel = the stream's assigned channel, not `0xFFFFFFFF`), and teardown
//! is [`PulseDaemon::delete_playback_stream`]. The server's unsolicited
//! `REQUEST` ("send more data") notifications carry tag `0xFFFFFFFF`, so
//! the next control round-trip skips them.
//!
//! Replies to `AUTH`/`SET_CLIENT_NAME` use the generic `REPLY` opcode — the
//! protocol defines no dedicated reply opcodes for them. All I/O is blocking
//! with a 5 s timeout; drive [`PulseDaemon`] from a worker thread only.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::codec::{command, PacketHeader, PulseError, HEADER_LEN};
use crate::tags::{TagReader, TagWriter};

/// Native protocol version this client speaks (v35 — PulseAudio 14+ and
/// pipewire-pulse alike).
pub const PROTOCOL_VERSION: u32 = 35;

/// A PulseAudio auth cookie is a fixed 256-byte shared secret.
pub const AUTH_COOKIE_LEN: usize = 256;

/// Blocking I/O timeout for every daemon round-trip.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors returned by the daemon handshake client.
#[derive(Debug, thiserror::Error)]
pub enum PulseDaemonError {
    /// Socket I/O failure (connect, read/write, timeout).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The daemon sent something that does not match the protocol.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Wire codec failure while encoding/decoding a handshake message.
    #[error("codec: {0}")]
    Codec(#[from] PulseError),
    /// None of the candidate daemon sockets exist.
    #[error("no PulseAudio server socket found (tried: {})", tried.join(", "))]
    NoServerSocket {
        /// Candidate socket paths that were tried, in priority order.
        tried: Vec<String>,
    },
    /// An auth cookie file exists but is unusable.
    #[error("cookie: {0}")]
    Cookie(String),
    /// The daemon answered a request with `PA_COMMAND_ERROR`.
    #[error("pulse server error {code}: {message}")]
    Server {
        /// PulseAudio error code (e.g. 14 = `PA_ERR_ACCESS`).
        code: u32,
        /// Human-readable server diagnostics.
        message: String,
    },
}

/// Untagged sample-spec fields exactly as they appear in a `SERVER_INFO`
/// reply. The format byte is kept verbatim: the full `pa_sample_format` set
/// is wider than [`crate::codec::SampleFormat`]'s two-value P1 subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawSampleSpec {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u8,
    /// Raw `pa_sample_format` byte (e.g. 5 = FLOAT32LE, 3 = S16LE).
    pub format: u8,
}

/// A live playback stream on the daemon (index assigned at creation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackStream {
    /// Server-assigned stream index.
    pub index: u32,
}

/// Frames and writes one AUDIO memblock: 20-byte descriptor (channel =
/// stream index — memblocks are NOT 0xFFFFFFFF) + raw FLOAT32LE payload.
fn write_memblock<W: Write>(
    w: &mut W,
    channel: u32,
    samples: &[f32],
) -> Result<(), PulseDaemonError> {
    let payload_len = samples.len() * 4;
    w.write_all(&(payload_len as u32).to_be_bytes())?;
    w.write_all(&channel.to_be_bytes())?; // memblock frame: real channel id
    w.write_all(&0u64.to_be_bytes())?; // offset
    w.write_all(&0u32.to_be_bytes())?; // flags = PA_SEEK_RELATIVE
    let mut bytes = Vec::with_capacity(payload_len);
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    w.write_all(&bytes)?;
    Ok(())
}

/// Daemon introspection data parsed from a `SERVER_INFO` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// User the daemon runs as.
    pub user_name: Option<String>,
    /// Host the daemon runs on.
    pub host_name: Option<String>,
    /// Daemon version string (e.g. `"16.1.0"`).
    pub server_version: String,
    /// Daemon implementation name (e.g. `"pulseaudio"`).
    pub server_name: String,
    /// Default sink name.
    pub default_sink_name: Option<String>,
    /// Default source name.
    pub default_source_name: Option<String>,
    /// Daemon cookie id.
    pub cookie: u32,
    /// Default sink sample spec (raw wire fields).
    pub sample_spec: RawSampleSpec,
}

/// A blocking, handshake-completed connection to a PulseAudio daemon.
///
/// Created via [`connect`](Self::connect) (default socket + cookie
/// discovery) or one of its explicit-path variants. See the
/// [module docs](self) for the wire sequence.
#[derive(Debug)]
pub struct PulseDaemon {
    stream: UnixStream,
    socket_path: PathBuf,
    next_tag: u32,
    server_version: u32,
}

impl PulseDaemon {
    /// Connects to the default daemon socket and performs the handshake
    /// (`AUTH` + `SET_CLIENT_NAME`), loading the auth cookie from the
    /// standard candidate paths when present.
    ///
    /// # Errors
    ///
    /// [`PulseDaemonError::NoServerSocket`] when no candidate socket exists;
    /// see also [`connect_with_cookie`](Self::connect_with_cookie).
    pub fn connect() -> Result<Self, PulseDaemonError> {
        let cookie = load_default_cookie()?;
        let candidates = socket_candidates();
        let path = candidates.iter().find(|p| p.exists()).ok_or_else(|| {
            PulseDaemonError::NoServerSocket {
                tried: candidates.iter().map(|p| p.display().to_string()).collect(),
            }
        })?;
        Self::connect_with_cookie(path, cookie.as_deref())
    }

    /// Connects to an explicit daemon socket path and performs the same
    /// handshake (and default cookie discovery) as [`connect`](Self::connect).
    ///
    /// # Errors
    ///
    /// See [`connect_with_cookie`](Self::connect_with_cookie).
    pub fn connect_to(path: &Path) -> Result<Self, PulseDaemonError> {
        let cookie = load_default_cookie()?;
        Self::connect_with_cookie(path, cookie.as_deref())
    }

    /// Connects to `path` and performs the handshake with an explicit auth
    /// cookie (`None` sends cookie-less `AUTH` — only accepted by servers
    /// with cookie auth disabled).
    ///
    /// # Errors
    ///
    /// - [`PulseDaemonError::Io`] on connect/read/write/timeout failure,
    /// - [`PulseDaemonError::Cookie`] when `cookie` is not
    ///   [`AUTH_COOKIE_LEN`] bytes,
    /// - [`PulseDaemonError::Server`] when the daemon rejects the handshake,
    /// - [`PulseDaemonError::Protocol`] on malformed replies.
    pub fn connect_with_cookie(
        path: &Path,
        cookie: Option<&[u8]>,
    ) -> Result<Self, PulseDaemonError> {
        if let Some(cookie) = cookie {
            if cookie.len() != AUTH_COOKIE_LEN {
                return Err(PulseDaemonError::Cookie(format!(
                    "explicit cookie must be {AUTH_COOKIE_LEN} bytes, got {}",
                    cookie.len()
                )));
            }
        }

        let stream = UnixStream::connect(path)?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))?;

        let mut daemon = Self {
            stream,
            socket_path: path.to_path_buf(),
            next_tag: 1,
            server_version: 0,
        };
        daemon.auth(cookie)?;
        daemon.set_client_name()?;
        tracing::debug!(
            server_version = daemon.server_version,
            socket = %daemon.socket_path.display(),
            "pulse daemon handshake complete"
        );
        Ok(daemon)
    }

    /// The protocol version the daemon reported during `AUTH`.
    #[must_use]
    pub fn protocol_version(&self) -> u32 {
        self.server_version
    }

    /// The socket path this daemon connection uses.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Round-trips one raw control packet: sends `cmd` plus `body` (raw
    /// tagstruct bytes, no command/tag words) framed with the next request
    /// tag, and returns the reply's opcode and tagstruct payload. Intended
    /// for tests and protocol probing.
    ///
    /// # Errors
    ///
    /// See [`connect_with_cookie`](Self::connect_with_cookie); a `u16` reply
    /// opcode is assumed ([`PulseDaemonError::Protocol`] otherwise).
    pub fn roundtrip(&mut self, cmd: u16, body: &[u8]) -> Result<(u16, Vec<u8>), PulseDaemonError> {
        let frame = self.request(u32::from(cmd), body)?;
        let command = u16::try_from(frame.command)
            .map_err(|_| PulseDaemonError::Protocol("reply opcode exceeds u16".to_owned()))?;
        Ok((command, frame.payload))
    }

    /// Creates a playback stream (`CREATE_PLAYBACK_STREAM`, protocol v35
    /// payload — name lives in the proplist for v≥13).
    ///
    /// `sink`: `None` selects the default sink. Interleaved FLOAT32LE audio
    /// is then delivered via [`write_audio`](Self::write_audio).
    ///
    /// # Errors
    ///
    /// See [`connect_with_cookie`](Self::connect_with_cookie).
    pub fn create_playback_stream(
        &mut self,
        name: &str,
        sink: Option<&str>,
        rate: u32,
        channels: u8,
    ) -> Result<PlaybackStream, PulseDaemonError> {
        if self.server_version < 13 {
            return Err(PulseDaemonError::Protocol(
                "playback streams need protocol >= 13".to_owned(),
            ));
        }
        let mut w = TagWriter::new();
        // Field order per protocol-native.c command_create_playback_stream:
        // sample_spec, channel_map, sink_index, sink_name, buffer attr
        // (maxlength, corked, tlength, prebuf, minreq), syncid, cvolume,
        // v>=12 flag booleans, v>=13 (muted, adjust_latency, proplist),
        // v>=14 (volume_set, early_requests), v>=15 (muted_set,
        // dont_inhibit_auto_suspend, fail_on_suspend), v>=17 relative_volume,
        // v>=18 passthrough, v>=21 n_formats.
        w.sample_spec(rate, channels, command::SAMPLE_FLOAT32LE);
        // Default channel map: mono → [0], stereo → [0, 1] (mono/stereo
        // positions), otherwise sequential channel indices.
        let positions: Vec<u8> = (0..channels).collect();
        w.channel_map(&positions);
        w.u32(u32::MAX); // sink_index = default
        match sink {
            Some(s) => {
                w.string(s);
            }
            None => {
                w.string_null();
            }
        }
        w.u32(u32::MAX); // maxlength: default
        w.boolean(false); // corked
        w.u32(u32::MAX); // tlength: default
        w.u32(u32::MAX); // prebuf: default
        w.u32(u32::MAX); // minreq: default
        w.u32(0); // syncid
                  // cvolume: channel count + norm volume per channel.
        w.cvolume(&vec![command::VOLUME_NORM; channels as usize]);
        // v>=12 booleans: no_remap..variable_rate (7).
        for _ in 0..7 {
            w.boolean(false);
        }
        // v>=13: muted, adjust_latency, proplist.
        w.boolean(false); // muted
        w.boolean(false); // adjust_latency
        w.proplist(&[
            ("media.name", name),
            ("application.name", "sonicbrew gw-pulse"),
        ]);
        // v>=14: volume_set, early_requests.
        w.boolean(true); // volume_set
        w.boolean(false); // early_requests
                          // v>=15: muted_set, dont_inhibit_auto_suspend, fail_on_suspend.
        w.boolean(false);
        w.boolean(false);
        w.boolean(false);
        // v>=17: relative_volume; v>=18: passthrough.
        w.boolean(false);
        w.boolean(false);
        // v>=21: explicit format-info list (empty → negotiated ss above).
        w.u8(0); // n_formats = 0

        let frame = self.request(command::CREATE_PLAYBACK_STREAM, &w.into_bytes())?;
        if frame.command != command::REPLY {
            return Err(PulseDaemonError::Protocol(format!(
                "CREATE_PLAYBACK_STREAM: expected REPLY, got {}",
                frame.command
            )));
        }
        let mut r = TagReader::new(&frame.payload);
        let index = r.read_u32()?;
        Ok(PlaybackStream { index })
    }

    /// Streams interleaved FLOAT32LE samples as a pstream memblock frame.
    ///
    /// Fire-and-forget raw write (no reply expected); the server's flow
    /// control (`REQUEST` packets) is deliberately ignored — writers that
    /// overrun the server buffer experience server-side drops, which is the
    /// documented MVP behaviour for this bridge.
    ///
    /// # Errors
    ///
    /// [`PulseDaemonError::Io`] on socket write failure.
    pub fn write_audio(
        &mut self,
        stream: &PlaybackStream,
        samples: &[f32],
    ) -> Result<(), PulseDaemonError> {
        write_memblock(&mut self.stream, stream.index, samples)
    }

    /// Closes a playback stream (`DELETE_PLAYBACK_STREAM`).
    ///
    /// # Errors
    ///
    /// See [`connect_with_cookie`](Self::connect_with_cookie).
    pub fn delete_playback_stream(
        &mut self,
        stream: &PlaybackStream,
    ) -> Result<(), PulseDaemonError> {
        let mut w = TagWriter::new();
        w.u32(stream.index);
        let frame = self.request(command::DELETE_PLAYBACK_STREAM, &w.into_bytes())?;
        if frame.command != command::REPLY {
            return Err(PulseDaemonError::Protocol(format!(
                "DELETE_PLAYBACK_STREAM: expected REPLY, got {}",
                frame.command
            )));
        }
        Ok(())
    }

    /// Queries daemon introspection data (`QUERY_INFO`(server) →
    /// `SERVER_INFO`).
    ///
    /// # Errors
    ///
    /// See [`connect_with_cookie`](Self::connect_with_cookie).
    pub fn server_info(&mut self) -> Result<ServerInfo, PulseDaemonError> {
        // GET_SERVER_INFO takes no arguments — the reply arrives as a
        // REPLY (2) carrying the server-info tagstruct body.
        let frame = self.request(command::QUERY_INFO, &[])?;
        if frame.command != command::SERVER_INFO {
            return Err(PulseDaemonError::Protocol(format!(
                "expected SERVER_INFO ({}), got {}",
                command::SERVER_INFO,
                frame.command
            )));
        }

        let mut r = TagReader::new(&frame.payload);
        let user_name = r.read_string()?;
        let host_name = r.read_string()?;
        let server_version = r.read_string()?.unwrap_or_default();
        let server_name = r.read_string()?.unwrap_or_default();
        let (sample_rate, channels, format) = r.read_sample_spec()?;
        let default_sink_name = r.read_string()?;
        let default_source_name = r.read_string()?;
        let cookie = r.read_u32()?;
        // Servers append a channel map once the negotiated version >= 15;
        // skip it when present (best effort — trailing fields are ignored).
        if self.server_version >= 15 && !r.is_empty() {
            r.skip_channel_map()?;
        }

        Ok(ServerInfo {
            user_name,
            host_name,
            server_version,
            server_name,
            default_sink_name,
            default_source_name,
            cookie,
            sample_spec: RawSampleSpec {
                sample_rate,
                channels,
                format,
            },
        })
    }

    /// Handshake step 1: `AUTH` (protocol version + optional cookie).
    fn auth(&mut self, cookie: Option<&[u8]>) -> Result<(), PulseDaemonError> {
        let mut tagstruct = TagWriter::new();
        tagstruct.u32(PROTOCOL_VERSION);
        if let Some(cookie) = cookie {
            tagstruct.arbitrary(cookie);
        }
        let frame = self.request(command::AUTH, &tagstruct.into_bytes())?;
        if frame.command != command::REPLY {
            return Err(PulseDaemonError::Protocol(format!(
                "AUTH: expected REPLY ({}), got {}",
                command::REPLY,
                frame.command
            )));
        }

        let mut r = TagReader::new(&frame.payload);
        self.server_version = r.read_u32()?;
        // Trailing fields (SHM id etc., version-dependent) are deliberately
        // ignored — only the negotiated version matters to this client.
        Ok(())
    }

    /// Handshake step 2: `SET_CLIENT_NAME` with an `application.*` proplist.
    fn set_client_name(&mut self) -> Result<(), PulseDaemonError> {
        let pid = std::process::id().to_string();
        let mut tagstruct = TagWriter::new();
        tagstruct.proplist(&[
            ("application.name", "sonicbrew gw-pulse"),
            ("application.process.id", pid.as_str()),
        ]);
        let frame = self.request(command::SET_CLIENT_NAME, &tagstruct.into_bytes())?;
        if frame.command != command::REPLY {
            return Err(PulseDaemonError::Protocol(format!(
                "SET_CLIENT_NAME: expected REPLY ({}), got {}",
                command::REPLY,
                frame.command
            )));
        }
        // The reply carries the assigned client index (version >= 16); read
        // it only when present so empty replies still pass.
        if !frame.payload.is_empty() {
            let mut r = TagReader::new(&frame.payload);
            r.read_u32()?;
        }
        Ok(())
    }

    /// Sends one request and awaits one reply, validating the echoed tag and
    /// translating `ERROR`/`TIMEOUT` replies.
    fn request(&mut self, command: u32, tagstruct: &[u8]) -> Result<Frame, PulseDaemonError> {
        let tag = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1);

        write_frame(&mut self.stream, command, tag, tagstruct)?;
        // Servers interleave unsolicited packets (REGISTER_MEMFD_SHMID with
        // ancillary fds, ENABLE_SRBCHANNEL probes, subscription events)
        // before the reply — skip frames until OUR tag comes back.
        let deadline = std::time::Instant::now() + IO_TIMEOUT;
        loop {
            if std::time::Instant::now() > deadline {
                return Err(PulseDaemonError::Io(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "timed out skipping unsolicited frames",
                )));
            }
            let frame = read_frame(&mut self.stream)?;
            if frame.tag != tag {
                continue; // unsolicited (memfd/srbchannel/subscription) — skip
            }
            if frame.command == command::ERROR || frame.command == command::TIMEOUT {
                return Err(error_reply(&frame));
            }
            return Ok(frame);
        }
    }
}

/// One parsed control frame: reply opcode, echoed tag and tagstruct payload.
struct Frame {
    command: u32,
    tag: u32,
    payload: Vec<u8>,
}

/// Frames and writes one control packet: 20-byte descriptor (reusing
/// [`HEADER_LEN`]'s layout) + a tagstruct whose FIRST TWO u32 fields are the
/// command and tag (pdispatch parses the entire payload as one tagstruct —
/// see `pa_pdispatch_run` / `pa_pstream_send_tagstruct`).
fn write_frame<W: Write>(
    w: &mut W,
    command: u32,
    tag: u32,
    tagstruct: &[u8],
) -> std::io::Result<()> {
    // Payload = 'L'command + 'L'tag + the caller's tagstruct.
    let mut payload = Vec::with_capacity(10 + tagstruct.len());
    payload.extend_from_slice(&[crate::tags::TAG_U32]);
    payload.extend_from_slice(&command.to_be_bytes());
    payload.extend_from_slice(&[crate::tags::TAG_U32]);
    payload.extend_from_slice(&tag.to_be_bytes());
    payload.extend_from_slice(tagstruct);

    w.write_all(
        &u32::try_from(payload.len())
            .expect("tagstruct fits in u32")
            .to_be_bytes(),
    )?;
    // Packet frames MUST carry channel (uint32_t)-1 (0xFFFFFFFF): pstream.c
    // treats every other channel value as a MEMBLOCK (audio) frame and
    // silently consumes it — no reply ever comes back.
    w.write_all(&u32::MAX.to_be_bytes())?; // channel = -1 = control packet
    w.write_all(&0u64.to_be_bytes())?; // offset (memblock positioning only)
    w.write_all(&0u32.to_be_bytes())?; // flags
    w.write_all(&payload)
}

/// Reads one control packet and splits the leading opcode/tag words.
fn read_frame<R: Read>(r: &mut R) -> Result<Frame, PulseDaemonError> {
    let mut descriptor = [0u8; HEADER_LEN];
    r.read_exact(&mut descriptor)?;
    // Reuses the P1 parser: decodes the descriptor and sanity-checks the
    // declared body length via `is_valid`.
    let header = PacketHeader::parse(&descriptor)?;
    if !header.is_valid() {
        return Err(PulseDaemonError::Protocol(format!(
            "invalid frame length {}",
            header.length
        )));
    }

    let mut body = vec![0u8; header.length as usize];
    r.read_exact(&mut body)?;
    // The payload IS one tagstruct: first two u32 fields are command + tag.
    let mut reader = TagReader::new(&body);
    let command = reader
        .read_u32()
        .map_err(|e| PulseDaemonError::Protocol(format!("reply command field: {e:?}")))?;
    let tag = reader
        .read_u32()
        .map_err(|e| PulseDaemonError::Protocol(format!("reply tag field: {e:?}")))?;
    let payload = body[reader.offset().min(body.len())..].to_vec();
    Ok(Frame {
        command,
        tag,
        payload,
    })
}

/// Parses an `ERROR`/`TIMEOUT` reply body (`u32` code + message string) into
/// a [`PulseDaemonError::Server`], tolerating malformed bodies.
fn error_reply(frame: &Frame) -> PulseDaemonError {
    let mut r = TagReader::new(&frame.payload);
    let code = r.read_u32().unwrap_or(0);
    let message = r
        .read_string()
        .ok()
        .flatten()
        .unwrap_or_else(|| "no message".to_owned());
    PulseDaemonError::Server { code, message }
}

fn env_str(name: &str) -> Option<String> {
    std::env::var_os(name).and_then(|value| value.into_string().ok())
}

/// Candidate daemon sockets, in priority order (see module docs).
///
/// Pure variant of [`socket_candidates`] for testability.
fn socket_candidates_from(
    pulse_server: Option<&str>,
    xdg_runtime_dir: Option<&str>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = pulse_server
        .and_then(|value| value.strip_prefix("unix:"))
        .filter(|path| !path.is_empty())
    {
        candidates.push(PathBuf::from(path));
    }
    if let Some(dir) = xdg_runtime_dir.filter(|dir| !dir.is_empty()) {
        candidates.push(PathBuf::from(dir).join("pulse").join("native"));
    }
    // FreeBSD package defaults: a system-mode (or root-launched) daemon
    // exposes its native socket under /var/run/pulse; per-user installs
    // sometimes land in the package prefix instead.
    candidates.push(PathBuf::from("/var/run/pulse/native"));
    candidates.push(PathBuf::from("/usr/local/var/run/pulse/native"));
    candidates.push(PathBuf::from("/tmp/pulse-native"));
    candidates
}

/// Candidate daemon sockets from the process environment, in priority order.
///
/// After the fixed candidates, scans `/tmp` for PulseAudio runtime
/// directories (`pulse-*`/`pulse-<pid>-<rand>` containing a `native`
/// socket) — created when `XDG_RUNTIME_DIR` is unset, as on a FreeBSD
/// root-launched daemon.
#[must_use]
pub fn socket_candidates() -> Vec<PathBuf> {
    let mut candidates = socket_candidates_from(
        env_str("PULSE_SERVER").as_deref(),
        env_str("XDG_RUNTIME_DIR").as_deref(),
    );
    candidates.extend(scan_tmp_pulse_runtimes());
    candidates
}

/// Finds `/tmp/pulse-*/native` unix sockets (PulseAudio runtime dirs).
fn scan_tmp_pulse_runtimes() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("pulse-"))
        })
        .map(|p| p.join("native"))
        .filter(|p| {
            std::fs::metadata(p)
                .map(|m| {
                    use std::os::unix::fs::FileTypeExt;
                    m.file_type().is_socket()
                })
                .unwrap_or(false)
        })
        .collect();
    // Deterministic order regardless of readdir order.
    found.sort();
    found
}

/// Candidate auth-cookie paths, in priority order. Pure variant of
/// [`cookie_candidates`] for testability.
fn cookie_candidates_from(pulse_cookie: Option<&str>, home: Option<&str>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = pulse_cookie.filter(|path| !path.is_empty()) {
        candidates.push(PathBuf::from(path));
    }
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        candidates.push(
            PathBuf::from(home)
                .join(".config")
                .join("pulse")
                .join("cookie"),
        );
    }
    candidates.push(PathBuf::from("/usr/local/etc/pulse/cookie"));
    candidates
}

/// Candidate auth-cookie paths from the process environment, in priority
/// order: `$PULSE_COOKIE`, `~/.config/pulse/cookie`,
/// `/usr/local/etc/pulse/cookie` (FreeBSD package default).
#[must_use]
pub fn cookie_candidates() -> Vec<PathBuf> {
    cookie_candidates_from(
        env_str("PULSE_COOKIE").as_deref(),
        env_str("HOME").as_deref(),
    )
}

/// Reads an auth cookie file. `Ok(None)` means "not found — send cookie-less
/// AUTH"; any present file must be exactly [`AUTH_COOKIE_LEN`] bytes.
///
/// # Errors
///
/// [`PulseDaemonError::Cookie`] for a present-but-malformed file;
/// [`PulseDaemonError::Io`] for non-`NotFound` read errors.
pub fn load_cookie_file(path: &Path) -> Result<Option<Vec<u8>>, PulseDaemonError> {
    match fs::read(path) {
        Ok(bytes) => {
            if bytes.len() == AUTH_COOKIE_LEN {
                Ok(Some(bytes))
            } else {
                Err(PulseDaemonError::Cookie(format!(
                    "{}: expected {AUTH_COOKIE_LEN} bytes, got {}",
                    path.display(),
                    bytes.len()
                )))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(PulseDaemonError::Io(err)),
    }
}

/// Loads the first usable cookie from [`cookie_candidates`], if any.
fn load_default_cookie() -> Result<Option<Vec<u8>>, PulseDaemonError> {
    for candidate in cookie_candidates() {
        if let Some(cookie) = load_cookie_file(&candidate)? {
            return Ok(Some(cookie));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;

    /// Handshake + introspection helper mirror of [`write_frame`].
    fn reply(stream: &mut UnixStream, command: u32, tag: u32, tagstruct: &[u8]) {
        write_frame(stream, command, tag, tagstruct).expect("fake server write");
    }

    fn next_request(stream: &mut UnixStream) -> Frame {
        read_frame(stream).expect("fake server read")
    }

    fn unique_socket_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gw-pulse-daemon-{label}-{}-{n}.sock",
            std::process::id()
        ))
    }

    fn write_temp_file(label: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gw-pulse-cookie-{label}-{}-{}.bin",
            std::process::id(),
            label.len()
        ));
        fs::write(&path, bytes).expect("temp cookie write");
        path
    }

    /// Fake-server `AUTH` step: validates the client body (version + cookie)
    /// and answers with the negotiated version plus a dummy SHM blob
    /// (servers send one from protocol v13 — exercises ignore-the-rest).
    fn fake_auth(stream: &mut UnixStream, expect_cookie: Option<usize>) {
        let frame = next_request(stream);
        assert_eq!(frame.command, command::AUTH);
        let mut r = TagReader::new(&frame.payload);
        assert_eq!(r.read_u32().expect("version"), PROTOCOL_VERSION);
        match expect_cookie {
            Some(len) => {
                let cookie = r.read_arbitrary().expect("cookie blob");
                assert_eq!(cookie.len(), len);
            }
            None => {
                // Cookie optional: only present when a default one was found.
                if !r.is_empty() {
                    let cookie = r.read_arbitrary().expect("cookie blob");
                    assert_eq!(cookie.len(), AUTH_COOKIE_LEN);
                }
            }
        }

        let mut tagstruct = TagWriter::new();
        tagstruct.u32(35).arbitrary(&[0xAB; 4]);
        reply(stream, command::REPLY, frame.tag, &tagstruct.into_bytes());
    }

    /// Fake-server `SET_CLIENT_NAME` step: validates the proplist shape and
    /// answers with a client index.
    fn fake_set_client_name(stream: &mut UnixStream) {
        let frame = next_request(stream);
        assert_eq!(frame.command, command::SET_CLIENT_NAME);
        let mut r = TagReader::new(&frame.payload);
        let props = r.read_proplist().expect("proplist");
        assert!(
            props
                .iter()
                .any(|(key, value)| key == "application.name" && value.contains("sonicbrew")),
            "application.name missing from {props:?}"
        );

        let mut tagstruct = TagWriter::new();
        tagstruct.u32(42);
        reply(stream, command::REPLY, frame.tag, &tagstruct.into_bytes());
    }

    #[test]
    fn socket_candidates_priority_order() {
        let candidates =
            socket_candidates_from(Some("unix:/custom/pulse.sock"), Some("/run/user/1000"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/custom/pulse.sock"),
                PathBuf::from("/run/user/1000/pulse/native"),
                PathBuf::from("/var/run/pulse/native"),
                PathBuf::from("/usr/local/var/run/pulse/native"),
                PathBuf::from("/tmp/pulse-native"),
            ]
        );

        // Non-unix PULSE_SERVER values are ignored; XDG then /tmp apply.
        let candidates = socket_candidates_from(Some("tcp:pulsehost:4713"), Some("/run/user/7"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/run/user/7/pulse/native"),
                PathBuf::from("/var/run/pulse/native"),
                PathBuf::from("/usr/local/var/run/pulse/native"),
                PathBuf::from("/tmp/pulse-native"),
            ]
        );

        // No environment at all: FreeBSD system-mode defaults, then /tmp.
        let candidates = socket_candidates_from(None, None);
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/var/run/pulse/native"),
                PathBuf::from("/usr/local/var/run/pulse/native"),
                PathBuf::from("/tmp/pulse-native"),
            ]
        );
    }

    #[test]
    fn cookie_file_loading_validation() {
        let good = write_temp_file("good", &[0x33; AUTH_COOKIE_LEN]);
        assert_eq!(
            load_cookie_file(&good).expect("valid cookie"),
            Some(vec![0x33; AUTH_COOKIE_LEN])
        );

        let bad = write_temp_file("bad", &[0u8; 16]);
        let err = load_cookie_file(&bad).expect_err("wrong size rejected");
        assert!(matches!(err, PulseDaemonError::Cookie(_)));

        let missing = std::env::temp_dir().join("gw-pulse-cookie-definitely-absent.bin");
        assert_eq!(load_cookie_file(&missing).expect("missing ok"), None);

        let _ = fs::remove_file(&good);
        let _ = fs::remove_file(&bad);
    }

    #[test]
    fn handshake_against_fake_server() {
        let path = unique_socket_path("handshake");
        let listener = UnixListener::bind(&path).expect("bind fake server");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            fake_auth(&mut stream, Some(AUTH_COOKIE_LEN));
            fake_set_client_name(&mut stream);

            // One raw round-trip probe (PA_COMMAND_STAT = 13): echo a reply
            // with a u32+u64 body.
            let frame = next_request(&mut stream);
            assert_eq!(frame.command, 13);
            let mut tagstruct = TagWriter::new();
            tagstruct.u32(7).u64(9);
            reply(
                &mut stream,
                command::REPLY,
                frame.tag,
                &tagstruct.into_bytes(),
            );
        });

        let mut daemon = PulseDaemon::connect_with_cookie(&path, Some(&[0x11; AUTH_COOKIE_LEN]))
            .expect("handshake");
        assert_eq!(daemon.protocol_version(), 35);
        assert_eq!(daemon.socket_path(), path.as_path());

        let (reply_command, payload) = daemon.roundtrip(13, &[]).expect("roundtrip");
        assert_eq!(
            reply_command,
            u16::try_from(command::REPLY).expect("fits u16")
        );
        let mut r = TagReader::new(&payload);
        assert_eq!(r.read_u32().expect("u32"), 7);
        assert_eq!(r.read_u64().expect("u64"), 9);
        assert!(r.is_empty());

        server.join().expect("fake server thread");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn server_info_against_fake_server() {
        let path = unique_socket_path("info");
        let listener = UnixListener::bind(&path).expect("bind fake server");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            fake_auth(&mut stream, None);
            fake_set_client_name(&mut stream);

            let frame = next_request(&mut stream);
            assert_eq!(frame.command, command::QUERY_INFO);
            // GET_SERVER_INFO carries an EMPTY tagstruct.
            assert!(frame.payload.is_empty());

            let mut tagstruct = TagWriter::new();
            tagstruct
                .string("pulseuser")
                .string("fbsdstation")
                .string("16.1.0")
                .string("pulseaudio")
                .sample_spec(48_000, 2, 5)
                .string("alsa_output.pci-0000_03_00.1.analog-stereo")
                .string("alsa_input.pci-0000_03_00.1.analog-stereo")
                .u32(0x00C0_FFEE)
                .channel_map(&[0, 1]);
            reply(
                &mut stream,
                command::SERVER_INFO,
                frame.tag,
                &tagstruct.into_bytes(),
            );
        });

        let mut daemon = PulseDaemon::connect_with_cookie(&path, None).expect("handshake");
        let info = daemon.server_info().expect("server info");
        assert_eq!(info.user_name.as_deref(), Some("pulseuser"));
        assert_eq!(info.host_name.as_deref(), Some("fbsdstation"));
        assert_eq!(info.server_version, "16.1.0");
        assert_eq!(info.server_name, "pulseaudio");
        assert_eq!(
            info.default_sink_name.as_deref(),
            Some("alsa_output.pci-0000_03_00.1.analog-stereo")
        );
        assert_eq!(
            info.default_source_name.as_deref(),
            Some("alsa_input.pci-0000_03_00.1.analog-stereo")
        );
        assert_eq!(info.cookie, 0x00C0_FFEE);
        assert_eq!(
            info.sample_spec,
            RawSampleSpec {
                sample_rate: 48_000,
                channels: 2,
                format: 5,
            }
        );

        server.join().expect("fake server thread");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn server_error_reply_fails_handshake() {
        let path = unique_socket_path("error");
        let listener = UnixListener::bind(&path).expect("bind fake server");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Reject AUTH outright: PA_ERR_ACCESS-style error.
            let frame = next_request(&mut stream);
            assert_eq!(frame.command, command::AUTH);
            let mut tagstruct = TagWriter::new();
            tagstruct.u32(14).string("Access denied");
            reply(
                &mut stream,
                command::ERROR,
                frame.tag,
                &tagstruct.into_bytes(),
            );
        });

        // Exercises connect_to's default-cookie discovery path too.
        let err = PulseDaemon::connect_to(&path).expect_err("handshake rejected");
        match err {
            PulseDaemonError::Server { code, message } => {
                assert_eq!(code, 14);
                assert_eq!(message, "Access denied");
            }
            other => panic!("expected Server error, got {other:?}"),
        }

        server.join().expect("fake server thread");
        let _ = fs::remove_file(&path);
    }
}
