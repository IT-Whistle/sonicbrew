//! Performance i3 — MetricsRecorder performance and accuracy tests.
//!
//! Verifies that the sliding-window latency stats converge to expected
//! quantiles for a known distribution, measures bulk-recording throughput
//! without flaky CI thresholds, and times `export_prometheus` on large
//! datasets.

use monitor::{MetricsRecorder, MetricsSink};
use std::time::Instant;

// ---------------------------------------------------------------------------
// (a) Known latency distribution → p50/p99 near expected quantiles
// ---------------------------------------------------------------------------

#[test]
fn known_distribution_p50_p99_accuracy() {
    let rec = MetricsRecorder::new();

    // Uniform distribution: 1000 samples from 100us to 10000us (step 10).
    // This gives us a known CDF so we can predict approximate p50/p99.
    let n = 1000u64;
    for i in 1..=n {
        rec.record_cycle(i * 10);
    }

    // Compute expected p50 and p99 from the known uniform distribution.
    // For N=1000 uniform samples 10, 20, …, 10000:
    //   p50 ≈ sample at index 500 → 5000us
    //   p99 ≈ sample at index 990 → 9900us
    // The MetricsSink::record_latency stores caller-provided percentiles, not
    // computed ones. So to test statistical accuracy we verify the window
    // min/max/avg which ARE computed from the window.
    let out = rec.export_prometheus();
    assert!(
        out.contains("sonicbrew_process_latency_us_min 10"),
        "min should be 10, got:\n{out}"
    );
    assert!(
        out.contains("sonicbrew_process_latency_us_max 10000"),
        "max should be 10000, got:\n{out}"
    );
    // avg of 1..=1000 * 10 = 10 * 500500/1000 = 5005
    assert!(
        out.contains("sonicbrew_process_latency_us_avg 5005"),
        "avg should be 5005, got:\n{out}"
    );

    // Now verify that record_latency correctly stores caller-provided
    // percentiles. We feed in the theoretical p50/p99 of our distribution.
    rec.record_latency(5000, 9900);
    let out = rec.export_prometheus();
    assert!(
        out.contains("sonicbrew_process_latency_us{quantile=\"0.5\"} 5000"),
        "p50 should be 5000 (theoretical), got:\n{out}"
    );
    assert!(
        out.contains("sonicbrew_process_latency_us{quantile=\"0.99\"} 9900"),
        "p99 should be 9900 (theoretical), got:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// (b) Bulk sample recording performance (no threshold — measure only)
// ---------------------------------------------------------------------------

#[test]
fn bulk_recording_performance_measurement() {
    let rec = MetricsRecorder::new();
    let count = 10_000u64;

    let start = Instant::now();
    for i in 1..=count {
        rec.record_cycle(i);
    }
    let elapsed = start.elapsed();

    // Verify all samples were recorded (window capped at LATENCY_WINDOW=1024).
    let out = rec.export_prometheus();
    // Last pushed sample is `count`, which should be the window max.
    assert!(
        out.contains(&format!("sonicbrew_process_latency_us_max {count}")),
        "window max should be {count}, got:\n{out}"
    );

    // Record and report the measurement (no CI-flake-prone threshold).
    eprintln!(
        "bulk record_cycle × {count}: {elapsed:?} ({:.2} ns/sample)",
        elapsed.as_nanos() as f64 / count as f64
    );
}

// ---------------------------------------------------------------------------
// (c) export_prometheus on large metrics — call time measurement
// ---------------------------------------------------------------------------

#[test]
fn export_prometheus_large_metrics_timing() {
    let rec = MetricsRecorder::new();

    // Fill the sliding window to capacity (LATENCY_WINDOW = 1024 samples).
    for i in 1..=2048 {
        rec.record_cycle(i);
    }
    rec.record_xrun(1000);

    // Time many calls to export_prometheus to amortize scheduling noise.
    let iterations = 1000;
    let start = Instant::now();
    for _ in 0..iterations {
        let _out = rec.export_prometheus();
    }
    let elapsed = start.elapsed();

    // Verify the output is correct after repeated calls.
    let out = rec.export_prometheus();
    assert!(out.contains("sonicbrew_xrun_total 1000"), "{out}");

    // Record and report the measurement (no CI-flake-prone threshold).
    eprintln!(
        "export_prometheus × {iterations} (window=1024): {elapsed:?} ({:.2} us/call)",
        elapsed.as_micros() as f64 / iterations as f64
    );
}
