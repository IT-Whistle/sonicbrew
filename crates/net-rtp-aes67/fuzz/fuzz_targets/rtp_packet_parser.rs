//! libFuzzer target for the net-rtp-aes67 RTP packet parser (Sanitizer i5).
//!
//! Feeds arbitrary bytes into `RtpPacket::parse`. The proptest suite
//! (tests/rtp_fuzz.rs) proves no-panic on random input; coverage-guided
//! fuzzing here chases deeper header/payload edge cases (CC overflow, padding,
//! extension, payload-type whitelist bypass attempts).
//!
//! Run: `cargo +nightly fuzz -p net-rtp-aes67 rtp_packet_parser`

#![no_main]

use libfuzzer_sys::fuzz_target;
use net_rtp_aes67::RtpPacket;

fuzz_target!(|data: &[u8]| {
    // Must never panic: every malformed packet yields Ok or Err.
    let _ = RtpPacket::parse(data);
});
