use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use ringbuf::HeapCons;
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::audio::drain_s16le;
use crate::config::PostprocessingConfig;
use crate::inject::{TextInjector, sanitize_for_injection};
use crate::postprocess::process_text;

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// WhisperLiveKit FrontData JSON structure.
/// The server sends cumulative text in `lines[]` array.
#[derive(Debug, serde::Deserialize)]
struct FrontData {
    #[serde(default)]
    lines: Vec<Line>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct Line {
    #[serde(default)]
    text: String,
}

/// Tracks cumulative text from WhisperLiveKit and extracts deltas.
pub struct SegmentTracker {
    last_line_count: usize,
    last_line_text_len: usize,
}

impl SegmentTracker {
    pub fn new() -> Self {
        Self {
            last_line_count: 0,
            last_line_text_len: 0,
        }
    }

    /// Process a snapshot of lines from the server.
    /// Returns delta strings that should be injected.
    pub(crate) fn process_snapshot(&mut self, lines: &[Line]) -> Vec<String> {
        let mut deltas = Vec::new();

        if lines.is_empty() {
            return deltas;
        }

        let current_count = lines.len();

        if current_count < self.last_line_count {
            // Server rewound (reset) — resync without injecting
            tracing::debug!(
                "segment rewind: {} -> {} lines, resyncing",
                self.last_line_count,
                current_count
            );
            self.last_line_count = current_count;
            self.last_line_text_len = lines.last().map_or(0, |l| l.text.len());
            return deltas;
        }

        // Completed lines since last snapshot (all fully new lines)
        if current_count > self.last_line_count {
            let start = if self.last_line_count > 0 {
                // Delta from the last known line's text
                let prev_last = &lines[self.last_line_count - 1].text;
                if prev_last.len() > self.last_line_text_len {
                    let delta = prev_last[self.last_line_text_len..].trim_start();
                    if !delta.is_empty() {
                        deltas.push(delta.to_string());
                    }
                }
                self.last_line_count
            } else {
                0
            };

            // New complete lines
            for line in &lines[start..current_count - 1] {
                let text = line.text.trim();
                if !text.is_empty() {
                    deltas.push(text.to_string());
                }
            }
        }

        // Current (in-progress) last line — extract delta
        let last_line = &lines[current_count - 1].text;
        let prev_len = if current_count == self.last_line_count {
            self.last_line_text_len
        } else {
            0
        };

        if last_line.len() > prev_len {
            let delta = last_line[prev_len..].trim_start();
            if !delta.is_empty() {
                deltas.push(delta.to_string());
            }
        } else if last_line.len() < prev_len {
            // Rewind within a line — resync without injecting
            tracing::debug!(
                "text rewind: {} -> {} chars on last line, resyncing",
                prev_len,
                last_line.len()
            );
        }

        self.last_line_count = current_count;
        self.last_line_text_len = last_line.len();

        deltas
    }
}

/// Run a WebSocket session: send audio, receive text, inject deltas.
/// Accepts an optional pre-connected WebSocket stream. If None, connects fresh.
pub async fn run_session(
    url: String,
    mut consumer: HeapCons<f32>,
    injector: &'static dyn TextInjector,
    config: &PostprocessingConfig,
    mut shutdown_rx: watch::Receiver<bool>,
    preconnected: Option<WsStream>,
) -> Result<()> {
    let ws_stream = if let Some(ws) = preconnected {
        tracing::info!("using pre-connected WebSocket");
        ws
    } else {
        let (ws, _) = connect_async(&url)
            .await
            .context("failed to connect to ASR server")?;
        tracing::info!("connected to {url}");
        ws
    };

    let (mut sender, mut receiver) = ws_stream.split();

    let pp_config = PostprocessingConfig {
        hallucination_filter: config.hallucination_filter,
        spoken_punctuation: config.spoken_punctuation,
    };

    // Shutdown channel for the send task
    let mut shutdown_send = shutdown_rx.clone();

    // Send task: drain ring buffer every 20ms, send as binary s16le
    let send_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let bytes = drain_s16le(&mut consumer);
                    if !bytes.is_empty() {
                        if let Err(e) = sender.send(Message::Binary(bytes.into())).await {
                            tracing::error!("ws send error: {e}");
                            return Err(anyhow::anyhow!("ws send error: {e}"));
                        }
                    }
                }
                _ = shutdown_send.changed() => {
                    tracing::debug!("send task shutting down");
                    let _ = sender.close().await;
                    return Ok(());
                }
            }
        }
    });

    // Receive task: parse JSON, diff, postprocess, inject
    let recv_handle: tokio::task::JoinHandle<Result<()>> = tokio::spawn(async move {
        let mut tracker = SegmentTracker::new();
        let mut first_chunk = true;

        loop {
            tokio::select! {
                msg = receiver.next() => {
                    let Some(msg) = msg else {
                        tracing::info!("ws connection closed by server");
                        return Ok(());
                    };
                    match msg {
                        Ok(Message::Text(text)) => {
                            match serde_json::from_str::<FrontData>(&text) {
                                Ok(data) => {
                                    let deltas = tracker.process_snapshot(&data.lines);
                                    for delta in deltas {
                                        if let Some(processed) = process_text(&delta, &pp_config) {
                                            let to_inject = if first_chunk {
                                                first_chunk = false;
                                                sanitize_for_injection(&processed)
                                            } else {
                                                // Prepend space before non-punctuation
                                                let sanitized = sanitize_for_injection(&processed);
                                                if sanitized.starts_with(|c: char| {
                                                    matches!(c, '.' | ',' | '?' | '!' | ':' | ';' | ')' | ']' | '}')
                                                }) {
                                                    sanitized
                                                } else {
                                                    format!(" {sanitized}")
                                                }
                                            };
                                            if let Err(e) = injector.inject(&to_inject) {
                                                tracing::error!("injection error: {e}");
                                                return Err(e);
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!("ignoring non-FrontData message: {e}");
                                }
                            }
                        }
                        Ok(Message::Close(_)) => {
                            tracing::info!("ws close frame received");
                            return Ok(());
                        }
                        Err(e) => {
                            tracing::error!("ws recv error: {e}");
                            return Err(anyhow::anyhow!("ws recv error: {e}"));
                        }
                        _ => {}
                    }
                }
                _ = shutdown_rx.changed() => {
                    tracing::debug!("recv task shutting down");
                    return Ok(());
                }
            }
        }
    });

    // Wait for both tasks
    let (send_result, recv_result) = tokio::try_join!(send_handle, recv_handle)
        .context("ws task panicked")?;
    send_result?;
    recv_result?;

    Ok(())
}
