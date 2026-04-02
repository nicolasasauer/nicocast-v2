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
//!
//! # Resilience
//!
//! * **WFDIEs**: If the D-Bus `Properties.Set WFDIEs` call fails (e.g. the
//!   running wpa_supplicant build was compiled without Wi-Fi Display support),
//!   the manager falls back to `wpa_cli wfd_subelem_set`.  If that also fails
//!   a warning is logged and the manager continues without WFD IEs — P2P
//!   discovery will still work, but Samsung Smart View may not show the sink.
//! * **wpa_supplicant readiness**: `P2pManager::new` retries the initial
//!   D-Bus handshake up to `WPA_INIT_MAX_RETRIES` times so that the manager
//!   survives a race between nicocast and wpa_supplicant startup.
//! * **P2P capability**: Before entering the discovery loop the manager checks
//!   whether the interface exposes the `fi.w1.wpa_supplicant1.Interface.P2PDevice`
//!   sub-interface and logs a clear warning when it does not.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::sync::{atomic::Ordering, Arc};
use tracing::{debug, info, warn};
use zbus::{proxy, Connection};
use zvariant::OwnedValue;

use crate::config::Config;
use crate::health::{AppState, STATE_IDLE, STATE_P2P_DISCOVERING};

// ─── tunables ────────────────────────────────────────────────────────────────


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

