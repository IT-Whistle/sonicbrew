//! Integration i4 — real-socket WebSocket server round-trip (heatmap
//! Protocol/Integration 심화: 실제 소켓 바인딩 + 바이너리 프레임 왕복).
//!
//! Unlike the inline `lib.rs` loopback tests (which bypass the audio graph and
//! use tiny mono frames), these tests exercise the **full end-to-end pipeline**
//! over a real `tokio-tungstenite` socket: browser client → WS binary frame →
//! codec decode → rtrb ring → `RingSource` → `Graph::process_cycle` →
//! `RingSink::flush` → rtrb ring → WS binary frame → client, with full
//! 256-sample stereo PCM integrity verification.
//!
//! Also covers protocol edge cases: text-frame tolerance and abrupt client
//! disconnect — verifying the server's accept loop survives both.

use std::time::Duration;

use audio_core_bsd::{AudioFrame, ProcessContext};
use audio_graph_bsd::{Graph, GraphConfig, RingSink, RingSource};
use futures_util::{SinkExt, StreamExt};
use gw_browser::{decode_frame, encode_frame, BrowserGateway, FrameSpec};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const CHANNELS: u16 = 2;
const SAMPLE_RATE: u32 = 48_000;
const NUM_FRAMES: usize = 256;
const SPEC: FrameSpec = FrameSpec::new(CHANNELS, SAMPLE_RATE);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Builds a passthrough graph (`RingSource` → `RingSink`) wired to two rtrb
/// rings, and returns the graph plus the worker-thread ring handles that the
/// WS server will own (the opposite ends of the graph's ring nodes).
fn build_passthrough_graph() -> (
    Graph,
    rtrb::Producer<AudioFrame>,
    rtrb::Consumer<AudioFrame>,
) {
    let mut g = Graph::new();
    let (inbound_prod, inbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(128);
    let (outbound_prod, outbound_cons) = rtrb::RingBuffer::<AudioFrame>::new(128);

    let src = g.add_node(Box::new(RingSource::new(
        inbound_cons,
        CHANNELS,
        SAMPLE_RATE,
        NUM_FRAMES,
    )));
    let sink = g.add_sink(Box::new(RingSink::new(
        outbound_prod,
        CHANNELS,
        SAMPLE_RATE,
        NUM_FRAMES,
    )));
    g.link((src, 0), (sink, 0)).expect("link src->sink");
    g.compile(GraphConfig::new(NUM_FRAMES, SAMPLE_RATE, CHANNELS))
        .expect("compile");

    (g, inbound_prod, outbound_cons)
}

/// Generates a deterministic 256-sample stereo test signal (sine + ramp)
/// that is distinct from all-zero silence, so we can distinguish the real
/// frame from silence emitted by `RingSource` on empty-ring cycles.
fn test_samples() -> Vec<f32> {
    (0..NUM_FRAMES * usize::from(CHANNELS))
        .map(|i| {
            let phase = (i as f32) / (NUM_FRAMES as f32) * std::f32::consts::TAU;
            phase.sin() * 0.5 + (i as f32) * 0.001
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Full end-to-end real-socket round-trip through a live audio graph
// ---------------------------------------------------------------------------

/// Connects a real WS client, sends a 256-sample stereo PCM binary frame,
/// drives the graph (`process_cycle` + `flush_sinks`) until the frame
/// propagates back, and verifies sample-level integrity.
#[tokio::test]
async fn ws_binary_frame_round_trips_over_real_socket() {
    let (mut graph, inbound_prod, outbound_cons) = build_passthrough_graph();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let gw = BrowserGateway::new(); // stereo / 48 kHz / 256 frames
    let serve_task = tokio::spawn(async move {
        let _ = gw
            .serve_with_listener(listener, inbound_prod, outbound_cons)
            .await;
    });

    // --- connect a real WS client ---
    let (client, _resp) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(format!("ws://{addr}")),
    )
    .await
    .expect("client connect timeout")
    .expect("client connect ok");
    let (mut c_sink, mut c_src) = client.split();

    // --- send a 256-sample stereo PCM binary frame ---
    let samples = test_samples();
    let sent = AudioFrame::from_planar(CHANNELS, SAMPLE_RATE, samples.clone());
    let wire = encode_frame(&sent).expect("encode");
    c_sink
        .send(Message::Binary(wire))
        .await
        .expect("client send binary");

    // --- drive the graph until the frame round-trips ---
    //
    // The server pushes the decoded frame to the inbound ring asynchronously.
    // Until it arrives, `RingSource` emits silence, which `RingSink` stashes
    // and the server dutifully sends back. We keep cycling until the client
    // receives a frame whose samples match the original (i.e. not silence).
    let echoed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            // Let the server task process the inbound WS message.
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;

            // Process one graph cycle + flush.
            let mut ctx = ProcessContext::new(NUM_FRAMES, 0, SAMPLE_RATE);
            let _ = graph.process_cycle(&mut ctx);
            graph.flush_sinks();

            // Check for a binary response from the server (short poll).
            if let Ok(Some(Ok(Message::Binary(b)))) =
                tokio::time::timeout(Duration::from_millis(50), c_src.next()).await
            {
                if let Ok(decoded) = decode_frame(&b, SPEC) {
                    if decoded.samples == samples {
                        return b;
                    }
                }
                // Silence from an earlier cycle — keep looping.
            }
        }
    })
    .await
    .expect("round-trip timeout (frame did not propagate within 5 s)");

    // --- verify full sample integrity ---
    let decoded = decode_frame(&echoed, SPEC).expect("decode echoed frame");
    assert_eq!(decoded.channels, CHANNELS);
    assert_eq!(decoded.sample_rate, SAMPLE_RATE);
    assert_eq!(decoded.samples.len(), samples.len());
    for (i, (got, want)) in decoded.samples.iter().zip(&samples).enumerate() {
        assert!(
            (got - want).abs() < 1e-6,
            "sample[{i}]: got {got}, want {want}"
        );
    }

    // Cleanup.
    drop(c_sink);
    drop(c_src);
    serve_task.abort();
}

