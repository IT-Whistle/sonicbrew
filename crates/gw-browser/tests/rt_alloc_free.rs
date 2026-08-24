//! RT-safety verification for the M12 browser gateway's ring nodes.
//!
//! `BrowserGateway::register` inserts a `RingSource` (inbound from browser)
//! and a `RingSink` (outbound to browser) into the graph. When the audio
//! engine drives `Graph::process_cycle` on the RT thread, those nodes must
//! perform **zero** heap allocations.
//!
//! Uses the thread-local counting global allocator pattern established in
//! `audio-graph-bsd/tests/rt_alloc_free.rs` (TESTING-STANDARDS §3.2 /
//! BUILD-PLAN §4.3). The build/compile phase allocates freely; the
//! measurement window brackets ONLY the `process_cycle` loop.
//!
//! # Why thread-local counters
//!
//! A global `MEASURING` flag is unreliable in a `cargo test` harness: the
//! libtest harness performs bookkeeping allocations on background threads
//! that intermittently land inside the measurement window, producing false
//! positives. Per-thread counting ensures the RT-thread measurement is
//! deterministic regardless of what other threads do.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use audio_core_bsd::{AudioFrame, ProcessContext};
use audio_graph_bsd::GraphConfig;
use gw_browser::{BrowserGateway, Graph};

// ---------------------------------------------------------------------------
// Counting global allocator with THREAD-LOCAL measurement windows.
// ---------------------------------------------------------------------------

struct CountingAllocator;

thread_local! {
    /// Per-thread "am I currently measuring?" flag.
    static RT_MEASURING: Cell<bool> = const { Cell::new(false) };
    /// Per-thread allocation counter (only incremented while measuring).
    static RT_ALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with` returns Err before this thread's locals exist (e.g. during
        // their own lazy init, causing re-entrancy). In that case the measuring
        // flag is necessarily unset, so we skip counting — safe and correct.
        let _ = RT_MEASURING.try_with(|m| {
            if m.get() {
                let _ = RT_ALLOC_COUNT.try_with(|c| c.set(c.get().saturating_add(1)));
            }
        });
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static A: CountingAllocator = CountingAllocator;

/// Enables per-thread counting and resets the counter to zero.
fn rt_start_measuring() {
    RT_ALLOC_COUNT.with(|c| c.set(0));
    RT_MEASURING.with(|m| m.set(true));
}

/// Disables per-thread counting and returns how many allocations were observed.
fn rt_stop_and_count() -> usize {
    RT_MEASURING.with(|m| m.set(false));
    RT_ALLOC_COUNT.with(|c| c.get())
}

// ---------------------------------------------------------------------------
// The alloc=0 proof.
// ---------------------------------------------------------------------------

/// `Graph::process_cycle` through a `BrowserGateway`-registered graph must
/// perform **zero** heap allocations across 1000 cycles.
///
/// The graph topology is `RingSource → RingSink` (a pass-through), wired by
/// `BrowserGateway::register`. The inbound `rtrb` ring is pre-filled outside
/// the measurement window so `RingSource` always has a frame to pop.
/// `RingSink::flush` (the worker-thread clone-and-ship path) is deliberately
/// NOT called inside the loop — it is off-RT.
#[test]
fn process_cycle_through_gateway_is_alloc_free() {
    const NUM_FRAMES: usize = 256;
    const CHANNELS: u16 = 2;
    const SAMPLE_RATE: u32 = 48_000;

    // -- Build phase (allocations allowed) --------------------------------
    let mut graph = Graph::new();
    let gw = BrowserGateway::new();
    let (src, sink, mut inbound, _outbound) = gw
        .register(&mut graph)
        .expect("register wires RingSource + RingSink");
    graph.link((src, 0), (sink, 0)).expect("link src→sink");
    graph
        .compile(GraphConfig::new(NUM_FRAMES, SAMPLE_RATE, CHANNELS))
        .expect("compile");

    // Pre-fill the inbound ring so RingSource has frames to pop during the
    // measured loop (done OUTSIDE the measurement window).
    let fill_frame = AudioFrame::from_planar(
        CHANNELS,
        SAMPLE_RATE,
        vec![0.0; NUM_FRAMES * usize::from(CHANNELS)],
    );
    for _ in 0..64 {
        let _ = inbound.push(fill_frame.clone());
    }

    let mut ctx = ProcessContext::new(NUM_FRAMES, 0, SAMPLE_RATE);

    // -- Warmup: 2 cycles (allow initial scratch settle) -----------------
    for _ in 0..2 {
        graph.process_cycle(&mut ctx).expect("warmup process_cycle");
    }

    // -- Measurement window: ONLY process_cycle ---------------------------
    rt_start_measuring();
    for _ in 0..1000 {
        graph.process_cycle(&mut ctx).expect("process_cycle");
    }
    let n = rt_stop_and_count();

    assert_eq!(
        n, 0,
        "RT path (BrowserGateway RingSource→RingSink): process_cycle allocated {n} times across 1000 cycles — RT-safety violation (TESTING-STANDARDS §3.2)"
    );
}
