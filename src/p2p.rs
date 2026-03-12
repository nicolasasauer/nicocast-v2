//! WiFi Direct (P2P) manager using `zbus` to communicate with wpa_supplicant.
//!
//! # Responsibilities
//!
//! * Connect to wpa_supplicant over the system D-Bus.
//! * Inject **WFD Information Elements** into P2P beacons so that Samsung
//!   Smart View lists this device as a Miracast Primary Sink.
//! * Set the **WPS Primary Device Type** to `7-0050F204-1`
//!   (Category 7 = Display, OUI 00:50:F2:04, Subcategory 1 = TV/Monitor).
//! * Disable `p2p_no_group_iface` (set to `0`) so that the main `wlan0`
//!   keeps its IP address while a P2P group is active.
//! * Start P2P discovery / listen and keep the device continuously
//!   discoverable.

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};
use zbus::{proxy, Connection};

use crate::config::Config;

// ─── D-Bus proxy definitions ─────────────────────────────────────────────────

/// Proxy for the top-level `fi.w1.wpa_supplicant1` object.
#[proxy(
    interface = "fi.w1.wpa_supplicant1",
    default_service = "fi.w1.wpa_supplicant1",
    default_path = "/fi/w1/wpa_supplicant1"
)]
trait WpaSupplicant {
    /// Returns the D-Bus object path of the interface named `ifname`.
    fn get_interface(&self, ifname: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    /// Create an interface entry in wpa_supplicant (used if not already present).
    fn create_interface(
        &self,
        args: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

/// Proxy for a `fi.w1.wpa_supplicant1.Interface` object.
#[proxy(
    interface = "fi.w1.wpa_supplicant1.Interface",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaInterface {
    /// Set a wpa_supplicant network / interface property by name and value.
    #[zbus(name = "Set")]
    fn set_property(&self, name: &str, value: zbus::zvariant::Value<'_>) -> zbus::Result<()>;
}

/// Proxy for `fi.w1.wpa_supplicant1.Interface.P2PDevice`.
#[proxy(
    interface = "fi.w1.wpa_supplicant1.Interface.P2PDevice",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaP2PDevice {
    /// Start P2P peer discovery.
    ///
    /// `args` is a dictionary; typical keys are `Timeout` (int32) and
    /// `RequestedDeviceTypes` (array of binary device-type blobs).
    fn find(
        &self,
        args: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    /// Stop P2P peer discovery.
    fn stop_find(&self) -> zbus::Result<()>;

    /// Start P2P listen mode (makes us discoverable without scanning).
    fn listen(&self, timeout: i32) -> zbus::Result<()>;

    /// Set a P2P-specific property on the interface (e.g. `WFDIEs`,
    /// `DeviceName`, `DevType`, `NoGroupIface`).
    #[zbus(name = "Set")]
    fn set_property(&self, name: &str, value: zbus::zvariant::Value<'_>) -> zbus::Result<()>;
}

// ─── WFD IE helpers ──────────────────────────────────────────────────────────

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

// ─── P2pManager ──────────────────────────────────────────────────────────────

/// Manages the WiFi Direct P2P lifecycle for the Miracast Sink.
pub struct P2pManager {
    cfg: Config,
    conn: Connection,
    iface_path: zbus::zvariant::OwnedObjectPath,
}

impl P2pManager {
    /// Connect to wpa_supplicant and look up the configured WiFi interface.
    pub async fn new(cfg: &Config) -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("connecting to system D-Bus")?;

        let wpa = WpaSupplicantProxy::new(&conn)
            .await
            .context("creating WpaSupplicant proxy")?;

        // Try to get the interface; create it if wpa_supplicant doesn't know it yet.
        let iface_path = match wpa.get_interface(&cfg.wifi_interface).await {
            Ok(path) => {
                info!("wpa_supplicant: found interface '{}'", cfg.wifi_interface);
                path
            }
            Err(e) => {
                warn!(
                    "wpa_supplicant: GetInterface failed ({}), attempting CreateInterface",
                    e
                );
                let mut args = std::collections::HashMap::new();
                args.insert("Ifname", zbus::zvariant::Value::from(cfg.wifi_interface.as_str()));
                wpa.create_interface(args)
                    .await
                    .context("CreateInterface failed")?
            }
        };

        debug!("Interface D-Bus path: {}", iface_path);

        Ok(Self {
            cfg: cfg.clone(),
            conn,
            iface_path,
        })
    }

    /// Configure WFD IEs and P2P settings, then enter the discovery loop.
    pub async fn run(&mut self) -> Result<()> {
        self.configure_wfd_ies().await?;
        self.configure_p2p_settings().await?;
        self.start_discovery().await?;

        // Keep the manager alive; real event handling (DeviceFound signals)
        // would be wired up here via a zbus SignalStream.
        info!(
            "P2P manager active — discoverable as '{}' ({})",
            self.cfg.device_name, self.cfg.p2p.wps_dev_type
        );
        loop {
            // Re-trigger listen every `listen_secs` seconds so the device stays
            // visible.  wpa_supplicant's listen timeout is a hard deadline.
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

    /// Push the WFD Information Elements into wpa_supplicant so that Samsung
    /// Smart View recognises us as a Miracast Primary Sink.
    ///
    /// The subelements are passed as a raw byte array via the `WFDIEs`
    /// property on the P2PDevice interface.
    async fn configure_wfd_ies(&self) -> Result<()> {
        let ie_bytes = hex_to_bytes(&self.cfg.p2p.wfd_subelems)
            .context("decoding wfd_subelems hex string")?;

        info!(
            "Setting WFD IEs ({} bytes): {}",
            ie_bytes.len(),
            self.cfg.p2p.wfd_subelems
        );

        let p2p = WpaP2PDeviceProxy::builder(&self.conn)
            .path(self.iface_path.clone())?
            .build()
            .await
            .context("building P2PDevice proxy")?;

        p2p.set_property("WFDIEs", zbus::zvariant::Value::from(ie_bytes))
            .await
            .context("setting WFDIEs property")?;

        Ok(())
    }

    /// Apply P2P-specific wpa_supplicant settings:
    ///
    /// * `DeviceName`  — the human-readable name shown in Smart View.
    /// * `DevType`     — `7-0050F204-1` (Display / Television).
    /// * `NoGroupIface`— `false` (p2p_no_group_iface=0): keeps `wlan0`
    ///                   usable while a P2P group is active.
    async fn configure_p2p_settings(&self) -> Result<()> {
        let p2p = WpaP2PDeviceProxy::builder(&self.conn)
            .path(self.iface_path.clone())?
            .build()
            .await
            .context("building P2PDevice proxy")?;

        info!(
            "Setting P2P DeviceName='{}' DevType='{}'",
            self.cfg.device_name, self.cfg.p2p.wps_dev_type
        );

        p2p.set_property(
            "DeviceName",
            zbus::zvariant::Value::from(self.cfg.device_name.as_str()),
        )
        .await
        .context("setting P2P DeviceName")?;

        p2p.set_property(
            "DevType",
            zbus::zvariant::Value::from(self.cfg.p2p.wps_dev_type.as_str()),
        )
        .await
        .context("setting P2P DevType")?;

        // p2p_no_group_iface=0 → NoGroupIface=false → a separate group iface
        // IS created, which is the correct behaviour for most drivers.
        p2p.set_property(
            "NoGroupIface",
            zbus::zvariant::Value::from(self.cfg.p2p.no_group_iface),
        )
        .await
        .context("setting P2P NoGroupIface")?;

        Ok(())
    }

    /// Start P2P peer discovery followed by a listen window.
    async fn start_discovery(&self) -> Result<()> {
        let p2p = WpaP2PDeviceProxy::builder(&self.conn)
            .path(self.iface_path.clone())?
            .build()
            .await
            .context("building P2PDevice proxy")?;

        let timeout_secs = self.cfg.p2p.listen_secs as i32;

        // Start discovery (scan for peers)
        let find_args: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> =
            std::collections::HashMap::new();
        p2p.find(find_args)
            .await
            .context("P2PDevice.Find failed")?;

        // Enter listen mode — makes us visible to remote scanners
        p2p.listen(timeout_secs)
            .await
            .context("P2PDevice.Listen failed")?;

        info!("P2P discovery started (listen timeout: {timeout_secs}s)");
        Ok(())
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
        // Verify the default WFD IE encodes a Primary Sink correctly.
        // Format: ID(1) | Length(2) | DevInfo(2) | CtrlPort(2) | MaxTput(2)
        let bytes = hex_to_bytes("000600111c4400c8").unwrap();
        assert_eq!(bytes[0], 0x00, "subelement ID must be 0x00 (Device Info)");
        assert_eq!(
            u16::from_be_bytes([bytes[1], bytes[2]]),
            6,
            "subelement length must be 6"
        );
        let dev_info = u16::from_be_bytes([bytes[3], bytes[4]]);
        assert_eq!(dev_info & 0x0003, 0x0001, "bits 1:0 must be 01 (Primary Sink)");
        assert_ne!(dev_info & 0x0010, 0, "bit 4 must be set (Session Available)");
        let ctrl_port = u16::from_be_bytes([bytes[5], bytes[6]]);
        assert_eq!(ctrl_port, 7236, "control port must be 7236");
    }
}
