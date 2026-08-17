//! SSE event streaming (`SPEC.md §15.2`): `GET /events/stream?after={seq}`.
//!
//! A connection first replays persisted events with `seq > after` (the
//! reconnect cursor), then switches to the live broadcast. The handoff is
//! race-free: the live subscription is opened BEFORE the replay snapshot, and
//! events whose seq was already replayed are filtered out on the live side —
//! a client never sees a gap or a duplicate.
//!
//! Every SSE event carries `event: <kind>` + `data: {envelope}`. Axum emits a
//! `: keepalive` comment every 15s of idle time. When the daemon's graceful
//! shutdown flips, the stream ends so the server can drain the connection.

use std::convert::Infallible;
use std::pin::Pin;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use super::{ApiError, ApiState};
use crate::event::SubscriberError;

/// Replay cap per connection: an `after=` cursor older than this must be
/// re-established with a fresh cursor (the CLI tails incrementally, so this
/// only binds very stale clients).
const REPLAY_LIMIT: usize = 10_000;
/// Keepalive cadence for idle connections.
const KEEPALIVE: Duration = Duration::from_secs(15);

#[derive(Deserialize, Default)]
pub struct SseParams {
    /// Replay persisted events with `seq > after` before switching to live.
    pub after: Option<i64>,
}

type SseStream = Pin<Box<dyn Stream<Item = Result<SseEvent, Infallible>> + Send>>;

/// `GET /events/stream?after={seq}`.
pub async fn stream_events(
    State(state): State<ApiState>,
    Query(params): Query<SseParams>,
) -> std::result::Result<impl axum::response::IntoResponse, ApiError> {
    let after = params.after.unwrap_or(0);

    // 1. Replay the durable tail (snapshot taken after subscribing so the
    //    live channel can be filtered against it without a gap).
    let mut sub = state.bus.subscribe();
    let replayed = state
        .bus
        .replay(after, REPLAY_LIMIT)
        .await
        .map_err(ApiError::from_kern)?;
    let last_replayed = replayed.last().map(|e| e.seq).unwrap_or(after);

    // 2. Forward the live stream into a bounded channel, dropping events
    //    already covered by the replay and stopping on shutdown/lag/close.
    let (tx, rx) = tokio::sync::mpsc::channel::<crate::store::Event>(1024);
    let mut shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        let mut last = last_replayed;
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                recv = sub.recv() => match recv {
                    Ok(event) if event.seq > last => {
                        last = event.seq;
                        if tx.send(event).await.is_err() {
                            break; // client disconnected
                        }
                    }
                    Ok(_) => {} // already replayed (handoff filter)
                    Err(SubscriberError::Lagged { .. }) => {
                        // The client missed live events and must reconnect
                        // with its last seen seq to replay them.
                        tracing::warn!(
                            "SSE subscriber lagged; closing stream — client must reconnect with after=<last seq>"
                        );
                        break;
                    }
                    Err(SubscriberError::Closed) => break,
                },
            }
        }
    });

    let replay_stream = tokio_stream::iter(replayed.into_iter().map(to_sse));
    let live_stream = ReceiverStream::new(rx).map(to_sse);
    let stream = replay_stream.chain(live_stream);

    Ok(Sse::new(Box::pin(stream) as SseStream).keep_alive(KeepAlive::new().interval(KEEPALIVE)))
}

/// Envelope → SSE event: `event: <kind>` + `data: {envelope JSON}`.
fn to_sse(event: crate::store::Event) -> Result<SseEvent, Infallible> {
    let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
    Ok(SseEvent::default().event(event.kind).data(data))
}
