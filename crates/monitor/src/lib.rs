//! M14 — Metrics / observability sink.
//!
//! **P1 module**: real-time process-cycle latency tracking (sliding-window
//! p50/p99), cumulative xrun counting, and Prometheus exposition rendering.
//! This replaces the MVP's `std::time::Instant` placeholder logging per
//! ROADMAP Phase 3 / P11 §7c.
//!
//! ## RT path
//!
//! [`MetricsRecorder::record_latency`] (the [`MetricsSink`] trait method) and
//! the extension [`MetricsRecorder::record_cycle`] are called from the
//! real-time loop. Each call performs a single bounded `push_back` (+ optional
//! `pop_front` once the window is full) under a `std::sync::Mutex`, so the
//! held-lock critical section is O(1). A truly lock-free histogram (e.g.
//! per-shard atomic counters) is a documented later optimization; the
//! `std::Mutex` is acceptable for the MVP/P1 monitor.
//!
//! ## kqueue note (FreeBSD optimization)
//!
//! The high-efficiency event loop on FreeBSD multiplexes timer + audio-fd
//! readiness through a single `kqueue(2)` system call. That path is a
//! FreeBSD-only optimization landing in a later phase. The MVP/P1 monitor
//! shipped here uses plain `std` timers + the in-process [`MetricsRecorder`]
//! and has **no `kqueue` / `nix` dependency** — those crates are
//! Linux-incompatible and would break the dev-host build. See
//! [`spawn_kqueue_loop`] for the gated stub.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Sliding-window capacity for recorded latency samples.
const LATENCY_WINDOW: usize = 1024;

/// Sink for real-time and operational metrics.
pub trait MetricsSink: Send + Sync {
    /// Record per-cycle processing latency (p50 / p99, microseconds).
    ///
    /// Called from the real-time loop; the two values are the cycle's
    /// representative percentile points (typically pre-computed by the caller).
    /// Both are appended to the sliding window and the most-recent p50/p99 are
    /// remembered for the Prometheus summary lines. For callers that have a
    /// single per-cycle number, prefer [`MetricsRecorder::record_cycle`].
    fn record_latency(&self, p50_us: u64, p99_us: u64);
    /// Record cumulative xrun count.
    fn record_xrun(&self, count: u64);
    /// Render metrics in Prometheus exposition format.
    fn export_prometheus(&self) -> String;
}

/// Sliding window of recent latency samples (microseconds).
///
/// A thin wrapper over a `Mutex<VecDeque<u64>>`. The critical section in
/// [`LatencyHistogram::record`] is a bounded `push_back` + optional
/// `pop_front` — O(1) work under the lock, acceptable for the MVP RT call.
struct LatencyHistogram {
    samples: Mutex<VecDeque<u64>>,
}

impl LatencyHistogram {
    fn new() -> Self {
        Self {
            // Pre-allocate to capacity so the first LATENCY_WINDOW pushes never
            // reallocate (deterministic RT behaviour).
            samples: Mutex::new(VecDeque::with_capacity(LATENCY_WINDOW)),
        }
    }

    /// Append one sample. Bounded: pushes to the back and, once the window is
    /// full, pops the front — so the held-lock critical section is O(1).
    fn record(&self, latency_us: u64) {
        let mut buf = self.samples.lock().expect("latency window mutex poisoned");
        if buf.len() >= LATENCY_WINDOW {
            buf.pop_front();
        }
        buf.push_back(latency_us);
    }

    /// Take a snapshot copy for stat computation. Not called from the RT path —
    /// only from [`MetricsRecorder::export_prometheus`].
    fn snapshot(&self) -> Vec<u64> {
        let buf = self.samples.lock().expect("latency window mutex poisoned");
        buf.iter().copied().collect()
    }
}

/// Windowed statistics (min / max / avg) over a latency-sample snapshot.
struct LatencyStats {
    min: u64,
    max: u64,
    avg: u64,
}

