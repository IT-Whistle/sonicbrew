//! Integration i3 — MetricsRecorder integration tests.
//!
//! Verifies statistical accuracy of recorded latency samples, xrun counting,
//! Prometheus exposition format compliance, and the raw-HTTP `serve_metrics`
//! endpoint end-to-end.

use monitor::{MetricsRecorder, MetricsSink};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------------------
// (a) Deterministic latency samples → p50/p99/min/max/avg accuracy
// ---------------------------------------------------------------------------

#[test]
fn latency_stats_accuracy_deterministic() {
    let rec = MetricsRecorder::new();

    // Push 100 samples: 100us, 200us, …, 10000us via record_cycle.
    for i in 1..=100 {
        rec.record_cycle(i * 100);
    }

    let out = rec.export_prometheus();

    // Window-based stats (computed from all 100 samples).
    assert!(
        out.contains("sonicbrew_process_latency_us_min 100"),
        "min should be 100, got:\n{out}"
    );
    assert!(
        out.contains("sonicbrew_process_latency_us_max 10000"),
        "max should be 10000, got:\n{out}"
    );
    // avg = sum(100..=10000 step 100) / 100 = 505000 / 100 = 5050
    assert!(
        out.contains("sonicbrew_process_latency_us_avg 5050"),
        "avg should be 5050, got:\n{out}"
    );

    // p50/p99 quantile lines: record_cycle does NOT set last_p50/last_p99,
    // so the stored values remain 0. The quantile lines come from atomic
    // loads, not from window computation.
    assert!(
        out.contains("sonicbrew_process_latency_us{quantile=\"0.5\"} 0"),
        "p50 should be 0 (record_cycle doesn't set it), got:\n{out}"
    );
    assert!(
        out.contains("sonicbrew_process_latency_us{quantile=\"0.99\"} 0"),
        "p99 should be 0 (record_cycle doesn't set it), got:\n{out}"
    );
}

