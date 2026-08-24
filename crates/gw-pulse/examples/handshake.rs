//! Connects to a running PulseAudio server via [`PulseDaemon::connect`],
//! completes the AUTH + SET_CLIENT_NAME handshake, and prints the server
//! info. Exit code 0 = handshake succeeded.
//!
//! `handshake --play` additionally opens a playback stream and pushes
//! 3 seconds of a 440 Hz sine (FLOAT32LE stereo) — the full client-side
//! audio path (CREATE_PLAYBACK_STREAM + memblock writes + teardown).

use gw_pulse::daemon::PulseDaemon;

fn main() {
    let play = std::env::args().nth(1).as_deref() == Some("--play");
    match PulseDaemon::connect() {
        Ok(mut daemon) => {
            println!("handshake OK via {}", daemon.socket_path().display());
            println!("negotiated protocol version: {}", daemon.protocol_version());
            match daemon.server_info() {
                Ok(info) => {
                    println!("server: {}", info.server_name);
                    println!("version: {}", info.server_version);
                    println!("default sink: {:?}", info.default_sink_name);
                    println!("default source: {:?}", info.default_source_name);
                }
                Err(e) => {
                    eprintln!("handshake OK but server_info failed: {e}");
                    std::process::exit(2);
                }
            }
            if play {
                play_sine(&mut daemon);
            }
        }
        Err(e) => {
            eprintln!("handshake FAILED: {e}");
            std::process::exit(1);
        }
    }
}

/// 3 s of 440 Hz sine, 48 kHz stereo FLOAT32LE, in 480-frame blocks.
fn play_sine(daemon: &mut PulseDaemon) {
    const RATE: u32 = 48_000;
    const BLOCK: usize = 480;
    const BLOCKS: usize = RATE as usize * 3 / BLOCK;

    let stream = match daemon.create_playback_stream("sonicbrew-play-test", None, RATE, 2) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("create_playback_stream FAILED: {e}");
            std::process::exit(3);
        }
    };
    println!("playback stream created: index={}", stream.index);

    let mut phase: f32 = 0.0;
    let step = std::f32::consts::TAU * 440.0 / RATE as f32;
    let mut written = 0_usize;
    for _ in 0..BLOCKS {
        // Interleaved stereo block.
        let mut block = Vec::with_capacity(BLOCK * 2);
        for _ in 0..BLOCK {
            let s = phase.sin() * 0.3;
            block.push(s);
            block.push(s);
            phase += step;
            if phase >= std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
        }
        match daemon.write_audio(&stream, &block) {
            Ok(()) => written += BLOCK,
            Err(e) => {
                eprintln!("write_audio FAILED after {written} frames: {e}");
                break;
            }
        }
        // ~10 ms per block — rough real-time pacing so the null sink is not
        // overrun before its 2 s tlength absorbs the burst.
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
    println!("wrote {written} frames");

    match daemon.delete_playback_stream(&stream) {
        Ok(()) => println!("PLAY_OK"),
        Err(e) => {
            eprintln!("delete_playback_stream FAILED: {e}");
            std::process::exit(4);
        }
    }
}
