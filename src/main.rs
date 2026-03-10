mod audio;
mod config;
mod filter;
mod inject;
mod postprocess;
mod ws;

use anyhow::Result;
use tokio::sync::watch;
use tokio_tungstenite::connect_async;

/// Holds the resources for an active dictation session.
struct ActiveSession {
    /// Shutdown signal sender — dropping or sending true stops the session.
    shutdown_tx: watch::Sender<bool>,
    /// WebSocket session task handle.
    ws_handle: tokio::task::JoinHandle<Result<()>>,
    /// cpal stream — kept alive here (not Send, lives on main task).
    _stream: cpal::Stream,
}

impl ActiveSession {
    /// Signal shutdown and wait for the WS task to finish.
    async fn deactivate(self) {
        let _ = self.shutdown_tx.send(true);
        // Drop the stream to stop audio capture
        drop(self._stream);
        match self.ws_handle.await {
            Ok(Ok(())) => tracing::info!("session ended cleanly"),
            Ok(Err(e)) => tracing::warn!("session ended with error: {e:#}"),
            Err(e) => tracing::error!("session task panicked: {e}"),
        }
    }
}

/// Start a new dictation session: capture audio, connect WS, stream.
fn activate(
    config: &config::Config,
    injector: &'static dyn inject::TextInjector,
    preconnected: Option<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>,
) -> Result<ActiveSession> {
    let (stream, consumer) = audio::start_capture(&config.audio.device)?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let url = config.backend.url.clone();
    let pp_config = config.postprocessing.clone();

    let ws_handle = tokio::spawn(async move {
        ws::run_session(url, consumer, injector, &pp_config, shutdown_rx, preconnected).await
    });

    tracing::info!("session activated");

    Ok(ActiveSession {
        shutdown_tx,
        ws_handle,
        _stream: stream,
    })
}

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Spawn a background pre-connect task with exponential backoff.
fn spawn_preconnect(url: String) -> tokio::task::JoinHandle<Option<WsStream>> {
    tokio::spawn(async move {
        let mut delay = std::time::Duration::from_secs(2);
        let max_delay = std::time::Duration::from_secs(30);
        loop {
            match connect_async(&url).await {
                Ok((ws, _)) => {
                    tracing::info!("pre-connected to {url}");
                    return Some(ws);
                }
                Err(e) => {
                    tracing::info!("pre-connect to {url} failed: {e}, retrying in {delay:?}");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(max_delay);
                }
            }
        }
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("asr_rs=info".parse().unwrap()),
        )
        .init();

    let config = config::load_config()?;

    // Create injector with static lifetime (lives for the process)
    let injector: &'static dyn inject::TextInjector =
        Box::leak(inject::create_injector(&config.injection));

    let drivers_desc = config.injection.driver_order.join(",");
    tracing::info!(
        "asr-rs starting (backend={}, drivers=[{}], niri_detect={})",
        config.backend.url,
        drivers_desc,
        config.injection.niri_detect,
    );

    // SIGUSR1 for toggle
    let mut sigusr1 =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;

    // SIGUSR2 for explicit deactivate only
    let mut sigusr2 =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2())?;

    // SIGTERM / SIGINT for shutdown
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let mut session: Option<ActiveSession> = None;

    // Pre-connect WebSocket on startup
    let mut preconnect_handle = Some(spawn_preconnect(config.backend.url.clone()));

    tracing::info!("ready — send SIGUSR1 to toggle, SIGUSR2 to stop (pkill -USR1 asr-rs)");

    loop {
        tokio::select! {
            _ = sigusr1.recv() => {
                if let Some(s) = session.take() {
                    tracing::info!("deactivating...");
                    s.deactivate().await;
                    // Pre-connect for next session
                    preconnect_handle = Some(spawn_preconnect(config.backend.url.clone()));
                } else {
                    tracing::info!("activating...");
                    // Take the pre-connected WS if available
                    let pre_ws = if let Some(handle) = preconnect_handle.take() {
                        // Try to get it if ready, otherwise don't block
                        match tokio::time::timeout(std::time::Duration::from_millis(50), handle).await {
                            Ok(Ok(ws)) => ws,
                            _ => None,
                        }
                    } else {
                        None
                    };
                    match activate(&config, injector, pre_ws) {
                        Ok(s) => session = Some(s),
                        Err(e) => {
                            tracing::error!("activation failed: {e:#}");
                            preconnect_handle = Some(spawn_preconnect(config.backend.url.clone()));
                        }
                    }
                }
            }
            _ = sigusr2.recv() => {
                if let Some(s) = session.take() {
                    tracing::info!("SIGUSR2: deactivating...");
                    s.deactivate().await;
                    // Pre-connect for next session
                    preconnect_handle = Some(spawn_preconnect(config.backend.url.clone()));
                }
            }
            _ = sigterm.recv() => {
                tracing::info!("received shutdown signal");
                if let Some(s) = session.take() {
                    s.deactivate().await;
                }
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received ctrl-c");
                if let Some(s) = session.take() {
                    s.deactivate().await;
                }
                break;
            }
        }
    }

    tracing::info!("goodbye");
    Ok(())
}