#[test]
fn latency_stats_with_record_latency_sets_p50_p99() {
    let rec = MetricsRecorder::new();

    // record_latency stores p50/p99 in atomics AND records both into the
    // window.
    rec.record_latency(500, 9500);
    rec.record_latency(600, 9800);

    let out = rec.export_prometheus();

    // Most-recent p50/p99 are the last call's values.
    assert!(
        out.contains("sonicbrew_process_latency_us{quantile=\"0.5\"} 600"),
        "last p50 should be 600, got:\n{out}"
    );
    assert!(
        out.contains("sonicbrew_process_latency_us{quantile=\"0.99\"} 9800"),
        "last p99 should be 9800, got:\n{out}"
    );

    // Window now has 4 samples: 500, 9500, 600, 9800.
    assert!(
        out.contains("sonicbrew_process_latency_us_min 500"),
        "min should be 500, got:\n{out}"
    );
    assert!(
        out.contains("sonicbrew_process_latency_us_max 9800"),
        "max should be 9800, got:\n{out}"
    );
    // avg = (500 + 9500 + 600 + 9800) / 4 = 20400 / 4 = 5100
    assert!(
        out.contains("sonicbrew_process_latency_us_avg 5100"),
        "avg should be 5100, got:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// (b) xrun count record → query
// ---------------------------------------------------------------------------

#[test]
fn xrun_count_record_and_query() {
    let rec = MetricsRecorder::new();

    // Initially zero.
    let out = rec.export_prometheus();
    assert!(
        out.contains("sonicbrew_xrun_total 0"),
        "initial xrun should be 0, got:\n{out}"
    );

    // Record xrun events.
    rec.record_xrun(5);
    let out = rec.export_prometheus();
    assert!(
        out.contains("sonicbrew_xrun_total 5"),
        "xrun should be 5, got:\n{out}"
    );

    // Accumulate.
    rec.record_xrun(12);
    let out = rec.export_prometheus();
    assert!(
        out.contains("sonicbrew_xrun_total 12"),
        "xrun should be 12, got:\n{out}"
    );

    // Stale (smaller) report must not decrease the counter.
    rec.record_xrun(3);
    let out = rec.export_prometheus();
    assert!(
        out.contains("sonicbrew_xrun_total 12"),
        "stale report must not decrease counter, got:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// (c) export_prometheus() output contains required metric lines
// ---------------------------------------------------------------------------

#[test]
fn prometheus_output_contains_required_metric_lines() {
    let rec = MetricsRecorder::new();
    rec.record_latency(100, 500);
    rec.record_xrun(3);

    let out = rec.export_prometheus();

    // Required HELP / TYPE directives.
    assert!(out.contains("# HELP sonicbrew_process_latency_us"), "{out}");
    assert!(
        out.contains("# TYPE sonicbrew_process_latency_us summary"),
        "{out}"
    );
    assert!(out.contains("# HELP sonicbrew_xrun_total"), "{out}");
    assert!(out.contains("# TYPE sonicbrew_xrun_total counter"), "{out}");

    // Required quantile sample lines.
    assert!(
        out.contains("sonicbrew_process_latency_us{quantile=\"0.5\"}"),
        "{out}"
    );
    assert!(
        out.contains("sonicbrew_process_latency_us{quantile=\"0.99\"}"),
        "{out}"
    );

    // Required min / max / avg lines.
    assert!(out.contains("sonicbrew_process_latency_us_min"), "{out}");
    assert!(out.contains("sonicbrew_process_latency_us_max"), "{out}");
    assert!(out.contains("sonicbrew_process_latency_us_avg"), "{out}");

    // Required counter line.
    assert!(out.contains("sonicbrew_xrun_total 3"), "{out}");

    // Verify parseability: every non-empty, non-comment line must contain
    // a space-separated metric name and numeric value.
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
        assert_eq!(
            parts.len(),
            2,
            "non-comment line must be 'name value': {trimmed}"
        );
        let val: f64 = parts[1]
            .parse()
            .unwrap_or_else(|e| panic!("value must be numeric: {trimmed} — {e}"));
        assert!(val >= 0.0, "metric value must be non-negative: {trimmed}");
    }
}

// ---------------------------------------------------------------------------
// (d) serve_metrics HTTP endpoint: bind → GET /metrics → 200 + body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn serve_metrics_http_endpoint() {
    // Bind to port 0 (OS-assigned) to avoid conflicts.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener); // Free the port for serve_metrics.

    let rec = Arc::new(MetricsRecorder::new());
    rec.record_latency(123, 456);
    rec.record_xrun(7);

    let recorder = Arc::clone(&rec);
    let server = tokio::spawn(async move {
        monitor::serve_metrics(addr, recorder)
            .await
            .expect("serve_metrics");
    });

    // Give the server a moment to start listening.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect with a tokio TcpStream (async I/O).
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");

    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write request");

    // Read until EOF (server sends Connection: close).
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");

    let response_str = String::from_utf8_lossy(&response);

    // Status line must be 200 OK.
    assert!(
        response_str.contains("HTTP/1.1 200 OK"),
        "expected 200 OK, got:\n{response_str}"
    );

    // Content-Type must be text/plain.
    assert!(
        response_str.contains("Content-Type: text/plain"),
        "expected text/plain content type, got:\n{response_str}"
    );

    // Body must contain the recorded metrics.
    assert!(
        response_str.contains("sonicbrew_process_latency_us"),
        "body must contain latency metrics, got:\n{response_str}"
    );
    assert!(
        response_str.contains("sonicbrew_xrun_total 7"),
        "body must contain xrun count, got:\n{response_str}"
    );
    assert!(
        response_str.contains("sonicbrew_process_latency_us{quantile=\"0.5\"} 123"),
        "body must contain p50=123, got:\n{response_str}"
    );
    assert!(
        response_str.contains("sonicbrew_process_latency_us{quantile=\"0.99\"} 456"),
        "body must contain p99=456, got:\n{response_str}"
    );

    server.abort();
}
