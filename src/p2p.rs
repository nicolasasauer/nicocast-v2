//! WiFi Direct (P2P) manager using `zbus` to communicate with wpa_supplicant.
//!
//! # D-Bus interface version
//!
//! This module exclusively uses the **v2** wpa_supplicant D-Bus interface
//! (`fi.w1.wpa_supplicant1`).  The legacy `fi.epitest.hostap.WPASupplicant`
//! interface is intentionally not used.
//!
//! # Property access
//!
//! All wpa_supplicant properties are read and written through the standard
//! `org.freedesktop.DBus.Properties` interface (`Get` / `Set`).  zbus
//! generates these calls automatically when a proxy method is annotated with
//! `#[zbus(property)]` — no hand-rolled `Set` method is needed or used.
//!
//! # Key properties set at startup
//!
//! | Property on `P2PDevice` | Value | Effect |
//! |---|---|---|
//! | `WFDIEs` | `000600111c4400c8` | Advertises this device as a Miracast **Primary Sink** (session available, port 7236, 200 Mbps) |
//! | `P2PDeviceConfig.DeviceName` | `config.device_name` | Name shown in Samsung Smart View list |
//! | `P2PDeviceConfig.PrimaryDeviceType` | 8-byte blob for `7-0050F204-1` | Category 7 = Display, Sub 1 = TV/Monitor |
//! | `P2PDeviceConfig.NoGroupIface` | `false` | Maps to `p2p_no_group_iface=0`; a dedicated group interface is created so `wlan0` keeps its address |

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use tracing::{debug, info, warn};
use zbus::{proxy, Connection};
use zvariant::OwnedValue;

use crate::config::Config;

// ─── D-Bus proxy definitions ─────────────────────────────────────────────────

