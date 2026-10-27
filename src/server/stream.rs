use super::*;
use crate::jobs::JobStatus;
use futures_core::Stream;
use std::{
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

pub(super) const NDJSON: &str = "application/x-ndjson";

pub(super) fn accepts_ndjson(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|media| {
                media
                    .split(';')
                    .next()
                    .is_some_and(|media| media.trim().eq_ignore_ascii_case(NDJSON))
            })
        })
}

#[derive(Default)]
pub(super) struct Utf8Chunks {
    pending: Vec<u8>,
}

impl Utf8Chunks {
    pub(super) fn push(&mut self, bytes: &[u8], final_chunk: bool) -> Option<String> {
        self.pending.extend_from_slice(bytes);
        let split = if final_chunk {
            self.pending.len()
        } else {
            complete_prefix_len(&self.pending)
        };
        if split == 0 {
            return None;
        }
        let suffix = self.pending.split_off(split);
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending = suffix;
        (!text.is_empty()).then_some(text)
    }
}

fn complete_prefix_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => {
            // A real invalid sequence is lossy-converted now. Only a trailing
            // incomplete sequence is retained for the next filesystem read.
            let mut end = bytes.len();
            for keep in 1..=3.min(bytes.len()) {
                let start = bytes.len() - keep;
                if matches!(std::str::from_utf8(&bytes[start..]), Err(e) if e.error_len().is_none())
                {
                    end = start;
                }
            }
            end
        }
    }
}

fn ping_due(idle: Duration, interval: Duration) -> bool {
    idle >= interval
}

pub(super) fn out_event(data: &str) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({"t":"out", "d":data})).unwrap();
    bytes.push(b'\n');
    bytes
}

pub(super) fn ping_event() -> Vec<u8> {
    b"{\"t\":\"ping\"}\n".to_vec()
}

pub(super) fn exit_event(code: i32) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({"t":"exit", "code":code})).unwrap();
    bytes.push(b'\n');
    bytes
}

pub(super) fn err_event(message: &str) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({"t":"err", "msg":message})).unwrap();
    bytes.push(b'\n');
    bytes
}

pub(super) struct ReceiverStream {
    receiver: tokio::sync::mpsc::Receiver<Result<axum::body::Bytes, Infallible>>,
}

impl Stream for ReceiverStream {
    type Item = Result<axum::body::Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

pub(super) fn follow_response(state: AppState, id: String, offset: usize) -> Response {
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    tokio::spawn(follow(state, id.clone(), offset, sender));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, NDJSON)
        .header("X-Job-Id", &id)
        .body(Body::from_stream(ReceiverStream { receiver }))
        .unwrap()
}

async fn follow(
    state: AppState,
    id: String,
    mut offset: usize,
    sender: tokio::sync::mpsc::Sender<Result<axum::body::Bytes, Infallible>>,
) {
    let mut utf8 = Utf8Chunks::default();
    let mut last_ping = Instant::now();
    loop {
        match state.jobs.read_log(&id, offset) {
            Ok(Some(bytes)) => {
                offset += bytes.len();
                if let Some(text) = utf8.push(&bytes, false) {
                    if sender.send(Ok(out_event(&text).into())).await.is_err() {
                        return;
                    }
                }
            }
            Ok(None) => {
                let _ = sender.send(Ok(err_event("job not found").into())).await;
                return;
            }
            Err(error) => {
                let _ = sender.send(Ok(err_event(&error.to_string()).into())).await;
                return;
            }
        }
        if let Some(meta) = state.jobs.get(&id) {
            if !matches!(meta.state, JobStatus::Running | JobStatus::Orphaned) {
                if let Some(text) = utf8.push(&[], true) {
                    if sender.send(Ok(out_event(&text).into())).await.is_err() {
                        return;
                    }
                }
                let code = meta.exit_code.unwrap_or(-1);
                let _ = sender.send(Ok(exit_event(code).into())).await;
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        if ping_due(last_ping.elapsed(), state.stream_ping_interval) {
            if sender.send(Ok(ping_event().into())).await.is_err() {
                return;
            }
            last_ping = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ac_t6_3_2_incomplete_utf8_is_carried_without_replacement() {
        let mut chunks = Utf8Chunks::default();
        let bytes = "あ".as_bytes();
        assert_eq!(chunks.push(&bytes[..1], false), None);
        assert_eq!(chunks.push(&bytes[1..2], false), None);
        assert_eq!(chunks.push(&bytes[2..], false).as_deref(), Some("あ"));
    }

    #[test]
    fn ping_decision_is_a_pure_boundary_check() {
        assert!(!ping_due(
            Duration::from_millis(29),
            Duration::from_millis(30)
        ));
        assert!(ping_due(
            Duration::from_millis(30),
            Duration::from_millis(30)
        ));
    }
}
