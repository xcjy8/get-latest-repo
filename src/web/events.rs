use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_stream::stream;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

const REPLAY_CAPACITY: usize = 2_048;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EventEnvelope {
    schema_version: u8,
    server_instance_id: String,
    sequence: String,
    occurred_at: String,
    #[serde(rename = "type")]
    event_type: String,
    payload: serde_json::Value,
}

#[derive(Clone)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

struct EventBusInner {
    server_instance_id: String,
    sequence: AtomicU64,
    sender: broadcast::Sender<Arc<EventEnvelope>>,
    replay: Mutex<VecDeque<Arc<EventEnvelope>>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(REPLAY_CAPACITY);
        Self {
            inner: Arc::new(EventBusInner {
                server_instance_id: Uuid::new_v4().to_string(),
                sequence: AtomicU64::new(0),
                sender,
                replay: Mutex::new(VecDeque::with_capacity(REPLAY_CAPACITY)),
            }),
        }
    }

    pub fn publish<T: Serialize>(&self, event_type: &str, payload: &T) {
        let sequence = self.inner.sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let envelope = Arc::new(EventEnvelope {
            schema_version: 1,
            server_instance_id: self.inner.server_instance_id.clone(),
            sequence: sequence.to_string(),
            occurred_at: chrono::Local::now().to_rfc3339(),
            event_type: event_type.to_string(),
            payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
        });
        if let Ok(mut replay) = self.inner.replay.lock() {
            if replay.len() == REPLAY_CAPACITY {
                replay.pop_front();
            }
            replay.push_back(Arc::clone(&envelope));
        }
        let _ = self.inner.sender.send(envelope);
    }

    pub fn current_sequence(&self) -> u64 {
        self.inner.sequence.load(Ordering::Acquire)
    }

    /// 构造仅发送给当前连接的瞬时事件，不污染全局重放窗口和其他客户端。
    fn client_event<T: Serialize>(&self, event_type: &str, payload: &T) -> Arc<EventEnvelope> {
        Arc::new(EventEnvelope {
            schema_version: 1,
            server_instance_id: self.inner.server_instance_id.clone(),
            sequence: self.current_sequence().to_string(),
            occurred_at: chrono::Local::now().to_rfc3339(),
            event_type: event_type.to_string(),
            payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
        })
    }

    fn replay_after(&self, sequence: u64) -> Option<Vec<Arc<EventEnvelope>>> {
        let replay = self.inner.replay.lock().ok()?;
        if let Some(first) = replay.front() {
            let first_sequence = first.sequence.parse::<u64>().ok()?;
            if sequence > 0 && sequence.saturating_add(1) < first_sequence {
                return None;
            }
        }
        Some(
            replay
                .iter()
                .filter(|event| event.sequence.parse::<u64>().unwrap_or(0) > sequence)
                .cloned()
                .collect(),
        )
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct EventStreamQuery {
    after: Option<u64>,
}

pub async fn event_stream(
    State(state): State<super::WebState>,
    Query(query): Query<EventStreamQuery>,
    headers: HeaderMap,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let after = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .or(query.after)
        .unwrap_or(0);
    let replay = state.events.replay_after(after);
    let mut receiver = state.events.inner.sender.subscribe();
    let events = state.events.clone();

    let output = stream! {
        match replay {
            Some(replay) => {
                for envelope in replay {
                    yield Ok(to_sse_event(&envelope));
                }
            }
            None => {
                let sequence = events.current_sequence();
                let data = serde_json::json!({ "after": after, "current": sequence });
                yield Ok(to_sse_event(&events.client_event("resync.required", &data)));
                return;
            }
        }

        loop {
            let received = tokio::select! {
                event = receiver.recv() => event,
                _ = wait_for_shutdown() => break,
            };
            match received {
                Ok(envelope) => yield Ok(to_sse_event(&envelope)),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let payload = serde_json::json!({ "reason": "client_lagged" });
                    yield Ok(to_sse_event(&events.client_event("resync.required", &payload)));
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(output).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("heartbeat"),
    )
}

/// 主动结束长连接，避免 Axum 优雅关闭被 SSE 客户端一直占住。
async fn wait_for_shutdown() {
    while !crate::signal_handler::is_shutdown_requested() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn to_sse_event(envelope: &EventEnvelope) -> Event {
    Event::default()
        .id(envelope.sequence.clone())
        .json_data(envelope)
        .unwrap_or_else(|_| Event::default().data("{}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_is_ordered_and_bounded_by_sequence() {
        let events = EventBus::new();
        events.publish("heartbeat", &serde_json::json!({}));
        events.publish("heartbeat", &serde_json::json!({}));

        let replay = events.replay_after(1).unwrap();

        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].sequence, "2");
    }

    #[test]
    fn client_only_event_does_not_pollute_global_replay() {
        let events = EventBus::new();
        events.publish("heartbeat", &serde_json::json!({}));

        let event = events.client_event(
            "resync.required",
            &serde_json::json!({ "reason": "client_lagged" }),
        );

        assert_eq!(event.sequence, "1");
        assert_eq!(events.current_sequence(), 1);
        assert_eq!(events.replay_after(0).unwrap().len(), 1);
    }
}