/// Proxy for the top-level `fi.w1.wpa_supplicant1` object (D-Bus v2).
#[proxy(
    interface = "fi.w1.wpa_supplicant1",
    default_service = "fi.w1.wpa_supplicant1",
    default_path = "/fi/w1/wpa_supplicant1"
)]
trait WpaSupplicant {
    /// Returns the D-Bus object path for the named network interface.
    fn get_interface(&self, ifname: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    /// Register a new interface with wpa_supplicant.
    /// Used as a fallback when `GetInterface` reports the interface is unknown.
    fn create_interface(
        &self,
        args: HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

/// Proxy for `fi.w1.wpa_supplicant1.Interface.P2PDevice` (D-Bus v2).
///
/// Properties annotated with `#[zbus(property)]` are accessed through the
/// standard `org.freedesktop.DBus.Properties` interface — zbus generates
/// the correct `Properties.Get` / `Properties.Set` D-Bus calls automatically.
#[proxy(
    interface = "fi.w1.wpa_supplicant1.Interface.P2PDevice",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaP2PDevice {
    // ── Methods ──────────────────────────────────────────────────────────────

    /// Start P2P peer discovery.  Pass an empty dict for default settings.
    fn find(&self, args: HashMap<&str, zbus::zvariant::Value<'_>>) -> zbus::Result<()>;

    /// Stop P2P peer discovery.
    fn stop_find(&self) -> zbus::Result<()>;

    /// Enter P2P listen mode for `timeout` seconds (makes us discoverable).
    fn listen(&self, timeout: i32) -> zbus::Result<()>;

    // ── Properties (via org.freedesktop.DBus.Properties) ─────────────────────

    /// **WFDIEs** (`ay`) — Wi-Fi Display Information Elements injected into
    /// P2P beacons.  Setting this via `org.freedesktop.DBus.Properties.Set`
    /// is the correct way to make Samsung Smart View recognise this device as
    /// a Miracast Primary Sink.
    #[zbus(property, name = "WFDIEs")]
    fn wfd_ies(&self) -> zbus::Result<Vec<u8>>;

    /// Setter for `WFDIEs`.
    /// zbus calls `org.freedesktop.DBus.Properties.Set(
    ///   "fi.w1.wpa_supplicant1.Interface.P2PDevice", "WFDIEs", <ay>)`.
    #[zbus(property, name = "WFDIEs")]
    fn set_wfd_ies(&self, ies: &[u8]) -> zbus::Result<()>;

    /// **P2PDeviceConfig** (`a{sv}`) — Dictionary of P2P configuration knobs.
    /// Relevant keys: `DeviceName` (s), `PrimaryDeviceType` (ay, 8 bytes),
    /// `NoGroupIface` (b).
    #[zbus(property, name = "P2PDeviceConfig")]
    fn p2p_device_config(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    /// Setter for `P2PDeviceConfig`.
    /// zbus calls `org.freedesktop.DBus.Properties.Set(
    ///   "fi.w1.wpa_supplicant1.Interface.P2PDevice", "P2PDeviceConfig", <a{sv}>)`.
    #[zbus(property, name = "P2PDeviceConfig")]
    fn set_p2p_device_config(
        &self,
        config: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Decode a hex string into raw bytes (case-insensitive).
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        bail!("hex string has odd length: '{hex}'");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .with_context(|| format!("invalid hex byte at position {i}: '{}'", &hex[i..i + 2]))
        })
        .collect()
}

/// Encode a WPS Primary Device Type string (`"{cat}-{OUI}-{subcat}"`) into
/// the 8-byte binary representation required by `PrimaryDeviceType` in
/// `P2PDeviceConfig`.
///
/// # Format
///
/// ```text
/// "7-0050F204-1"
///  │  └──────┘  └─ subcategory (u16 BE) → [0x00, 0x01]
///  │  └─────────── OUI+type (4 bytes)   → [0x00, 0x50, 0xF2, 0x04]
///  └────────────── category (u16 BE)    → [0x00, 0x07]  (7 = Display)
/// ```
fn wps_dev_type_to_bytes(dev_type: &str) -> Result<[u8; 8]> {
    let parts: Vec<&str> = dev_type.splitn(3, '-').collect();
    if parts.len() != 3 {
        bail!(
            "invalid WPS device type '{}': expected format N-XXXXXXXX-N",
            dev_type
        );
    }

    let category: u16 = parts[0]
        .parse()
        .with_context(|| format!("invalid category '{}' in WPS dev type", parts[0]))?;

    let oui = hex_to_bytes(parts[1])
        .with_context(|| format!("invalid OUI '{}' in WPS dev type", parts[1]))?;
    if oui.len() != 4 {
        bail!("WPS OUI must be exactly 4 bytes, got {}", oui.len());
    }

    let subcategory: u16 = parts[2]
        .parse()
        .with_context(|| format!("invalid subcategory '{}' in WPS dev type", parts[2]))?;

    let mut out = [0u8; 8];
    out[0..2].copy_from_slice(&category.to_be_bytes());
    out[2..6].copy_from_slice(&oui);
    out[6..8].copy_from_slice(&subcategory.to_be_bytes());
    Ok(out)
}

// ─── P2pManager ──────────────────────────────────────────────────────────────

/// Manages the WiFi Direct P2P lifecycle for the Miracast Sink.
pub struct P2pManager {
    cfg: Config,
    conn: Connection,
    iface_path: zbus::zvariant::OwnedObjectPath,
}

impl P2pManager {
    /// Connect to wpa_supplicant (D-Bus v2) and look up the configured WiFi
    /// interface.
    pub async fn new(cfg: &Config) -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("connecting to system D-Bus")?;

        let wpa = WpaSupplicantProxy::new(&conn)
            .await
            .context("creating WpaSupplicant proxy (fi.w1.wpa_supplicant1)")?;

        // Try to look up the interface; create it if wpa_supplicant doesn't
        // know about it yet (e.g. first boot after wpa_supplicant start).
        let iface_path = match wpa.get_interface(&cfg.wifi_interface).await {
            Ok(path) => {
                info!("wpa_supplicant: found interface '{}'", cfg.wifi_interface);
                path
            }
            Err(e) => {
                warn!(
                    "wpa_supplicant: GetInterface('{}') failed ({e}), \
                     attempting CreateInterface",
                    cfg.wifi_interface
                );
                let mut args = HashMap::new();
                args.insert(
                    "Ifname",
                    zbus::zvariant::Value::from(cfg.wifi_interface.as_str()),
                );
                wpa.create_interface(args)
                    .await
                    .context("fi.w1.wpa_supplicant1.CreateInterface failed")?
            }
        };

        debug!("P2PDevice D-Bus path: {iface_path}");

        Ok(Self {
            cfg: cfg.clone(),
            conn,
            iface_path,
        })
    }

    /// Configure WFD IEs and P2PDeviceConfig, then enter the discovery loop.
    pub async fn run(&mut self) -> Result<()> {
        self.configure_wfd_ies().await?;
        self.configure_p2p_device_config().await?;
        self.start_discovery().await?;

        info!(
            "P2P manager active — discoverable as '{}' ({})",
            self.cfg.device_name, self.cfg.p2p.wps_dev_type
        );

        // Refresh the listen window before wpa_supplicant's timeout expires.
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(
                self.cfg.p2p.listen_secs as u64,
            ))
            .await;
            debug!("Refreshing P2P listen window");
            if let Err(e) = self.start_discovery().await {
                warn!("Failed to refresh P2P discovery: {e}");
            }
        }
    }

    // ── private helpers ──────────────────────────────────────────────────────

    /// Set the WFD Information Elements on the P2PDevice interface.
    ///
    /// This calls `org.freedesktop.DBus.Properties.Set(
    ///   "fi.w1.wpa_supplicant1.Interface.P2PDevice", "WFDIEs", <ay>)` via
    /// the `#[zbus(property)]`-generated setter, which is the correct D-Bus
    /// v2 approach.  A custom `Set` method does **not** exist on this
    /// interface.
    async fn configure_wfd_ies(&self) -> Result<()> {
        let ie_bytes = hex_to_bytes(&self.cfg.p2p.wfd_subelems)
            .context("decoding wfd_subelems hex string")?;

        info!(
            "Setting P2PDevice.WFDIEs ({} bytes): {}",
            ie_bytes.len(),
            self.cfg.p2p.wfd_subelems
        );

        self.p2p_proxy()
            .await?
            .set_wfd_ies(&ie_bytes)
            .await
            .context(
                "org.freedesktop.DBus.Properties.Set WFDIEs on \
                 fi.w1.wpa_supplicant1.Interface.P2PDevice",
            )?;

        Ok(())
    }

    /// Write `DeviceName`, `PrimaryDeviceType` and `NoGroupIface` into the
    /// `P2PDeviceConfig` property dict via `org.freedesktop.DBus.Properties`.
    ///
    /// | Key | D-Bus type | Value |
    /// |---|---|---|
    /// | `DeviceName` | `s` | human-readable sink name (shown in Smart View) |
    /// | `PrimaryDeviceType` | `ay` | 8-byte WPS blob for `7-0050F204-1` |
    /// | `NoGroupIface` | `b` | `false` → `p2p_no_group_iface=0` |
    async fn configure_p2p_device_config(&self) -> Result<()> {
        let dev_type_bytes = wps_dev_type_to_bytes(&self.cfg.p2p.wps_dev_type)
            .context("encoding WPS primary device type")?;

        info!(
            "Setting P2PDeviceConfig: DeviceName='{}' PrimaryDeviceType='{}'",
            self.cfg.device_name, self.cfg.p2p.wps_dev_type
        );

        // Build the a{sv} dict.  OwnedValue wraps a zvariant Value and
        // carries its type signature, so HashMap<String, OwnedValue>
        // serialises correctly as a{sv} over D-Bus.
        let config: HashMap<String, OwnedValue> = [
            (
                "DeviceName".to_owned(),
                OwnedValue::try_from(zvariant::Value::from(self.cfg.device_name.as_str()))
                    .context("encoding DeviceName")?,
            ),
            (
                "PrimaryDeviceType".to_owned(),
                OwnedValue::try_from(zvariant::Value::from(dev_type_bytes.to_vec()))
                    .context("encoding PrimaryDeviceType")?,
            ),
            (
                "NoGroupIface".to_owned(),
                OwnedValue::try_from(zvariant::Value::from(self.cfg.p2p.no_group_iface))
                    .context("encoding NoGroupIface")?,
            ),
        ]
        .into_iter()
        .collect();

        self.p2p_proxy()
            .await?
            .set_p2p_device_config(config)
            .await
            .context(
                "org.freedesktop.DBus.Properties.Set P2PDeviceConfig on \
                 fi.w1.wpa_supplicant1.Interface.P2PDevice",
            )?;

        Ok(())
    }

    /// Start P2P peer discovery followed by a listen window.
    async fn start_discovery(&self) -> Result<()> {
        let proxy = self.p2p_proxy().await?;
        let timeout_secs = self.cfg.p2p.listen_secs as i32;

        proxy
            .find(HashMap::new())
            .await
            .context("P2PDevice.Find failed")?;

        proxy
            .listen(timeout_secs)
            .await
            .context("P2PDevice.Listen failed")?;

        info!("P2P discovery started (listen timeout: {timeout_secs}s)");
        Ok(())
    }

    /// Construct a `WpaP2PDeviceProxy` bound to the per-interface object path.
    async fn p2p_proxy(&self) -> Result<WpaP2PDeviceProxy<'_>> {
        WpaP2PDeviceProxy::builder(&self.conn)
            .path(self.iface_path.clone())
            .context("setting P2PDevice proxy path")?
            .build()
            .await
            .context("building WpaP2PDeviceProxy")
    }
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_to_bytes_valid() {
        let bytes = hex_to_bytes("000600111c4400c8").unwrap();
        assert_eq!(bytes, vec![0x00, 0x06, 0x00, 0x11, 0x1c, 0x44, 0x00, 0xc8]);
    }

