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
use tracing::{error, info};

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
