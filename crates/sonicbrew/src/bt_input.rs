//! Bluetooth A2DP input bridge (optional, behind the `bluetooth` feature).
//!
//! Connects an [`audio_bluetooth_bsd::BtInputSource`] (a FreeBSD
//! `virtual_bt_speaker` daemon + OSS→rtrb worker wrapped as an
//! [`audio_io_bsd::InputSource`]) to an `audio-graph-bsd` inbound ring via a
//! dedicated worker thread.
//!
//! # Data path
//!
//! ```text
//! BtInputSource::read  →  rtrb::Producer<AudioFrame>  →  RingSource (graph)
//! (worker thread)         (this bridge)                  (RT thread, wait-free pop)
//! ```
//!
//! The bridge thread loops calling [`InputSource::read`] and pushes each
//! returned frame into the ring with **drop-on-full** semantics (never blocks
//! the worker). On a read error it logs at `warn` and sleeps briefly — the
//! FreeBSD daemon/OSS path may be absent or transient.
//!
//! # Platform
//!
//! Runtime is FreeBSD-only (the daemon and OSS leaf are
//! `cfg(target_os="freebsd")`-gated inside `audio-bluetooth-bsd`). This module
//! type-checks on Linux for CI; actually constructing a `BtInputSource` on
//! Linux yields silence frames (the OSS read leaf is stubbed).

use std::thread::{self, JoinHandle};
use std::time::Duration;

use audio_bluetooth_bsd::worker::BtInputSource;
use audio_bluetooth_bsd::{AudioFrame, InputSource};

/// Brief back-off after a read error before retrying. Keeps the worker from
/// hot-looping when the daemon/OSS path is absent.
const READ_ERROR_BACKOFF: Duration = Duration::from_millis(20);

/// Spawns a worker thread that drains `source` into `producer`.
///
/// On each successful [`InputSource::read`] the frame is pushed to the ring;
/// a full ring drops the frame (no blocking). On a read error the thread logs
/// at `warn` and sleeps [`READ_ERROR_BACKOFF`] before retrying. The returned
/// [`JoinHandle`] joins the worker; dropping `source` (which drops its
/// `Consumer`) signals the inner daemon worker to exit.
pub fn spawn_bt_to_ring(
    mut source: BtInputSource,
    mut producer: rtrb::Producer<AudioFrame>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("sonicbrew-bt-in".into())
        .spawn(move || loop {
            match source.read() {
                Ok(frame) => {
                    // Drop-on-full: never block the worker thread. A full ring
                    // means the RT consumer is behind; dropping the oldest-new
                    // frame is preferable to stalling capture.
                    if producer.push(frame).is_err() {
                        tracing::trace!(
                            target: "sonicbrew::bt_input",
                            "BT input ring full — frame dropped"
                        );
                    }
                }
                Err(e) => {
                    // The FreeBSD daemon/OSS path may be absent or transient
                    // (especially on Linux where the OSS read leaf is stubbed).
                    // Log and back off rather than hot-looping.
                    tracing::warn!(
                        target: "sonicbrew::bt_input",
                        error = %e,
                        "BtInputSource::read failed; retrying after back-off"
                    );
                    thread::sleep(READ_ERROR_BACKOFF);
                }
            }
        })
        .expect("spawning the BT input worker thread succeeds under normal conditions")
}

/// Returns a short human-readable summary of the Bluetooth A2DP input
/// integration path, for `--help` / doc surfaces. This does NOT pull the
/// `bluetooth` feature into the default build — it is a plain `&'static str`
/// accessible only when the feature is enabled.
#[must_use]
pub fn describe_integration() -> &'static str {
    "Bluetooth A2DP input: BtInputSource (FreeBSD virtual_bt_speaker + OSS) \
     → rtrb ring → RingSource (graph). FreeBSD runtime; Linux type-check only."
}