    #[test]
    fn hex_to_bytes_case_insensitive() {
        let bytes = hex_to_bytes("DEADBEEF").unwrap();
        assert_eq!(bytes, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn hex_to_bytes_odd_length_errors() {
        assert!(hex_to_bytes("abc").is_err());
    }

    #[test]
    fn hex_to_bytes_invalid_char_errors() {
        assert!(hex_to_bytes("ZZZZ").is_err());
    }

    #[test]
    fn wfd_ie_subelement_structure() {
        // Format: ID(1) | Length(2 BE) | DevInfo(2 BE) | CtrlPort(2 BE) | MaxTput(2 BE)
        let bytes = hex_to_bytes("000600111c4400c8").unwrap();
        assert_eq!(bytes[0], 0x00, "subelement ID must be 0x00 (WFD Device Info)");
        assert_eq!(
            u16::from_be_bytes([bytes[1], bytes[2]]),
            6,
            "subelement length must be 6"
        );
        let dev_info = u16::from_be_bytes([bytes[3], bytes[4]]);
        assert_eq!(
            dev_info & 0x0003,
            0x0001,
            "bits 1:0 must be 01 (Primary Sink)"
        );
        assert_ne!(dev_info & 0x0010, 0, "bit 4 must be 1 (Session Available)");
        let ctrl_port = u16::from_be_bytes([bytes[5], bytes[6]]);
        assert_eq!(ctrl_port, 7236, "RTSP control port must be 7236");
    }

    #[test]
    fn wps_dev_type_display_television() {
        // "7-0050F204-1" → Category 7 (Display), OUI 00:50:F2:04, Sub 1 (TV)
        let bytes = wps_dev_type_to_bytes("7-0050F204-1").unwrap();
        // Full expected byte array: [cat_hi, cat_lo, oui×4, sub_hi, sub_lo]
        assert_eq!(
            bytes,
            [0x00, 0x07, 0x00, 0x50, 0xF2, 0x04, 0x00, 0x01],
            "complete 8-byte WPS blob for 7-0050F204-1"
        );
        assert_eq!(u16::from_be_bytes([bytes[0], bytes[1]]), 7, "category 7 = Display");
        assert_eq!(&bytes[2..6], &[0x00, 0x50, 0xF2, 0x04], "OUI 00:50:F2:04");
        assert_eq!(u16::from_be_bytes([bytes[6], bytes[7]]), 1, "subcategory 1 = TV");
    }

    #[test]
    fn wps_dev_type_bad_format_errors() {
        assert!(wps_dev_type_to_bytes("7-0050F204").is_err(), "missing subcategory");
        assert!(wps_dev_type_to_bytes("x-0050F204-1").is_err(), "non-numeric category");
        assert!(wps_dev_type_to_bytes("7-ZZZZZZZZ-1").is_err(), "invalid OUI hex");
        assert!(wps_dev_type_to_bytes("7-0050F2-1").is_err(), "OUI too short");
    }
}
