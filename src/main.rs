//! nicocast-v2 — Miracast Sink for Raspberry Pi Zero 2W
//!
//! Supports Samsung Smart View via WiFi Direct (P2P), hardware-accelerated
//! H.264 decoding with v4l2h264dec, and persistent logging over USB Gadget.

mod config;
mod logger;
mod p2p;
mod rtsp;
mod video;

use anyhow::Result;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Load configuration.
    //
    // Search order (first file that exists and parses wins):
    //   1. /etc/nicocast/config.toml  — system-wide install (Dockerfile default)
    //   2. ./config.toml              — development / side-by-side fallback
    //
    // If neither is found the built-in defaults are used and a warning is printed.
    const CONFIG_PATHS: &[&str] = &["/etc/nicocast/config.toml", "config.toml"];
    let cfg = CONFIG_PATHS
        .iter()
        .find_map(|path| config::Config::load(path).ok())
        .unwrap_or_else(|| {
            eprintln!(
                "Warning: no config file found at {:?}, using built-in defaults",
                CONFIG_PATHS
            );
            config::Config::default()
        });

    // Initialise file-based + console logging.
    // The guard MUST be kept alive for the duration of the process so that
    // the background log-writer thread keeps flushing to disk.
    let _log_guard = logger::init(&cfg.log_file)?;

    info!("nicocast-v2 starting (device: {})", cfg.device_name);
    info!(
        "WiFi interface: {}, USB interface: {}",
        cfg.wifi_interface, cfg.usb_interface
    );

    // Ensure XDG_RUNTIME_DIR is set to a valid, writable directory.
    //
    // GStreamer (and Wayland/PipeWire/PulseAudio) rely on this variable for
    // socket and temporary-file placement.  On headless or container systems
    // the variable is often absent, which causes GStreamer to log
    // "XDG_RUNTIME_DIR is invalid" and may result in a crash.
    ensure_xdg_runtime_dir();

    // Initialise GStreamer once for the whole process
    gstreamer::init()?;

    // Spawn the GStreamer video pipeline (waits for a UDP stream on cfg.rtp_port)
    let video_handle = {
        let cfg_clone = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = video::run_pipeline(&cfg_clone).await {
                error!("Video pipeline error: {e:#}");
            }
        })
    };

    // Spawn the RTSP control-plane server (Miracast M1–M16 exchange)
    let rtsp_handle = {
        let cfg_clone = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = rtsp::serve(&cfg_clone).await {
                error!("RTSP server error: {e:#}");
            }
        })
    };

    // Bring up the P2P manager (wpa_supplicant D-Bus, WFD IEs, Samsung Smart View)
    let p2p_handle = {
        let cfg_clone = cfg.clone();
        tokio::spawn(async move {
            match p2p::P2pManager::new(&cfg_clone).await {
                Ok(mut mgr) => {
                    if let Err(e) = mgr.run().await {
                        error!("P2P manager error: {e:#}");
                    }
                }
                Err(e) => error!("P2P manager init error: {e:#}"),
            }
        })
    };

    // Wait for the first task to finish; the exit reason is logged so the
    // service logs show exactly what triggered the shutdown.
    let exit_reason = tokio::select! {
        res = video_handle => match res {
            Ok(()) => "video pipeline task exited normally",
            Err(e) if e.is_panic() => {
                error!("Video pipeline task panicked: {e}");
                "video pipeline task panicked"
            }
            Err(_) => "video pipeline task was cancelled",
        },
        res = rtsp_handle => match res {
            Ok(()) => "RTSP server task exited normally",
            Err(e) if e.is_panic() => {
                error!("RTSP server task panicked: {e}");
                "RTSP server task panicked"
            }
            Err(_) => "RTSP server task was cancelled",
        },
        res = p2p_handle => match res {
            Ok(()) => "P2P manager task exited normally",
            Err(e) if e.is_panic() => {
                error!("P2P manager task panicked: {e}");
                "P2P manager task panicked"
            }
            Err(_) => "P2P manager task was cancelled",
        },
        _ = tokio::signal::ctrl_c() => "received Ctrl-C signal",
    };
    info!("Shutdown triggered: {exit_reason}");

    info!("nicocast-v2 stopped");
    Ok(())
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Guarantee that `XDG_RUNTIME_DIR` is set to a valid, writable directory.
///
/// On headless or container systems the variable may be absent or point at a
/// non-existent path.  GStreamer (and Wayland/PipeWire/PulseAudio) use it for
/// socket and temporary-file placement; without it they log
/// *"XDG_RUNTIME_DIR is invalid"* and may crash or behave unexpectedly.
///
/// When the variable is missing or its target does not exist this function
/// falls back to `/tmp/nicocast-runtime`, creates the directory if necessary,
/// and re-exports `XDG_RUNTIME_DIR` so that child processes inherit the value.
// `std::env::set_var` was deprecated in Rust 1.81 due to potential unsoundness
// in multi-threaded programs, but this function runs at startup before any
// parallel work begins, making this usage safe.
#[allow(deprecated)]
fn ensure_xdg_runtime_dir() {
    const FALLBACK: &str = "/tmp/nicocast-runtime";

    let needs_fallback = match std::env::var("XDG_RUNTIME_DIR") {
        Err(_) => {
            // Variable not set at all.
            true
        }
        Ok(val) if val.is_empty() => {
            // Variable set to an empty string — treat as absent.
            true
        }
        Ok(val) => {
            // Variable is set; verify the path actually exists *and* is a
            // directory (not a plain file or symlink to something unexpected).
            let p = std::path::Path::new(&val);
            if !p.is_dir() {
                warn!(
                    "XDG_RUNTIME_DIR='{}' does not exist or is not a directory; \
                     falling back to '{FALLBACK}'",
                    val
                );
                true
            } else {
                false
            }
        }
    };

    if needs_fallback {
        if let Err(e) = create_runtime_dir(FALLBACK) {
            // Non-fatal: log and continue.  GStreamer will log its own warning
            // but the pipeline may still work (e.g. if no Wayland sink is used).
            warn!("Could not create fallback XDG_RUNTIME_DIR '{FALLBACK}': {e}");
        } else {
            std::env::set_var("XDG_RUNTIME_DIR", FALLBACK);
            warn!(
                "XDG_RUNTIME_DIR was not set or invalid; \
                 using fallback '{FALLBACK}'"
            );
        }
    }
}

/// Create `path` as a directory accessible only by the current user (mode 0700).
///
/// Uses platform-specific APIs on Unix to apply restrictive permissions at
/// creation time, reducing the window for privilege-escalation attacks in `/tmp`.
fn create_runtime_dir(path: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}
