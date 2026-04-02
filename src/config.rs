//! Configuration loading from a TOML file with `serde`.
//!
//! A `Config` is read from `config.toml` at startup. Every field has a
//! sensible default so the application can run without a configuration file.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use tracing::warn;

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

    /// TCP port for the HTTP health endpoint (`GET /health`).
    /// Set to `0` to disable the endpoint entirely.
    #[serde(default = "default_health_port")]
    pub health_port: u16,

    /// Seconds of inactivity (no RTSP messages) after which an RTSP
    /// connection is considered dead and is closed.  Samsung devices send
    /// M16 keep-alives every ~30 s; the default of 60 s allows two missed
    /// keep-alives before the connection is torn down.
    #[serde(default = "default_rtsp_keepalive_secs")]
    pub rtsp_keepalive_secs: u64,

    /// When `true` the RTSP server sends a sink-initiated M2
    /// `GET_PARAMETER` request to the source immediately after responding
    /// to the source's M1 `OPTIONS`.  Some Samsung firmware versions
    /// require this exchange; leave it `false` for the common case.
    #[serde(default = "default_rtsp_send_m2")]
    pub rtsp_send_m2: bool,

    /// When `true` NicoCast spawns `uxplay` as a child process to provide
    /// AirPlay (iPhone / iPad) reception in addition to Miracast.
    #[serde(default = "default_airplay_enabled")]
    pub airplay_enabled: bool,

    /// Name advertised by UxPlay over mDNS (shown in iPhone Control Centre).
    /// Defaults to the device name with an `-AirPlay` suffix.
    #[serde(default = "default_airplay_name")]
    pub airplay_name: String,

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

    /// Maximum number of attempts to connect to wpa_supplicant on D-Bus
    /// before giving up.  Increase this on slow systems where wpa_supplicant
    /// takes a long time to start.
    #[serde(default = "default_p2p_connect_retries")]
    pub connect_retries: u32,

    /// Seconds to wait between successive wpa_supplicant connection attempts.
    #[serde(default = "default_p2p_connect_retry_secs")]
    pub connect_retry_secs: u64,
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
fn default_health_port() -> u16 {
    8080
}
fn default_rtsp_keepalive_secs() -> u64 {
    60
}
fn default_rtsp_send_m2() -> bool {
    false
}
fn default_airplay_enabled() -> bool {
    false
}
fn default_airplay_name() -> String {
    "NicoCast-AirPlay".to_owned()
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
fn default_p2p_connect_retries() -> u32 {
    5
}
fn default_p2p_connect_retry_secs() -> u64 {
    2
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            wps_dev_type: default_wps_dev_type(),
            no_group_iface: default_p2p_no_group_iface(),
            wfd_subelems: default_wfd_subelems(),
            listen_secs: default_p2p_listen_secs(),
            connect_retries: default_p2p_connect_retries(),
            connect_retry_secs: default_p2p_connect_retry_secs(),
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
            health_port: default_health_port(),
            rtsp_keepalive_secs: default_rtsp_keepalive_secs(),
            rtsp_send_m2: default_rtsp_send_m2(),
            airplay_enabled: default_airplay_enabled(),
            airplay_name: default_airplay_name(),
            p2p: P2pConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from a TOML file at `path`.
    ///
    /// After parsing the TOML the configuration is validated (e.g. the
    /// `p2p.wfd_subelems` field must be a valid even-length hex string).
    /// Validation errors are reported with the field name so the user can
    /// immediately identify and fix the problem.
    pub fn load(path: &str) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config file '{path}'"))?;
        let cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing config file '{path}'"))?;
        cfg.validate()
            .with_context(|| format!("validating config file '{path}'"))?;
        Ok(cfg)
    }

    /// Validate semantic constraints that TOML parsing cannot enforce.
    ///
    /// Currently checks:
    /// * `p2p.wfd_subelems` must be a non-empty, even-length hex string.
    /// * `rtsp_port`, `rtp_port`, and `health_port` must not collide with each
    ///   other (when non-zero).
    /// * `airplay_enabled = true` with P2P active is warned about because both
    ///   modes require exclusive use of the same `wlan0` interface.
    pub fn validate(&self) -> Result<()> {
        validate_hex_field("p2p.wfd_subelems", &self.p2p.wfd_subelems)?;

        // Port collision checks (skip health_port = 0, which disables it).
        if self.rtsp_port == self.rtp_port {
            bail!(
                "rtsp_port ({}) and rtp_port ({}) must not be the same",
                self.rtsp_port, self.rtp_port
            );
        }
        if self.health_port != 0 {
            if self.health_port == self.rtsp_port {
                bail!(
                    "health_port ({}) and rtsp_port ({}) must not be the same",
                    self.health_port, self.rtsp_port
                );
            }
            if self.health_port == self.rtp_port {
                bail!(
                    "health_port ({}) and rtp_port ({}) must not be the same",
                    self.health_port, self.rtp_port
                );
            }
        }

        // AirPlay and Miracast (P2P) both require exclusive use of wlan0;
        // warn the user so the conflict is visible at startup.
        if self.airplay_enabled {
            warn!(
                "airplay_enabled = true: AirPlay (uxplay) and Miracast (P2P) both require \
                 exclusive control of WiFi interface '{}'. Running both simultaneously will \
                 cause connectivity failures. Disable one mode or use separate interfaces.",
                self.wifi_interface
            );
        }

        Ok(())
    }
}

/// Verify that `value` is a non-empty, even-length string of ASCII hex digits.
fn validate_hex_field(field: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("config field '{field}' must not be empty");
    }
    if value.len() % 2 != 0 {
        bail!(
            "config field '{field}' has odd length ({} chars); \
             hex strings must encode complete bytes",
            value.len()
        );
    }
    for (i, ch) in value.chars().enumerate() {
        if !ch.is_ascii_hexdigit() {
            bail!(
                "config field '{field}' contains invalid hex character '{ch}' \
                 at position {i}; only 0-9 and a-f/A-F are allowed"
            );
        }
    }
    Ok(())
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
        assert_eq!(cfg.health_port, 8080);
        assert_eq!(cfg.rtsp_keepalive_secs, 60);
        assert!(!cfg.rtsp_send_m2);
        assert!(!cfg.airplay_enabled);
        assert_eq!(cfg.p2p.connect_retries, 5);
        assert_eq!(cfg.p2p.connect_retry_secs, 2);
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

    #[test]
    fn validate_accepts_valid_hex() {
        let cfg = Config::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_odd_length_hex() {
        let mut cfg = Config::default();
        cfg.p2p.wfd_subelems = "000600111c4400c".to_owned(); // odd length
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_invalid_hex_chars() {
        let mut cfg = Config::default();
        cfg.p2p.wfd_subelems = "000GXX11".to_owned(); // invalid chars
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_wfd_subelems() {
        let mut cfg = Config::default();
        cfg.p2p.wfd_subelems = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_rtsp_rtp_port_collision() {
        let mut cfg = Config::default();
        cfg.rtp_port = cfg.rtsp_port;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_health_rtsp_port_collision() {
        let mut cfg = Config::default();
        cfg.health_port = cfg.rtsp_port;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_health_rtp_port_collision() {
        let mut cfg = Config::default();
        cfg.health_port = cfg.rtp_port;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_allows_health_port_zero() {
        let mut cfg = Config::default();
        // health_port = 0 disables health endpoint; port "collision" checks
        // must not fire for the disabled port.
        cfg.health_port = 0;
        assert!(cfg.validate().is_ok());
    }
}
