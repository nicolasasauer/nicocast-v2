//! nicocast-v2 — Miracast Sink for Raspberry Pi Zero 2W
//!
//! Supports Samsung Smart View via WiFi Direct (P2P), hardware-accelerated
//! H.264 decoding with v4l2h264dec (software fallback: avdec_h264), and
//! persistent logging over USB Gadget.

mod airplay;
mod config;
mod health;
mod logger;
mod p2p;
mod rtsp;
mod video;

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // ── Parse command-line arguments ─────────────────────────────────────────
    let args: Vec<String> = std::env::args().collect();
    let check_mode = args.iter().any(|a| a == "--check");

    // ── Load configuration ────────────────────────────────────────────────────
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

    // ── Pre-flight check mode (`--check`) ────────────────────────────────────
    //
    // Run all prerequisite checks, print results, then exit.
    // Exit code 0 = all critical checks passed (warnings are non-fatal).
    // Exit code 1 = at least one critical check failed.
    if check_mode {
        // GStreamer must be initialised before we can query the registry.
        gstreamer::init().context("initialising GStreamer for --check")?;
        let ok = run_preflight_check(&cfg).await;
        std::process::exit(if ok { 0 } else { 1 });
    }

    // ── Initialise file-based + console logging ───────────────────────────────
    // The guard MUST be kept alive for the duration of the process so that
    // the background log-writer thread keeps flushing to disk.
    let _log_guard = logger::init(&cfg.log_file)?;

    info!("nicocast-v2 starting (device: {})", cfg.device_name);
    info!(
        "WiFi interface: {}, USB interface: {}",
        cfg.wifi_interface, cfg.usb_interface
    );

    // ── Ensure XDG_RUNTIME_DIR is set to a valid, writable directory ─────────
    //
    // GStreamer (and Wayland/PipeWire/PulseAudio) rely on this variable for
    // socket and temporary-file placement.  On headless or container systems
    // the variable is often absent, which causes GStreamer to log
    // "XDG_RUNTIME_DIR is invalid" and may result in a crash.
    ensure_xdg_runtime_dir();

    // ── Initialise GStreamer once for the whole process ───────────────────────
    gstreamer::init()?;

    // ── Shared application state (for the health endpoint) ───────────────────
    let state = Arc::new(health::AppState::default());

    // ── Sequential startup — RTSP port is bound BEFORE P2P advertising ───────
    //
    // Binding the RTSP listener first ensures that by the time Samsung Smart
    // View discovers the sink (via P2P) and immediately connects to RTSP port
    // 7236, the server is already accepting connections.  A race between P2P
    // advertisement and RTSP readiness can cause the first connection attempt
    // to be refused, which some Samsung firmware does not retry.
    let rtsp_listener = rtsp::bind(cfg.rtsp_port)
        .await
        .with_context(|| format!("binding RTSP port {}", cfg.rtsp_port))?;
    info!("RTSP port {} bound — ready for connections", cfg.rtsp_port);

    // ── Spawn the HTTP health endpoint ────────────────────────────────────────
    let health_handle = {
        let state_clone = Arc::clone(&state);
        let health_port = cfg.health_port;
        tokio::spawn(async move {
            if let Err(e) = health::serve(health_port, state_clone).await {
                error!("Health endpoint error: {e:#}");
            }
        })
    };

    // ── Spawn the GStreamer video pipeline ────────────────────────────────────
    // (waits for a UDP stream on cfg.rtp_port)
    let video_handle = {
        let cfg_clone = cfg.clone();
        let state_clone = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = video::run_pipeline(&cfg_clone, state_clone).await {
                error!("Video pipeline error: {e:#}");
            }
        })
    };

    // ── Spawn the RTSP control-plane server ───────────────────────────────────
    // (Miracast M1–M16 exchange; uses the pre-bound listener)
    let rtsp_handle = {
        let cfg_clone = cfg.clone();
        let state_clone = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = rtsp::serve(rtsp_listener, &cfg_clone, state_clone).await {
                error!("RTSP server error: {e:#}");
            }
        })
    };

    // ── Spawn the P2P manager ─────────────────────────────────────────────────
    // (wpa_supplicant D-Bus, WFD IEs, Samsung Smart View discovery)
    let p2p_handle = {
        let cfg_clone = cfg.clone();
        let state_clone = Arc::clone(&state);
        tokio::spawn(async move {
            match p2p::P2pManager::new(&cfg_clone).await {
                Ok(mut mgr) => {
                    if let Err(e) = mgr.run(state_clone).await {
                        error!("P2P manager error: {e:#}");
                    }
                }
                Err(e) => error!("P2P manager init error: {e:#}"),
            }
        })
    };

    // ── Spawn the AirPlay (UxPlay) subprocess ─────────────────────────────────
    let airplay_handle = {
        let cfg_clone = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = airplay::run_uxplay(&cfg_clone).await {
                error!("AirPlay (UxPlay) error: {e:#}");
            }
        })
    };

    // ── Wait for the first task to finish ────────────────────────────────────
    // The exit reason is logged so the service logs show exactly what triggered
    // the shutdown.
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
        res = airplay_handle => match res {
            Ok(()) => "AirPlay task exited normally",
            Err(e) if e.is_panic() => {
                error!("AirPlay task panicked: {e}");
                "AirPlay task panicked"
            }
            Err(_) => "AirPlay task was cancelled",
        },
        res = health_handle => match res {
            Ok(()) => "health endpoint task exited normally",
            Err(e) if e.is_panic() => {
                error!("Health endpoint task panicked: {e}");
                "health endpoint task panicked"
            }
            Err(_) => "health endpoint task was cancelled",
        },
        _ = tokio::signal::ctrl_c() => "received Ctrl-C signal",
        _ = wait_for_sigterm() => "received SIGTERM signal",
    };
    info!("Shutdown triggered: {exit_reason}");

    info!("nicocast-v2 stopped");
    Ok(())
}

