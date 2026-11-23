//! `/api/ws`: one WebSocket per browser tab. JSON text frames both ways; see
//! [`crate::session::Command`] and [`crate::session::Event`]. Probe frames go
//! to the tab that asked, as binary messages (see [`crate::probe`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::AppState;
use crate::probe::{CLIENT_QUEUE, Outgoing};
use crate::session::{Command, Event};

/// Graphs are small; this is generous.
const MAX_MESSAGE: usize = 4 << 20;

/// 0 is reserved for edits made over REST.
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

pub async fn upgrade(ws: WebSocketUpgrade, State(s): State<Arc<AppState>>) -> Response {
    ws.max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| client(socket, s))
}

fn text(ev: &Event) -> Message {
    Message::Text(Utf8Bytes::from(
        serde_json::to_string(ev).unwrap_or_default(),
    ))
}

async fn client(socket: WebSocket, s: Arc<AppState>) {
    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let (out_tx, mut out_rx) = mpsc::channel(CLIENT_QUEUE);
    s.session.hub().add_client(id, out_tx);
    session_loop(id, socket, &s, &mut out_rx).await;
    s.session.hub().remove_client(id);
}

async fn session_loop(
    id: u64,
    socket: WebSocket,
    s: &AppState,
    probes: &mut mpsc::Receiver<Outgoing>,
) {
    let (mut tx, mut rx) = socket.split();
    let (mut events, hello) = s.session.subscribe(id);
    if tx.send(text(&hello)).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<Command>(&t) {
                    Ok(cmd) => s.session.handle(id, cmd),
                    Err(e) => tracing::warn!("client {id}: bad message: {e}"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            Some(out) = probes.recv() => {
                let msg = match out {
                    Outgoing::Text(t) => Message::Text(Utf8Bytes::from(t.as_ref())),
                    Outgoing::Binary(b) => Message::Binary(b.into()),
                };
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
            ev = events.recv() => {
                let msg = match ev {
                    Ok(json) => Message::Text(Utf8Bytes::from(json.as_ref())),
                    // Fell behind: start over from a fresh snapshot.
                    Err(RecvError::Lagged(_)) => text(&s.session.snapshot(id)),
                    Err(RecvError::Closed) => break,
                };
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
        }
    }
}
