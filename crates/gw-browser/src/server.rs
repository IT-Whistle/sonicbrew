//! Async WebSocket accept loop and per-connection handler.
//!
//! This module is the transport layer between a browser client and the two
//! `rtrb` rings wired into the audio graph by [`crate::BrowserGateway::register`]:
//!
//! * **Inbound** (`Producer`): a received binary message is parsed by
//!   [`crate::codec`] and pushed here; the graph-side [`RingSource`] drains it.
//! * **Outbound** (`Consumer`): a [`RingSink`] stashes graph output which the
//!   audio engine flushes into the matching `Producer`; this module pops the
//!   `Consumer` and sends each frame back to the client.
//!
//! Connections are served **sequentially** (one client at a time). `rtrb` 0.3's
//! `Producer`/`Consumer` are not `Clone`, so a single pair of handles is shared
//! across connections in turn rather than duplicated per client — appropriate
//! for the single-listener MVP. Multi-client fan-out is a P1 concern.
//!
//! [`RingSource`]: audio_graph_bsd::RingSource
//! [`RingSink`]: audio_graph_bsd::RingSink

use std::time::Duration;

use audio_core_bsd::AudioFrame;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time;
use tokio_tungstenite::tungstenite::Message;

use crate::codec::{decode_frame, encode_frame, FrameSpec};
use crate::Result;

/// Interval at which the outbound ring is polled for frames to send back to the
/// client. ~1 ms keeps end-to-end latency near one graph block at 48 kHz / 256
/// frames without busy-spinning the async runtime.
const OUTBOUND_PUMP_INTERVAL: Duration = Duration::from_millis(1);

/// Runs the accept loop against caller-supplied push/pop callbacks instead of
/// owned `rtrb` handles. This is the **bridge-ready** core: a caller holding an
/// `Arc<GatewayBridge>` (or any shared transport) passes
/// `|f| bridge.push_inbound(f)` / `|| bridge.pop_outbound()` so the WS worker
/// transparently follows the bridge across graph rebuilds (live-reload).
///
/// `push`/`pop` are taken by mutable reference so a single accept loop can
/// drive them across every sequential connection without being `'static` here;
/// the `'static` bound (when needed for spawning) is imposed by the public
/// [`serve_with_io`](crate::BrowserGateway::serve_with_io) entry points.
///
/// Exposed `pub(crate)` so a caller (or test) can inject a pre-bound listener,
/// avoiding the bind/rebind port race.
pub(crate) async fn run_accept_loop_io<P, Q>(
    listener: TcpListener,
    spec: FrameSpec,
    push: &mut P,
    pop: &mut Q,
) -> Result<()>
where
    P: FnMut(AudioFrame) -> std::result::Result<(), rtrb::PushError<AudioFrame>> + Send,
    Q: FnMut() -> std::result::Result<AudioFrame, rtrb::PopError> + Send,
{
    loop {
        let (stream, peer) = listener.accept().await?;
        tracing::info!(%peer, "browser gateway: client connected");
        if let Err(err) = handle_connection_io(stream, spec, push, pop).await {
            tracing::warn!(%peer, %err, "browser gateway: connection ended");
        }
    }
}

/// Drives a single WebSocket connection until the client closes or the sink
/// breaks, routing frames through the supplied `push`/`pop` callbacks.
async fn handle_connection_io<P, Q>(
    stream: TcpStream,
    spec: FrameSpec,
    push: &mut P,
    pop: &mut Q,
) -> Result<()>
where
    P: FnMut(AudioFrame) -> std::result::Result<(), rtrb::PushError<AudioFrame>> + Send,
    Q: FnMut() -> std::result::Result<AudioFrame, rtrb::PopError> + Send,
{
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut ws_sink, mut ws_source) = ws.split();

    let mut pump = time::interval(OUTBOUND_PUMP_INTERVAL);
    // Discard the immediate first tick so the first select iteration is fair
    // between inbound messages and the outbound pump.
    pump.tick().await;

    loop {
        tokio::select! {
            // Inbound: client -> graph (via the push callback).
            msg = ws_source.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => match decode_frame(&bytes, spec) {
                    Ok(frame) => {
                        if push(frame).is_err() {
                            tracing::warn!(
                                "browser gateway: inbound ring full, dropping client frame"
                            );
                        }
                    }
                    Err(err) => tracing::warn!(%err, "browser gateway: dropping malformed frame"),
                },
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Ping(_))) => {
                    // tokio-tungstenite auto-answers pings at the frame level;
                    // no explicit action needed here.
                }
                Some(Ok(_other)) => { /* ignore Text / Pong / Binary-as-text */ }
                Some(Err(err)) => return Err(err.into()),
            },

            // Outbound: graph -> client (via the pop callback).
            _ = pump.tick() => {
                while let Ok(frame) = pop() {
                    match encode_frame(&frame) {
                        Ok(bytes) => {
                            if ws_sink.send(Message::Binary(bytes)).await.is_err() {
                                // Client vanished mid-send; end the connection.
                                return Ok(());
                            }
                        }
                        Err(err) => tracing::warn!(
                            %err,
                            "browser gateway: dropping unencodable outbound frame"
                        ),
                    }
                }
            }
        }
    }

    let _ = ws_sink.close().await;
    Ok(())
}