// ---------------------------------------------------------------------------
// 2. Text-frame tolerance: server ignores text, binary still works after
// ---------------------------------------------------------------------------

/// The server only accepts binary frames; text frames are silently dropped
/// (`Some(Ok(_other)) => { /* ignore */ }`). This test verifies that after
/// sending a text frame, the connection stays alive and a subsequent binary
/// frame is processed correctly.
#[tokio::test]
async fn ws_server_tolerates_text_frame_and_still_processes_binary() {
    let (mut graph, inbound_prod, outbound_cons) = build_passthrough_graph();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let gw = BrowserGateway::new();
    let serve_task = tokio::spawn(async move {
        let _ = gw
            .serve_with_listener(listener, inbound_prod, outbound_cons)
            .await;
    });

    let (client, _resp) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(format!("ws://{addr}")),
    )
    .await
    .expect("client connect timeout")
    .expect("client connect ok");
    let (mut c_sink, mut c_src) = client.split();

    // --- send a TEXT frame (should be ignored, not crash the server) ---
    c_sink
        .send(Message::Text("hello-not-a-binary-frame".into()))
        .await
        .expect("client send text");

    // Give the server time to receive and ignore the text frame.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // --- send a BINARY frame afterwards — connection must still work ---
    let samples = test_samples();
    let sent = AudioFrame::from_planar(CHANNELS, SAMPLE_RATE, samples.clone());
    let wire = encode_frame(&sent).expect("encode");
    c_sink
        .send(Message::Binary(wire))
        .await
        .expect("client send binary after text");

    // Drive the graph until the binary frame round-trips.
    let echoed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;

            let mut ctx = ProcessContext::new(NUM_FRAMES, 0, SAMPLE_RATE);
            let _ = graph.process_cycle(&mut ctx);
            graph.flush_sinks();

            if let Ok(Some(Ok(Message::Binary(b)))) =
                tokio::time::timeout(Duration::from_millis(50), c_src.next()).await
            {
                if let Ok(decoded) = decode_frame(&b, SPEC) {
                    if decoded.samples == samples {
                        return b;
                    }
                }
            }
        }
    })
    .await
    .expect("binary round-trip timeout after text frame");

    let decoded = decode_frame(&echoed, SPEC).expect("decode");
    assert_eq!(
        decoded.samples, samples,
        "binary frame must survive after text"
    );

    drop(c_sink);
    drop(c_src);
    serve_task.abort();
}

