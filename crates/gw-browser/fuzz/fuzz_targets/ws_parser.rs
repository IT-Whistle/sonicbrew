//! libFuzzer target for the gw-browser wire codec (Sanitizer i5).
//!
//! Feeds arbitrary bytes straight into `decode_frame`. The proptest suite
//! (tests/codec_fuzz.rs) already proves no-panic on random input; this target
//! adds coverage-guided fuzzing to reach deeper parser states.
//!
//! Run: `cargo +nightly fuzz -p gw-browser ws_parser` (from workspace root)
//!      or `cargo fuzz run ws_parser` (from crates/gw-browser/fuzz/)

#![no_main]

use gw_browser::{decode_frame, FrameSpec};
use libfuzzer_sys::fuzz_target;

const SPEC: FrameSpec = FrameSpec::new(2, 48_000);

fuzz_target!(|data: &[u8]| {
    // Must never panic: every malformed input yields Ok or BadFrame.
    let _ = decode_frame(data, SPEC);
});