/// Proxy for `fi.w1.wpa_supplicant1.Interface` (D-Bus v2).
///
/// Used to probe whether the interface is P2P-capable before entering the
/// discovery loop.  A successful read of `Capabilities` confirms that
/// wpa_supplicant is managing this interface; the presence of the
/// `fi.w1.wpa_supplicant1.Interface.P2PDevice` sub-interface is checked
/// separately via `WpaP2PDeviceProxy`.
#[proxy(
    interface = "fi.w1.wpa_supplicant1.Interface",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaInterface {
    /// Capabilities dictionary — keyed on subsystem name (e.g. `"AuthAlg"`,
    /// `"Modes"`, `"Scan"`, `"P2P"` …).  Each value is an array of strings
    /// (`as`).  The exact set of keys depends on the wpa_supplicant build.
    #[zbus(property)]
    fn capabilities(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
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

/// Parse the first WFD subelement from a raw IE byte slice and return
/// `(subelement_id, payload_hex)`.
///
/// The encoding used by wpa_supplicant for the `WFDIEs` D-Bus property and
/// `wfd_subelem_set` is: `ID (1 byte) | Length (1 byte) | payload (Length bytes)`.
/// The returned `payload_hex` is the hex-encoded payload — i.e. the bytes
/// *after* the 2-byte header — which is what `wpa_cli wfd_subelem_set`
/// expects.
fn parse_wfd_subelement(ie_bytes: &[u8]) -> Result<(u8, String)> {
    if ie_bytes.len() < 2 {
        bail!(
            "WFD IE too short to parse subelement header ({} bytes)",
            ie_bytes.len()
        );
    }

    let subelement_id = ie_bytes[0];
    let declared_len = ie_bytes[1] as usize;

    if ie_bytes.len() < 2 + declared_len {
        bail!(
            "WFD IE length mismatch: header says {declared_len} bytes but only {} available",
            ie_bytes.len().saturating_sub(2)
        );
    }

    let payload_hex: String = ie_bytes[2..2 + declared_len]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    Ok((subelement_id, payload_hex))
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
    ///
    /// # wpa_supplicant readiness
    ///
    /// wpa_supplicant may not have finished initialising when nicocast starts.
    /// This constructor retries the D-Bus handshake up to
    /// [`WPA_INIT_MAX_RETRIES`] times with a [`WPA_INIT_RETRY_DELAY_SECS`]
    /// second delay between attempts so that a transient "service not found"
    /// error does not cause an immediate failure.
    pub async fn new(cfg: &Config) -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("connecting to system D-Bus")?;

        // Retry loop — wpa_supplicant may not be fully initialised yet.
        let iface_path = Self::get_or_create_interface(&conn, cfg).await?;

        debug!("P2PDevice D-Bus path: {iface_path}");

        // Check P2P capability before proceeding.
        Self::check_p2p_capability(&conn, &iface_path, &cfg.wifi_interface).await;

        Ok(Self {
            cfg: cfg.clone(),
            conn,
            iface_path,
        })
    }

    /// Attempt to obtain the wpa_supplicant D-Bus object path for the
    /// configured interface, retrying up to `cfg.p2p.connect_retries` times to
    /// tolerate a slow wpa_supplicant startup.
    async fn get_or_create_interface(
        conn: &Connection,
        cfg: &Config,
    ) -> Result<zbus::zvariant::OwnedObjectPath> {
        // `connect_retries = 0` means no attempts are made and the
        // function returns an error immediately.
        let max_retries = cfg.p2p.connect_retries;
        let retry_delay = cfg.p2p.connect_retry_secs;
        let mut last_err: Option<anyhow::Error> = None;

        for attempt in 1..=max_retries {
            let wpa = match WpaSupplicantProxy::new(conn).await {
                Ok(p) => p,
                Err(e) => {
                    let err = anyhow::Error::from(e)
                        .context("creating WpaSupplicant proxy (fi.w1.wpa_supplicant1)");
                    warn!(
                        "wpa_supplicant proxy not yet available \
                         (attempt {attempt}/{max_retries}): {err}"
                    );
                    last_err = Some(err);
                    if attempt < max_retries {
                        tokio::time::sleep(tokio::time::Duration::from_secs(retry_delay))
                            .await;
                    }
                    continue;
                }
            };

            match wpa.get_interface(&cfg.wifi_interface).await {
                Ok(path) => {
                    info!("wpa_supplicant: found interface '{}'", cfg.wifi_interface);
                    return Ok(path);
                }
                Err(get_err) => {
                    // If the error looks like "not found" try CreateInterface
                    // once.  Any other error is treated as transient and we retry.
                    let err_str = get_err.to_string();
                    let is_unknown = err_str.contains("InterfaceUnknown")
                        || err_str.contains("UnknownObject")
                        || err_str.contains("NoSuchInterface");

                    if is_unknown {
                        warn!(
                            "wpa_supplicant: GetInterface('{}') not found ({get_err}), \
                             attempting CreateInterface",
                            cfg.wifi_interface
                        );
                        let mut args = HashMap::new();
                        args.insert(
                            "Ifname",
                            zbus::zvariant::Value::from(cfg.wifi_interface.as_str()),
                        );
                        return wpa
                            .create_interface(args)
                            .await
                            .context("fi.w1.wpa_supplicant1.CreateInterface failed");
                    }

                    let err = anyhow::Error::from(get_err).context(format!(
                        "GetInterface('{}') failed (attempt {attempt}/{max_retries})",
                        cfg.wifi_interface
                    ));
                    warn!("{err}; retrying in {retry_delay}s");
                    last_err = Some(err);
                }
            }

            if attempt < max_retries {
                tokio::time::sleep(tokio::time::Duration::from_secs(retry_delay)).await;
            }
        }

        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!(
                "wpa_supplicant did not respond after {max_retries} attempts"
            )
        }))
    }

    /// Probe whether the wpa_supplicant interface at `iface_path` exposes the
    /// P2P-Device sub-interface and whether P2P appears in the interface
    /// capabilities.  Logs a warning when P2P support looks absent but does
    /// **not** return an error — some wpa_supplicant builds expose P2P without
    /// advertising it in the Capabilities dict.
    async fn check_p2p_capability(
        conn: &Connection,
        iface_path: &zbus::zvariant::OwnedObjectPath,
        ifname: &str,
    ) {
        // Step 1: check the Capabilities property on the Interface object.
        let iface_proxy = match WpaInterfaceProxy::builder(conn).path(iface_path) {
            Ok(builder) => match builder.build().await {
                Ok(p) => Some(p),
                Err(e) => {
                    warn!(
                        "Could not build Interface proxy for '{ifname}' ({e}); \
                         assuming P2P is supported"
                    );
                    None
                }
            },
            Err(e) => {
                warn!(
                    "Could not set path on Interface proxy for '{ifname}' ({e}); \
                     assuming P2P is supported"
                );
                None
            }
        };

        if let Some(proxy) = iface_proxy {
            match proxy.capabilities().await {
                Ok(caps) => {
                    let has_p2p = caps.contains_key("P2P") || caps.contains_key("p2p");
                    if has_p2p {
                        info!("Interface '{ifname}' reports P2P capability — OK");
                    } else {
                        warn!(
                            "Interface '{ifname}' Capabilities dict does not contain a 'P2P' \
                             key; wpa_supplicant may have been built without P2P support. \
                             Attempting P2P operations anyway."
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Could not read Capabilities for '{ifname}' ({e}); \
                         assuming P2P is supported"
                    );
                }
            }
        }

        // Step 2: confirm the P2PDevice sub-interface is accessible.
        let p2p_proxy = match WpaP2PDeviceProxy::builder(conn).path(iface_path) {
            Ok(builder) => match builder.build().await {
                Ok(p) => Some(p),
                Err(e) => {
                    warn!(
                        "Could not build P2PDevice proxy for '{ifname}' ({e}); proceeding anyway"
                    );
                    None
                }
            },
            Err(e) => {
                warn!(
                    "Could not set path on P2PDevice proxy for '{ifname}' ({e}); \
                     proceeding anyway"
                );
                None
            }
        };

        if let Some(proxy) = p2p_proxy {
            match proxy.p2p_device_config().await {
                Ok(_) => {
                    info!("P2PDevice sub-interface is accessible on '{ifname}'");
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("UnknownInterface")
                        || msg.contains("NoSuchInterface")
                        || msg.contains("org.freedesktop.DBus.Error.UnknownInterface")
                    {
                        warn!(
                            "Interface '{ifname}' does not expose \
                             fi.w1.wpa_supplicant1.Interface.P2PDevice ({e}). \
                             P2P operations will likely fail. \
                             Ensure wpa_supplicant.conf contains 'p2p_disabled=0' \
                             and was compiled with CONFIG_P2P=y."
                        );
                    } else {
                        warn!(
                            "P2PDevice config probe on '{ifname}' returned an unexpected \
                             error ({e}); proceeding anyway"
                        );
                    }
                }
            }
        }
    }

    /// Configure WFD IEs and P2PDeviceConfig, then enter the discovery loop.
    ///
    /// This method runs indefinitely.  If the initial configuration or
    /// discovery-start fails it logs a warning and retries after
    /// `cfg.p2p.connect_retry_secs` seconds.  After each `cfg.p2p.listen_secs`
    /// interval it re-issues `P2PDevice.Find` + `Listen` to keep the device
    /// discoverable without relying on systemd restarts.
    pub async fn run(&mut self, state: Arc<AppState>) -> Result<()> {
        let retry_delay = tokio::time::Duration::from_secs(self.cfg.p2p.connect_retry_secs);

        loop {
            // Build the D-Bus proxy once per outer iteration so that the
            // introspection round-trip is not repeated on every refresh cycle.
            let proxy = match self.p2p_proxy().await {
                Ok(p) => p,
                Err(e) => {
                    warn!(
                        "Could not build P2PDevice proxy: {e:#}; \
                         retrying in {}s",
                        self.cfg.p2p.connect_retry_secs
                    );
                    tokio::time::sleep(retry_delay).await;
                    continue;
                }
            };

            self.configure_wfd_ies(&proxy).await;

            if let Err(e) = self.configure_p2p_device_config(&proxy).await {
                warn!(
                    "P2P device config failed: {e:#}; \
                     retrying in {}s",
                    self.cfg.p2p.connect_retry_secs
                );
                tokio::time::sleep(retry_delay).await;
                continue;
            }

            if let Err(e) = self.start_discovery(&proxy).await {
                warn!(
                    "P2P discovery start failed: {e:#}; \
                     retrying in {}s",
                    self.cfg.p2p.connect_retry_secs
                );
                tokio::time::sleep(retry_delay).await;
                continue;
            }

            state.p2p.store(STATE_P2P_DISCOVERING, Ordering::Relaxed);
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
                if let Err(e) = self.start_discovery(&proxy).await {
                    warn!(
                        "Failed to refresh P2P discovery: {e}; \
                         restarting P2P configuration"
                    );
                    state.p2p.store(STATE_IDLE, Ordering::Relaxed);
                    break; // break inner → outer loop retries configuration
                }
            }
        }
    }

    // ── private helpers ──────────────────────────────────────────────────────

    /// Set the WFD Information Elements on the P2PDevice interface.
    ///
    /// Failure is **non-fatal**: if the D-Bus call fails (e.g. the
    /// wpa_supplicant build was compiled without Wi-Fi Display support and the
    /// `WFDIEs` property does not exist), the manager falls back to
    /// `wpa_cli wfd_subelem_set`.  If that also fails a warning is logged and
    /// the manager continues without WFD IEs.  P2P discovery will still work
    /// but Samsung Smart View may not recognise the sink as a Miracast display.
    async fn configure_wfd_ies(&self, proxy: &WpaP2PDeviceProxy<'_>) {
        let ie_bytes = match hex_to_bytes(&self.cfg.p2p.wfd_subelems)
            .context("decoding wfd_subelems hex string")
        {
            Ok(b) => b,
            Err(e) => {
                warn!("Invalid wfd_subelems configuration ({e}); skipping WFD IE setup");
                return;
            }
        };

        info!(
            "Setting P2PDevice.WFDIEs ({} bytes): {}",
            ie_bytes.len(),
            self.cfg.p2p.wfd_subelems
        );

        match proxy.set_wfd_ies(&ie_bytes).await {
            Ok(()) => {
                debug!("WFDIEs set via D-Bus successfully");
                return;
            }
            Err(e) => {
                warn!(
                    "org.freedesktop.DBus.Properties.Set WFDIEs failed ({e}); \
                     trying wpa_cli fallback"
                );
            }
        }

        // D-Bus path failed — attempt wpa_cli fallback.
        if let Err(cli_err) = self.set_wfd_ies_via_wpa_cli(&ie_bytes).await {
            warn!(
                "wpa_cli WFD fallback also failed ({cli_err}); \
                 continuing without WFD IEs — Samsung Smart View discovery may not work"
            );
        }
    }

    /// Fallback: configure WFD IEs by calling `wpa_cli wfd_subelem_set`.
    ///
    /// This is tried when the D-Bus `Properties.Set WFDIEs` call fails.  The
    /// `ie_bytes` slice must be the raw (already decoded) IE data — the first
    /// two bytes carry the subelement header (`ID || length`) and the
    /// remaining bytes are the payload passed to `wfd_subelem_set`.
    async fn set_wfd_ies_via_wpa_cli(&self, ie_bytes: &[u8]) -> Result<()> {
        let iface = &self.cfg.wifi_interface;

        let (subelement_id, payload_hex) =
            parse_wfd_subelement(ie_bytes).context("parsing WFD subelement for wpa_cli")?;

        // Enable Wi-Fi Display mode first.
        let enable_out = tokio::process::Command::new("wpa_cli")
            .args(["-i", iface, "set", "wifi_display", "1"])
            .output()
            .await
            .context("wpa_cli set wifi_display 1")?;

        if !enable_out.status.success() {
            bail!(
                "wpa_cli set wifi_display 1 failed (exit {}): {}",
                enable_out.status,
                String::from_utf8_lossy(&enable_out.stderr).trim()
            );
        }

        // Set the WFD subelement payload.
        let sub_id_str = subelement_id.to_string();
        let set_out = tokio::process::Command::new("wpa_cli")
            .args(["-i", iface, "wfd_subelem_set", &sub_id_str, &payload_hex])
            .output()
            .await
            .context("wpa_cli wfd_subelem_set")?;

        if !set_out.status.success() {
            bail!(
                "wpa_cli wfd_subelem_set failed (exit {}): {}",
                set_out.status,
                String::from_utf8_lossy(&set_out.stderr).trim()
            );
        }

        info!(
            "WFDIEs applied via wpa_cli (subelement {subelement_id}, \
             {} payload bytes)",
            payload_hex.len() / 2
        );
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
    async fn configure_p2p_device_config(&self, proxy: &WpaP2PDeviceProxy<'_>) -> Result<()> {
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

        proxy
            .set_p2p_device_config(config)
            .await
            .context(
                "org.freedesktop.DBus.Properties.Set P2PDeviceConfig on \
                 fi.w1.wpa_supplicant1.Interface.P2PDevice",
            )?;

        Ok(())
    }

    /// Start P2P peer discovery followed by a listen window.
    async fn start_discovery(&self, proxy: &WpaP2PDeviceProxy<'_>) -> Result<()> {
        // `listen_secs` is a u32; P2PDevice.Listen expects i32 (D-Bus type `i`).
        // Clamp to i32::MAX to avoid silent wrapping on implausibly large values.
        let timeout_secs = i32::try_from(self.cfg.p2p.listen_secs).unwrap_or(i32::MAX);

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
        // Format used by wpa_supplicant WFDIEs:
        //   ID(1) | Length(1) | DevInfo(2 BE) | CtrlPort(2 BE) | MaxTput(2 BE)
        let bytes = hex_to_bytes("000600111c4400c8").unwrap();
        assert_eq!(bytes[0], 0x00, "subelement ID must be 0x00 (WFD Device Info)");
        assert_eq!(bytes[1], 6, "subelement length must be 6");
        let dev_info = u16::from_be_bytes([bytes[2], bytes[3]]);
        assert_eq!(
            dev_info & 0x0003,
            0x0001,
            "bits 1:0 must be 01 (Primary Sink)"
        );
        assert_ne!(dev_info & 0x0010, 0, "bit 4 must be 1 (Session Available)");
        let ctrl_port = u16::from_be_bytes([bytes[4], bytes[5]]);
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

    // ── parse_wfd_subelement ──────────────────────────────────────────────────

    #[test]
    fn parse_wfd_subelement_default_ie() {
        // Default WFD IE: 000600111c4400c8
        // ID=0x00, Length=6 (1 byte), Payload=00111c4400c8
        let bytes = hex_to_bytes("000600111c4400c8").unwrap();
        let (id, payload_hex) = parse_wfd_subelement(&bytes).unwrap();
        assert_eq!(id, 0x00, "subelement ID");
        assert_eq!(payload_hex, "00111c4400c8", "payload hex (without header)");
    }

    #[test]
    fn parse_wfd_subelement_id_extracted() {
        // Subelement with ID=1, length=2 (1 byte), payload=[AB, CD]
        let bytes: Vec<u8> = vec![0x01, 0x02, 0xAB, 0xCD];
        let (id, payload_hex) = parse_wfd_subelement(&bytes).unwrap();
        assert_eq!(id, 1);
        assert_eq!(payload_hex, "abcd");
    }

    #[test]
    fn parse_wfd_subelement_too_short_errors() {
        assert!(parse_wfd_subelement(&[]).is_err(), "empty slice needs at least 2 bytes");
        // Header says length=3 but no payload bytes present
        assert!(parse_wfd_subelement(&[0x00, 0x03]).is_err(), "length mismatch");
    }

    #[test]
    fn parse_wfd_subelement_length_mismatch_errors() {
        // Header says length=10 but only 2 payload bytes are present
        let bytes: Vec<u8> = vec![0x00, 0x0A, 0x01, 0x02];
        assert!(parse_wfd_subelement(&bytes).is_err(), "length mismatch");
    }

    #[test]
    fn parse_wfd_subelement_zero_length_payload() {
        // Subelement with a zero-length payload (ID=0, Length=0)
        let bytes: Vec<u8> = vec![0x00, 0x00];
        let (id, payload_hex) = parse_wfd_subelement(&bytes).unwrap();
        assert_eq!(id, 0x00);
        assert_eq!(payload_hex, "", "empty payload hex");
    }
}