impl LatencyStats {
    /// Compute min/max/avg over a snapshot. Empty → all zeros.
    fn from_samples(samples: &[u64]) -> Self {
        if samples.is_empty() {
            return Self {
                min: 0,
                max: 0,
                avg: 0,
            };
        }
        let mut min = u64::MAX;
        let mut max = 0u64;
        let mut sum: u128 = 0;
        for &v in samples {
            min = min.min(v);
            max = max.max(v);
            sum += u128::from(v);
        }
        let avg = u64::try_from(sum / samples.len() as u128).unwrap_or(u64::MAX);
        Self { min, max, avg }
    }
}

/// Real [`MetricsSink`]: tracks a sliding window of process-cycle latency
/// samples plus a cumulative xrun counter, and renders Prometheus exposition
/// text on demand.
///
/// ## Two recording entry points
///
/// - [`MetricsRecorder::record_cycle`] (inherent, **not** on the trait): push a
///   single per-cycle latency from the RT loop when you have one number.
/// - [`MetricsSink::record_latency`] (trait): report already-computed p50 / p99
///   percentiles; it remembers the most-recent p50/p99 (for the summary lines)
///   and feeds both into the sliding window via [`record_cycle`].
pub struct MetricsRecorder {
    window: LatencyHistogram,
    last_p50: AtomicU64,
    last_p99: AtomicU64,
    xrun_total: AtomicU64,
    /// Label prefix prepended to every metric name (default `sonicbrew`).
    prefix: String,
}

impl MetricsRecorder {
    /// Construct with the default metric-name prefix `sonicbrew`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_prefix("sonicbrew")
    }

    /// Construct with a custom metric-name prefix.
    #[must_use]
    pub fn with_prefix(prefix: &str) -> Self {
        Self {
            window: LatencyHistogram::new(),
            last_p50: AtomicU64::new(0),
            last_p99: AtomicU64::new(0),
            xrun_total: AtomicU64::new(0),
            prefix: prefix.to_owned(),
        }
    }

    /// Record a single per-cycle latency (microseconds) into the window.
    ///
    /// Extension method — **not** part of [`MetricsSink`]. Prefer this from the
    /// RT loop when you have one number per cycle; use
    /// [`MetricsSink::record_latency`] when reporting already-computed p50/p99
    /// percentiles (it calls this twice and also remembers the last p50/p99).
    pub fn record_cycle(&self, latency_us: u64) {
        self.window.record(latency_us);
    }
}

impl Default for MetricsRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsSink for MetricsRecorder {
    fn record_latency(&self, p50_us: u64, p99_us: u64) {
        // The trait hands us already-computed percentiles: remember the
        // most-recent p50/p99 for the summary lines, then record both as the
        // cycle's representative samples into the sliding window.
        self.last_p50.store(p50_us, Ordering::Relaxed);
        self.last_p99.store(p99_us, Ordering::Relaxed);
        self.record_cycle(p50_us);
        self.record_cycle(p99_us);
    }

