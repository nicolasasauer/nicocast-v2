//! Configuration loading from a TOML file with `serde`.
//!
//! A `Config` is read from `config.toml` at startup. Every field has a
//! sensible default so the application can run without a configuration file.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Human-readable name advertised over P2P (shown in Samsung Smart View).
    #[serde(default = "default_device_name")]
    pub device_name: String,

    /// Linux network interface used for WiFi Direct (e.g. `wlan0`).
    #[serde(default = "default_wifi_interface")]
    pub wifi_interface: String,

    /// Linux network interface for USB Ethernet Gadget (e.g. `usb0`).
    #[serde(default = "default_usb_interface")]
    pub usb_interface: String,

    /// RTSP control-plane port (Miracast spec: 7236).
    #[serde(default = "default_rtsp_port")]
    pub rtsp_port: u16,

    /// UDP port on which RTP/MPEG-TS data will be received.
    #[serde(default = "default_rtp_port")]
    pub rtp_port: u16,

    /// Path to the persistent log file.
    #[serde(default = "default_log_file")]
    pub log_file: String,

    /// wpa_supplicant P2P-specific settings.
    #[serde(default)]
    pub p2p: P2pConfig,
}

/// P2P / WiFi Direct tuning knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2pConfig {
    /// WPS Primary Device Type advertised to peers (Display/Television).
    /// Format: `{category}-{OUI}-{subcategory}`.  Category 7 = Display.
    #[serde(default = "default_wps_dev_type")]
    pub wps_dev_type: String,

    /// Disable per-group virtual interface creation so the main wlan0
    /// keeps its address during a P2P session.  Corresponds to the
    /// wpa_supplicant `p2p_no_group_iface=0` setting.
    #[serde(default = "default_p2p_no_group_iface")]
    pub no_group_iface: bool,

    /// WFD IE subelements (hex-encoded) injected into P2P beacons so
    /// that Samsung Smart View recognises this device as a Miracast sink.
    ///
    /// The default value encodes:
    ///   * Subelement 0x00 — WFD Device Information
    ///     * Device type  : Primary Sink (bits 1:0 = 01)
    ///     * Session flag : Available   (bit  4   = 1)
    ///     → Device Info bitmap = 0x0011
    ///   * Control port  : 0x1C44 = 7236
    ///   * Max throughput: 0x00C8 = 200 Mbps
    #[serde(default = "default_wfd_subelems")]
    pub wfd_subelems: String,

    /// Timeout in seconds for the P2P listen / discovery phase.
    #[serde(default = "default_p2p_listen_secs")]
    pub listen_secs: u32,
}

// ─── defaults ────────────────────────────────────────────────────────────────

fn default_device_name() -> String {
    "NicoCast-Sink".to_owned()
}
fn default_wifi_interface() -> String {
    "wlan0".to_owned()
}
fn default_usb_interface() -> String {
    "usb0".to_owned()
}
fn default_rtsp_port() -> u16 {
    7236
}
fn default_rtp_port() -> u16 {
    16384
}
fn default_log_file() -> String {
    "/var/log/miracast_rs.log".to_owned()
}
fn default_wps_dev_type() -> String {
    "7-0050F204-1".to_owned()
}
fn default_p2p_no_group_iface() -> bool {
    false // corresponds to p2p_no_group_iface=0 (group interface IS created)
}
fn default_wfd_subelems() -> String {
    // Subelement 0x00 (6 bytes): DevInfo=0x0011, Port=0x1C44, MaxTput=0x00C8
    "000600111c4400c8".to_owned()
}
fn default_p2p_listen_secs() -> u32 {
    300
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            wps_dev_type: default_wps_dev_type(),
            no_group_iface: default_p2p_no_group_iface(),
            wfd_subelems: default_wfd_subelems(),
            listen_secs: default_p2p_listen_secs(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
            wifi_interface: default_wifi_interface(),
            usb_interface: default_usb_interface(),
            rtsp_port: default_rtsp_port(),
            rtp_port: default_rtp_port(),
            log_file: default_log_file(),
            p2p: P2pConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from a TOML file at `path`.
    pub fn load(path: &str) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config file '{path}'"))?;
        toml::from_str(&text).with_context(|| format!("parsing config file '{path}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.rtsp_port, 7236);
        assert_eq!(cfg.rtp_port, 16384);
        assert_eq!(cfg.p2p.wps_dev_type, "7-0050F204-1");
        assert_eq!(cfg.p2p.wfd_subelems, "000600111c4400c8");
        assert!(!cfg.p2p.no_group_iface);
    }

    #[test]
    fn load_from_toml_string() {
        let toml = r#"
            device_name = "TestSink"
            wifi_interface = "wlan1"
            rtsp_port = 7236
            rtp_port = 16384
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse failed");
        assert_eq!(cfg.device_name, "TestSink");
        assert_eq!(cfg.wifi_interface, "wlan1");
        // Unspecified fields must fall back to defaults
        assert_eq!(cfg.p2p.wps_dev_type, "7-0050F204-1");
    }

    #[test]
    fn load_missing_file_returns_error() {
        assert!(Config::load("/nonexistent/path/config.toml").is_err());
    }
}
