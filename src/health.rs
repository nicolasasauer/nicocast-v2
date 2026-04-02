//! HTTP health endpoint for runtime status monitoring.
//!
//! Exposes `GET /health` on a configurable TCP port (default 8080).
//! Returns a JSON object with the current state of each subsystem.
//!
//! # Usage
//!
//! Set `health_port = 0` in `config.toml` to disable the endpoint.
//!
//! ```text
//! GET /health  →  200 OK
//!
//! {"p2p":1,"p2p_label":"discovering","rtsp":0,"rtsp_label":"idle","video":0,"video_label":"idle"}
//! ```
//!
//! State codes:
//!
//! | Code | Label | Meaning |
//! |------|-------|---------|
//! | 0 | `idle` | Subsystem inactive / not yet started |
//! | 1 | `discovering` | P2P discovery active |
//! | 2 | `connected` | RTSP session established |
//! | 3 | `playing` | Streaming / GStreamer pipeline running |

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

// ─── subsystem state constants ───────────────────────────────────────────────

/// Subsystem is inactive or not yet initialised.
pub const STATE_IDLE: u8 = 0;
/// P2P discovery is running (Wi-Fi Direct Find + Listen).
pub const STATE_P2P_DISCOVERING: u8 = 1;
/// RTSP session has been established (M6 SETUP received).
pub const STATE_RTSP_CONNECTED: u8 = 2;
/// Streaming is active (M7 PLAY received / GStreamer pipeline playing).
pub const STATE_PLAYING: u8 = 3;

// ─── shared application state ────────────────────────────────────────────────

/// Atomic state shared between the P2P, RTSP, and video tasks and the
/// health endpoint.  Each subsystem stores its current state as a `u8`
/// using the `STATE_*` constants defined above.
pub struct AppState {
    /// Current WiFi Direct / P2P state.
    pub p2p: AtomicU8,
    /// Current RTSP session state.
    pub rtsp: AtomicU8,
    /// Current GStreamer video pipeline state.
    pub video: AtomicU8,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            p2p: AtomicU8::new(STATE_IDLE),
            rtsp: AtomicU8::new(STATE_IDLE),
            video: AtomicU8::new(STATE_IDLE),
        }
    }
}

impl AppState {
    /// Serialise the current state to a compact JSON string.
    pub fn to_json(&self) -> String {
        let p2p = self.p2p.load(Ordering::Relaxed);
        let rtsp = self.rtsp.load(Ordering::Relaxed);
        let video = self.video.load(Ordering::Relaxed);
        format!(
            r#"{{"p2p":{p2p},"p2p_label":"{}","rtsp":{rtsp},"rtsp_label":"{}","video":{video},"video_label":"{}"}}"#,
            state_label(p2p),
            state_label(rtsp),
            state_label(video),
        )
    }
}

fn state_label(state: u8) -> &'static str {
    match state {
        STATE_IDLE => "idle",
        STATE_P2P_DISCOVERING => "discovering",
        STATE_RTSP_CONNECTED => "connected",
        STATE_PLAYING => "playing",
        _ => "unknown",
    }
}

// ─── HTTP server ──────────────────────────────────────────────────────────────

/// Run the HTTP health endpoint.
///
/// Listens on `0.0.0.0:{port}` and responds to `GET /health` with a JSON
/// body describing the current state of all subsystems.
///
/// When `port == 0` the endpoint is disabled and this function blocks
/// indefinitely (returning `Ok(())` when the process exits normally).
pub async fn serve(port: u16, state: Arc<AppState>) -> anyhow::Result<()> {
    if port == 0 {
        info!("Health endpoint disabled (health_port = 0)");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding health endpoint on {addr}: {e}"))?;
    info!("Health endpoint listening on {addr}");

    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let mut buf = [0u8; 512];
                    // Read the first chunk — we only need the first line.
                    let _ = stream.read(&mut buf).await;
                    let first_line = std::str::from_utf8(&buf)
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_owned();

                    if first_line.starts_with("GET /health") {
                        let body = state.to_json();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else {
                        let _ = stream
                            .write_all(
                                b"HTTP/1.1 404 Not Found\r\n\
                                  Content-Length: 0\r\n\
                                  Connection: close\r\n\
                                  \r\n",
                            )
                            .await;
                    }
                    debug!("Health request from {peer}: {first_line}");
                });
            }
            Err(e) => {
                warn!("Health endpoint: accept error: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_idle() {
        let s = AppState::default();
        assert_eq!(s.p2p.load(Ordering::Relaxed), STATE_IDLE);
        assert_eq!(s.rtsp.load(Ordering::Relaxed), STATE_IDLE);
        assert_eq!(s.video.load(Ordering::Relaxed), STATE_IDLE);
    }

    #[test]
    fn to_json_reflects_state() {
        let s = AppState::default();
        s.p2p.store(STATE_P2P_DISCOVERING, Ordering::Relaxed);
        s.rtsp.store(STATE_RTSP_CONNECTED, Ordering::Relaxed);
        s.video.store(STATE_PLAYING, Ordering::Relaxed);
        let json = s.to_json();
        assert!(json.contains(r#""p2p":1"#));
        assert!(json.contains(r#""p2p_label":"discovering""#));
        assert!(json.contains(r#""rtsp":2"#));
        assert!(json.contains(r#""rtsp_label":"connected""#));
        assert!(json.contains(r#""video":3"#));
        assert!(json.contains(r#""video_label":"playing""#));
    }
}