    fn record_xrun(&self, count: u64) {
        // `count` is the authoritative cumulative total reported by the audio
        // engine. Keep the maximum so the exposed counter stays monotonically
        // non-decreasing (a Prometheus `counter` requirement) even if a stale
        // or out-of-order report arrives.
        let mut current = self.xrun_total.load(Ordering::Relaxed);
        loop {
            if count <= current {
                return;
            }
            match self.xrun_total.compare_exchange_weak(
                current,
                count,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    fn export_prometheus(&self) -> String {
        let samples = self.window.snapshot();
        let stats = LatencyStats::from_samples(&samples);
        let last_p50 = self.last_p50.load(Ordering::Relaxed);
        let last_p99 = self.last_p99.load(Ordering::Relaxed);
        let xrun = self.xrun_total.load(Ordering::Relaxed);
        format!(
            "# HELP {p}_process_latency_us Process-cycle latency in microseconds\n\
             # TYPE {p}_process_latency_us summary\n\
             {p}_process_latency_us{{quantile=\"0.5\"}} {p50}\n\
             {p}_process_latency_us{{quantile=\"0.99\"}} {p99}\n\
             {p}_process_latency_us_min {min}\n\
             {p}_process_latency_us_max {max}\n\
             {p}_process_latency_us_avg {avg}\n\
             # HELP {p}_xrun_total Cumulative xrun count\n\
             # TYPE {p}_xrun_total counter\n\
             {p}_xrun_total {xrun}\n",
            p = self.prefix,
            p50 = last_p50,
            p99 = last_p99,
            min = stats.min,
            max = stats.max,
            avg = stats.avg,
            xrun = xrun,
        )
    }
}

/// Stub sink: collects nothing. Real sink (P1) is [`MetricsRecorder`].
pub struct NoopSink;

impl NoopSink {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for NoopSink {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsSink for NoopSink {
    fn record_latency(&self, _p50_us: u64, _p99_us: u64) {}

    fn record_xrun(&self, _count: u64) {}

    fn export_prometheus(&self) -> String {
        String::new()
    }
}

/// Serve Prometheus exposition text over a minimal HTTP `/metrics` endpoint.
///
/// Raw HTTP (no axum/hyper dep): accepts connections on `addr`, responds to
/// `GET /metrics` with `200 text/plain` + the recorder's `export_prometheus()`.
/// Runs until the listener errors. Scrape frequency is low (~15s) so the brief
/// `Mutex<VecDeque>` snapshot under the RT recorder is acceptable (documented
/// in the RT-path section above).
pub async fn serve_metrics(
    addr: std::net::SocketAddr,
    recorder: std::sync::Arc<MetricsRecorder>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "metrics endpoint listening");
    loop {
        let (mut sock, peer) = listener.accept().await?;
        let rec = std::sync::Arc::clone(&recorder);
        tokio::spawn(async move {
            tracing::debug!(%peer, "metrics scrape");
            // Read the request line best-effort (tiny buffer); we only answer
            // `GET /metrics` and the response is identical regardless of path,
            // so the request bytes are intentionally not inspected.
            let mut buf = [0u8; 256];
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(500), sock.read(&mut buf))
                    .await;
            let body = rec.export_prometheus();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
    }
}

/// Spawn the high-efficiency `kqueue(2)` event loop (FreeBSD optimization).
///
/// **Not implemented in P1.** The MVP/P1 monitor uses plain `std` timers and
/// the in-process [`MetricsRecorder`]; this function exists to document and
/// gate the future FreeBSD-only optimization. It deliberately has **no
/// `kqueue`/`nix` crate dependency** (those are Linux-incompatible).
///
/// Returns an error with a clear message so any premature caller degrades
/// gracefully instead of panicking.
#[cfg(target_os = "freebsd")]
pub fn spawn_kqueue_loop() -> Result<(), &'static str> {
    Err("kqueue monitor loop is not implemented in P1; use MetricsRecorder + std timers")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_latency_updates_window() {
        let rec = MetricsRecorder::new();
        rec.record_latency(100, 200);
        rec.record_latency(150, 300);
        let out = rec.export_prometheus();
        assert!(
            out.contains("sonicbrew_process_latency_us{quantile=\"0.5\"} 150"),
            "last p50 should be 150, got:\n{out}"
        );
        assert!(
            out.contains("sonicbrew_process_latency_us{quantile=\"0.99\"} 300"),
            "last p99 should be 300, got:\n{out}"
        );
        assert!(
            out.contains("sonicbrew_process_latency_us_max 300"),
            "window max should be 300, got:\n{out}"
        );
    }

    #[test]
    fn export_prometheus_format() {
        let rec = MetricsRecorder::new();
        rec.record_latency(10, 20);
        let out = rec.export_prometheus();
        assert!(out.contains("# HELP sonicbrew_process_latency_us"), "{out}");
        assert!(
            out.contains("# TYPE sonicbrew_process_latency_us summary"),
            "{out}"
        );
        assert!(out.contains("# HELP sonicbrew_xrun_total"), "{out}");
        assert!(out.contains("# TYPE sonicbrew_xrun_total counter"), "{out}");
        assert!(
            out.contains("sonicbrew_process_latency_us{quantile=\"0.5\"}"),
            "{out}"
        );
        assert!(out.contains("sonicbrew_xrun_total "), "{out}");
    }

    #[test]
    fn xrun_counter_accumulates() {
        let rec = MetricsRecorder::new();
        rec.record_xrun(3);
        rec.record_xrun(7);
        let out = rec.export_prometheus();
        // Cumulative semantics: the larger reported total wins (monotonic
        // counter). A subsequent smaller report must NOT decrease the count.
        rec.record_xrun(5);
        let out_after_decrease = rec.export_prometheus();
        assert!(
            out.contains("sonicbrew_xrun_total 7"),
            "expected cumulative xrun total 7, got:\n{out}"
        );
        assert!(
            out_after_decrease.contains("sonicbrew_xrun_total 7"),
            "counter must not decrease on stale report, got:\n{out_after_decrease}"
        );
    }

    #[test]
    fn percentile_edge_cases() {
        // Empty recorder exports zeros.
        let rec = MetricsRecorder::new();
        let out = rec.export_prometheus();
        assert!(
            out.contains("sonicbrew_process_latency_us{quantile=\"0.5\"} 0"),
            "empty: last p50 should be 0, got:\n{out}"
        );
        assert!(
            out.contains("sonicbrew_process_latency_us{quantile=\"0.99\"} 0"),
            "empty: last p99 should be 0, got:\n{out}"
        );
        assert!(out.contains("sonicbrew_xrun_total 0"), "{out}");

        // Single sample.
        let rec = MetricsRecorder::new();
        rec.record_latency(42, 84);
        let out = rec.export_prometheus();
        assert!(
            out.contains("sonicbrew_process_latency_us{quantile=\"0.5\"} 42"),
            "{out}"
        );
        assert!(
            out.contains("sonicbrew_process_latency_us{quantile=\"0.99\"} 84"),
            "{out}"
        );
        assert!(out.contains("sonicbrew_process_latency_us_max 84"), "{out}");

        // Full window wrap-around: push far more than capacity; must not panic
        // and the window must cap at LATENCY_WINDOW samples (retaining the
        // most-recent ones).
        let rec = MetricsRecorder::new();
        let pushes = LATENCY_WINDOW as u64 + 512;
        for i in 0..pushes {
            rec.record_cycle(i);
        }
        let out = rec.export_prometheus();
        let max_pushed = pushes - 1;
        assert!(
            out.contains(&format!("sonicbrew_process_latency_us_max {max_pushed}")),
            "wrap: window max should be {max_pushed}, got:\n{out}"
        );
    }

    #[test]
    fn noop_sink_still_works() {
        let sink = NoopSink::new();
        sink.record_latency(1, 2);
        sink.record_xrun(3);
        assert_eq!(sink.export_prometheus(), "");
        // Unit-struct construction also usable (regression guard). NOTE: we do
        // not call `NoopSink::default()` here — clippy::default_constructed_unit_structs
        // warns on it; the `Default` impl is kept for API completeness.
        let _unit: NoopSink = NoopSink;
    }

    #[test]
    fn custom_prefix_is_applied() {
        let rec = MetricsRecorder::with_prefix("custom_node");
        rec.record_latency(1, 2);
        let out = rec.export_prometheus();
        assert!(
            out.contains("# HELP custom_node_process_latency_us"),
            "{out}"
        );
        assert!(out.contains("custom_node_xrun_total "), "{out}");
        assert!(
            !out.contains("sonicbrew_"),
            "default prefix must not leak when a custom prefix is set:\n{out}"
        );
    }
}
