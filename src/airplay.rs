//! AirPlay (iPhone / iPad) reception via UxPlay.
//!
//! This module manages `uxplay` as a supervised child process.  When enabled
//! (`airplay_enabled = true` in `config.toml`) NicoCast spawns `uxplay` at
//! startup and automatically restarts it if it exits unexpectedly.
//!
//! # Prerequisites
//!
//! `uxplay` must be installed on the Raspberry Pi:
//!
//! ```bash
//! sudo apt install uxplay
//! ```
//!
//! `setup.sh` installs it automatically when `airplay_enabled = true`.
//!
//! # Network interface note
//!
//! `wlan0` cannot be in P2P mode (Samsung Miracast) and in normal station /
//! AP mode (AirPlay) simultaneously.  If both `airplay_enabled` and the P2P
//! manager are active, they will use different radio modes.  Use a second
//! wireless interface or select the operating mode via `mode` in the config.

use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::Config;

/// Restart delay after an unexpected UxPlay exit (seconds).
const RESTART_DELAY_SECS: u64 = 5;

/// Run the UxPlay AirPlay receiver as a supervised child process.
///
/// When `cfg.airplay_enabled` is `false` this function returns `Ok(())`
/// immediately and does nothing.  Otherwise it enters an infinite loop,
/// spawning `uxplay` and restarting it whenever it exits.
pub async fn run_uxplay(cfg: &Config) -> Result<()> {
    if !cfg.airplay_enabled {
        // AirPlay disabled — block forever without consuming resources.
        std::future::pending::<()>().await;
        return Ok(());
    }

    info!(
        "AirPlay (UxPlay) enabled — advertising as '{}'",
        cfg.airplay_name
    );

    loop {
        info!("Starting uxplay process");

        let mut child = Command::new("uxplay")
            .arg("-n")
            .arg(&cfg.airplay_name)
            // Direct audio/video to the same sinks NicoCast uses.
            .arg("-vs")
            .arg("autovideosink")
            .arg("-as")
            .arg("autoaudiosink")
            .spawn()
            .context("spawning uxplay — is it installed? (apt install uxplay)")?;

        match child.wait().await {
            Ok(status) if status.success() => {
                info!("uxplay exited cleanly; restarting in {RESTART_DELAY_SECS}s");
            }
            Ok(status) => {
                warn!(
                    "uxplay exited with non-zero status {status}; \
                     restarting in {RESTART_DELAY_SECS}s"
                );
            }
            Err(e) => {
                error!("uxplay process error: {e}; restarting in {RESTART_DELAY_SECS}s");
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(RESTART_DELAY_SECS)).await;
    }
}