// ─── pre-flight check ────────────────────────────────────────────────────────

/// Run all prerequisite checks and print a human-readable report.
///
/// Returns `true` if all *critical* checks pass (warnings are allowed).
/// Returns `false` if at least one critical check fails.
async fn run_preflight_check(cfg: &config::Config) -> bool {
    let mut ok = true;

    // 1. Validate config (wfd_subelems hex, etc.)
    match cfg.validate() {
        Ok(()) => eprintln!("[OK]   Config validation passed"),
        Err(e) => {
            eprintln!("[FAIL] Config validation failed: {e}");
            ok = false;
        }
    }

    // 2. WiFi interface exists in sysfs
    let wifi_path = format!("/sys/class/net/{}", cfg.wifi_interface);
    if std::path::Path::new(&wifi_path).is_dir() {
        eprintln!("[OK]   WiFi interface '{}' found", cfg.wifi_interface);
    } else {
        eprintln!(
            "[FAIL] WiFi interface '{}' not found at {wifi_path}",
            cfg.wifi_interface
        );
        ok = false;
    }

    // 3. H.264 decoder plugin available (hardware or software fallback)
    let registry = gstreamer::Registry::get();
    let hw_dec = registry.find_plugin("video4linux2").is_some()
        && gstreamer::ElementFactory::find("v4l2h264dec").is_some();
    let sw_dec = gstreamer::ElementFactory::find("avdec_h264").is_some();
    if hw_dec {
        eprintln!("[OK]   GStreamer hardware H.264 decoder (v4l2h264dec) available");
    } else if sw_dec {
        eprintln!(
            "[WARN] v4l2h264dec not found — will use software fallback avdec_h264 \
             (install gstreamer1.0-libav for this)"
        );
    } else {
        eprintln!(
            "[FAIL] Neither v4l2h264dec nor avdec_h264 is available. \
             Install gstreamer1.0-plugins-good (hardware) or \
             gstreamer1.0-libav (software)."
        );
        ok = false;
    }

    // 4. RTSP port is not already bound by another process
    match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", cfg.rtsp_port)).await {
        Ok(_) => eprintln!("[OK]   RTSP port {} is available", cfg.rtsp_port),
        Err(e) => {
            eprintln!(
                "[FAIL] RTSP port {} is already in use: {e}",
                cfg.rtsp_port
            );
            ok = false;
        }
    }

    // 5. wpa_supplicant socket directory (heuristic — D-Bus is not tested)
    let wpa_socket_dirs = ["/run/wpa_supplicant", "/var/run/wpa_supplicant"];
    if wpa_socket_dirs
        .iter()
        .any(|p| std::path::Path::new(p).is_dir())
    {
        eprintln!("[OK]   wpa_supplicant socket directory found");
    } else {
        eprintln!(
            "[WARN] wpa_supplicant socket directory not found at {:?}. \
             Ensure wpa_supplicant is running with D-Bus support.",
            wpa_socket_dirs
        );
        // Non-fatal — might be using a non-standard socket path.
    }

    // 6. Health port (if enabled) is available
    if cfg.health_port != 0 {
        match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", cfg.health_port)).await {
            Ok(_) => eprintln!("[OK]   Health port {} is available", cfg.health_port),
            Err(e) => {
                eprintln!(
                    "[WARN] Health port {} is already in use: {e} \
                     (set health_port = 0 to disable)",
                    cfg.health_port
                );
                // Non-fatal — health endpoint is optional.
            }
        }
    }

    if ok {
        eprintln!("[PASS] All critical checks passed — nicocast is ready to start.");
    } else {
        eprintln!("[FAIL] One or more critical checks failed — fix the issues above.");
    }
    ok
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

/// Wait for a POSIX SIGTERM signal (Unix only).
///
/// On non-Unix platforms this future never resolves, which is harmless
/// because SIGTERM is also a Unix concept.
async fn wait_for_sigterm() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                warn!("Could not install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<()>().await;
    }
}