// ---------------------------------------------------------------------------
// 3. Abrupt client disconnect: server survives and accepts new connections
// ---------------------------------------------------------------------------

/// A client that drops without a WS Close handshake must not panic the server.
/// After the disconnect, the accept loop must continue to serve new clients.
#[tokio::test]
async fn ws_server_handles_disconnect_and_accepts_new_client() {
    let (mut graph, inbound_prod, outbound_cons) = build_passthrough_graph();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let gw = BrowserGateway::new();
    let serve_task = tokio::spawn(async move {
        let _ = gw
            .serve_with_listener(listener, inbound_prod, outbound_cons)
            .await;
    });

    // --- first client: connect then drop abruptly ---
    let (client1, _resp) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(format!("ws://{addr}")),
    )
    .await
    .expect("client1 connect timeout")
    .expect("client1 connect ok");

    // Abrupt disconnect: drop both halves without sending a Close frame.
    let (mut c1_sink, c1_src) = client1.split();
    // Send something to exercise the inbound path, then vanish.
    let silence = AudioFrame::silence(CHANNELS, NUM_FRAMES, SAMPLE_RATE);
    let _ = c1_sink
        .send(Message::Binary(encode_frame(&silence).expect("encode")))
        .await;
    drop(c1_sink);
    drop(c1_src);

    // Give the server time to notice the disconnect.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Process any pending frame from client1 (drains the inbound ring so
    // it doesn't interfere with client2's frame).
    let mut ctx = ProcessContext::new(NUM_FRAMES, 0, SAMPLE_RATE);
    let _ = graph.process_cycle(&mut ctx);
    graph.flush_sinks();

    // --- second client: the accept loop must still be alive ---
    let (client2, _resp) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(format!("ws://{addr}")),
    )
    .await
    .expect("client2 connect timeout")
    .expect("client2 connect ok — server survived disconnect");
    let (mut c2_sink, mut c2_src) = client2.split();

    // Verify client2 can exchange a frame.
    let samples = test_samples();
    let sent = AudioFrame::from_planar(CHANNELS, SAMPLE_RATE, samples.clone());
    let wire = encode_frame(&sent).expect("encode");
    c2_sink
        .send(Message::Binary(wire))
        .await
        .expect("client2 send binary");

    let echoed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;

            let mut ctx = ProcessContext::new(NUM_FRAMES, 0, SAMPLE_RATE);
            let _ = graph.process_cycle(&mut ctx);
            graph.flush_sinks();

            if let Ok(Some(Ok(Message::Binary(b)))) =
                tokio::time::timeout(Duration::from_millis(50), c2_src.next()).await
            {
                if let Ok(decoded) = decode_frame(&b, SPEC) {
                    if decoded.samples == samples {
                        return b;
                    }
                }
            }
        }
    })
    .await
    .expect("client2 round-trip timeout");

    let decoded = decode_frame(&echoed, SPEC).expect("decode client2 frame");
    assert_eq!(
        decoded.samples, samples,
        "second client must exchange frames after first client disconnected"
    );

    drop(c2_sink);
    drop(c2_src);
    serve_task.abort();
}
