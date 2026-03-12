# nicocast-v2

> **⚠️ Disclaimer:** Dieses Projekt wurde vollständig mit Hilfe von KI-Agenten (GitHub Copilot Coding Agent) erstellt. Der gesamte Code, die Dokumentation und die Konfigurationsdateien wurden durch KI generiert und können Fehler enthalten. Eine sorgfältige Prüfung vor dem produktiven Einsatz wird empfohlen.

Miracast Sink (Receiver) for the **Raspberry Pi Zero 2W**, written in Rust.

Supports **Samsung Smart View** out of the box via WiFi Direct (P2P) and
hardware-accelerated H.264 decoding.  Logs are always accessible over a
**USB Ethernet Gadget** (`usb0`) even while the WiFi interface is busy with
an active Miracast P2P session.

---

## Quick Start — on the Raspberry Pi

The recommended workflow requires no cross-compilation tools on your laptop.
A GitHub Actions workflow automatically builds the binary and publishes it as
a [GitHub Release](https://github.com/nicolasasauer/nicocast-v2/releases/latest).
`setup.sh` downloads it directly on the Pi.

**Requirements on the Pi:** Raspberry Pi OS Bookworm 64-bit, internet access,
`curl` (pre-installed), passwordless `sudo` (default on Raspberry Pi OS).

```bash
# 1. Clone the repository on the Raspberry Pi
git clone https://github.com/nicolasasauer/nicocast-v2.git
cd nicocast-v2

# 2. Run the setup script — it downloads the binary and handles everything
./setup.sh
```

`setup.sh` will (on aarch64 / Raspberry Pi):

1. Download the pre-built `aarch64` binary from the latest GitHub Release
2. Install the binary to `/usr/local/bin/nicocast` and config to `/etc/nicocast/`
3. Install GStreamer runtime dependencies via `apt`
4. Create and enable the `nicocast` systemd service (auto-starts at boot)
5. Configure the USB Ethernet Gadget (`usb0`) for persistent SSH/log access

> **Note:** If no release binary is available yet (e.g. on the very first push
> before GitHub Actions has completed), pass `--build` to compile from source
> instead: `./setup.sh --build`

### Follow live logs

```bash
# Follow systemd journal directly on the Pi
sudo journalctl -u nicocast -f

# Or stream it from your laptop (via USB Ethernet on 192.168.7.2)
ssh pi@192.168.7.2 'sudo journalctl -u nicocast -f --no-pager -o short-iso'
```

### Update to the latest build

Re-running `./setup.sh` is safe and idempotent.  It downloads the newest
release binary, re-installs it, and restarts the service.

---

## Developer Workflow — from a Laptop (Docker mode)

If you prefer to cross-compile and deploy in one step from your development
machine, `setup.sh` also supports a Docker mode.  It is selected automatically
on non-aarch64 hosts.

**Requirements:** Docker, `ssh`, `scp`, the Pi reachable over the network.

```bash
# 1. Clone on your laptop
git clone https://github.com/nicolasasauer/nicocast-v2.git
cd nicocast-v2

# 2. Build and deploy to the Pi in one step
./setup.sh                        # prompts for Pi SSH address
./setup.sh pi@192.168.1.42        # or pass the address directly
```

```bash
# Stream live logs from your laptop
./setup.sh --logs pi@192.168.7.2
```

---

## Architecture

```
  ┌──────────────────┐   WiFi Direct (P2P / wlan0)   ┌──────────────────────────────────────────┐
  │  Samsung Smart   │ ──────────────────────────────►│           Raspberry Pi Zero 2W           │
  │  View / Source   │◄──────────────────────────────  │                                          │
  └──────────────────┘   RTSP :7236 + RTP UDP :16384  │  ┌──────────────┐  ┌─────────────────┐  │
                                                        │  │  P2P Manager │  │   RTSP Server   │  │
                                                        │  │  (wlan0)     │  │   port 7236     │  │
                                                        │  │  WFD IEs     │  │   M1–M16 flow   │  │
                                                        │  └──────────────┘  └─────────────────┘  │
                                                        │           ↓                ↓             │
                                                        │  ┌────────────────────────────────────┐  │
                                                        │  │         GStreamer Pipeline          │  │
                                                        │  │  udpsrc → tsdemux → h264parse →    │  │
                                                        │  │  v4l2h264dec → autovideosink       │  │
                                                        │  └────────────────────────────────────┘  │
                                                        │                                          │
                                                        │  usb0  ◄── USB Ethernet Gadget ──────►  │
                                                        └──────────────────┬───────────────────────┘
                                                                           │ USB-C / Micro-USB OTG
  ┌──────────────────────────────────────────────────────────────────────────────────────────────┐
  │  Developer Laptop / Debug Host                                                               │
  │                                                                                              │
  │  ssh pi@192.168.7.2                            (static IP over USB Ethernet, always up)     │
  │  sudo tail -f /var/log/miracast_rs.log         (live log streaming)                         │
  └──────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Key design decision:** `wlan0` is dedicated to Miracast P2P.  `usb0` (USB
Ethernet Gadget) provides a completely independent management channel that
remains available regardless of WiFi activity.

---

## Hardware & OS Requirements

| Component | Detail |
|---|---|
| Board | Raspberry Pi Zero 2W (BCM2710A1, Cortex-A53 64-bit) |
| OS | Raspberry Pi OS **Bookworm** 64-bit (or any Debian Bookworm aarch64) |
| USB port | The micro-USB port labelled **USB** (OTG port) — *not* the PWR IN port |
| GStreamer | `gstreamer1.0-plugins-good` + `gstreamer1.0-plugins-bad` (runtime) |
| wpa_supplicant | ≥ 2.9, D-Bus v2 interface enabled (`CONFIG_CTRL_IFACE_DBUS_NEW=y`) |

---

## USB Ethernet Gadget — Persistent Log Access

The Raspberry Pi Zero 2W supports **USB gadget mode**: when plugged into a
laptop via its OTG port it appears as a standard RNDIS/ECM Ethernet adapter.
This gives you SSH and log access on a static IP (`192.168.7.2`) that is
**completely independent of the WiFi radio** — the kernel drivers for `usb0`
and `wlan0` do not share any resources.

### Why this matters for Miracast

During a Miracast session wpa_supplicant owns `wlan0` and drives P2P group
negotiation, key exchange, and IP assignment.  Any traffic on `wlan0`
(including SSH) competes with the real-time video stream.  The USB Gadget
channel eliminates this contention:

```
wlan0  ──  WiFi P2P  ──  Miracast video stream   (high bandwidth, latency sensitive)
usb0   ──  USB RNDIS ──  SSH / logs              (low bandwidth, always available)
```

---

### Step 1 — Enable the USB Gadget on the Raspberry Pi

Boot the Pi with a keyboard/display attached, or edit the SD card from
another machine.

#### 1a. Add the `dwc2` device-tree overlay

Open `/boot/firmware/config.txt` (Raspberry Pi OS Bookworm) and add the
following line **at the very end** of the file:

```ini
dtoverlay=dwc2,dr_mode=peripheral
```

> **Bullseye users:** the path is `/boot/config.txt`.

#### 1b. Load the `g_ether` module at boot

Open `/boot/firmware/cmdline.txt` and append `modules-load=dwc2,g_ether`
to the **single existing line**, separated by a space.  The file must remain
a single line — do not add line breaks.

Before:
```
console=serial0,115200 console=tty1 root=PARTUUID=... rootfstype=ext4 rootwait quiet
```

After:
```
console=serial0,115200 console=tty1 root=PARTUUID=... rootfstype=ext4 rootwait quiet modules-load=dwc2,g_ether
```

Reboot the Pi.  After booting, `ip link` should show a `usb0` interface.

---

### Step 2 — Assign a Static IP to `usb0` on the Pi

Choose the method that matches your Pi OS networking stack.

#### NetworkManager (default on Raspberry Pi OS Bookworm)

```bash
sudo nmcli connection add \
  type ethernet \
  ifname usb0 \
  con-name "USB Gadget" \
  ipv4.method manual \
  ipv4.addresses 192.168.7.2/24 \
  connection.autoconnect yes

sudo nmcli connection up "USB Gadget"
```

#### `/etc/network/interfaces` (Bullseye / dhcpcd setups)

Create `/etc/network/interfaces.d/usb0`:

```
allow-hotplug usb0
iface usb0 inet static
    address 192.168.7.2
    netmask 255.255.255.0
```

Then restart networking:

```bash
sudo systemctl restart networking
```

#### systemd-networkd

Create `/etc/systemd/network/10-usb0.network`:

```ini
[Match]
Name=usb0

[Network]
Address=192.168.7.2/24
```

```bash
sudo systemctl enable --now systemd-networkd
```

---

### Step 3 — Configure the Host (Laptop / Desktop)

When you plug the Pi's OTG port into your laptop a new network adapter
appears.  Assign it the address `192.168.7.1/24`.

#### Linux

```bash
# Replace usb0 with the actual interface name (check: ip link)
sudo ip addr add 192.168.7.1/24 dev usb0
sudo ip link set usb0 up
```

To persist across reboots, create `/etc/systemd/network/20-usb-gadget.network`:

```ini
[Match]
Name=usb0

[Network]
Address=192.168.7.1/24
```

#### macOS

1. **System Preferences → Network**
2. Select the **USB Ethernet (CDC ECM)** adapter (appears when the Pi is connected)
3. Configure **Manually**: IP `192.168.7.1`, Subnet Mask `255.255.255.0`
4. Click **Apply**

#### Windows

1. Open **Device Manager** → **Network Adapters**
2. Look for **"Remote NDIS Compatible Device"** or **"USB Ethernet/RNDIS Gadget"**
   - If it shows a yellow warning icon, right-click → **Update driver** →
     **Browse my computer** → **Let me pick** → select **"Remote NDIS
     Compatible Device"**
3. Open **Network & Internet Settings** → **Change adapter options**
4. Right-click the RNDIS adapter → **Properties** → **Internet Protocol
   Version 4 (TCP/IPv4)** → **Use the following IP address**:
   - IP address: `192.168.7.1`
   - Subnet mask: `255.255.255.0`

---

### Step 4 — Access the Logs

```bash
# Verify connectivity
ping 192.168.7.2

# SSH into the Pi (default user on Raspberry Pi OS: pi or the name you chose)
ssh pi@192.168.7.2

# Follow the live log on the Pi
sudo tail -f /var/log/miracast_rs.log

# Or stream it directly from your laptop without an interactive SSH session
ssh pi@192.168.7.2 "sudo tail -f /var/log/miracast_rs.log"

# Copy a snapshot of the log to your laptop for offline analysis
scp pi@192.168.7.2:/var/log/miracast_rs.log ./nicocast-$(date +%Y%m%d).log
```

#### Optional: log rotation

To prevent the log from growing indefinitely, create
`/etc/logrotate.d/nicocast` on the Pi:

```
/var/log/miracast_rs.log {
    daily
    rotate 7
    compress
    missingok
    notifempty
    create 640 root adm
    postrotate
        # nicocast re-opens the log file on SIGHUP
        systemctl kill -s HUP nicocast.service 2>/dev/null || true
    endscript
}
```

---

## CI/CD — Automated Binary Builds

Every push to `main` triggers the GitHub Actions workflow in
`.github/workflows/build.yml`, which:

1. Runs on an `ubuntu-latest` GitHub-hosted runner
2. Cross-compiles the binary for `aarch64-unknown-linux-gnu` inside the
   existing Docker container (same Dockerfile used for local builds)
3. Publishes the binary as a GitHub Release named **`latest`** — a rolling
   pre-release that is updated on every commit to `main`

The stable download URL is always:

```
https://github.com/nicolasasauer/nicocast-v2/releases/latest/download/nicocast-aarch64
```

`setup.sh` uses this URL automatically when run on the Pi.

Versioned releases (e.g. `v1.0.0`) are created when you push a semver tag:

```bash
git tag v1.0.0
git push origin v1.0.0
```

---

## Building for the Raspberry Pi (Cross-Compilation)

> **Recommended:** use `./setup.sh` on the Pi — it downloads the pre-built
> binary from GitHub Releases without requiring any local toolchain.
> For manual / offline builds, use the steps below.

The `Dockerfile` in this repository performs a complete cross-compilation
from any `x86_64` Linux host (or CI runner) to `aarch64-unknown-linux-gnu`.

**Prerequisites:** Docker with `linux/amd64` platform support.

```bash
# 1. Build the image (this cross-compiles the binary inside Docker)
docker build --platform linux/amd64 -t nicocast:latest .

# 2. Extract the aarch64 binary from the image
docker create --name nc_tmp nicocast:latest
docker cp nc_tmp:/usr/local/bin/nicocast ./nicocast-aarch64
docker rm nc_tmp

# 3. Copy the binary and default config to the Pi
scp nicocast-aarch64        pi@192.168.7.2:/usr/local/bin/nicocast
scp config.toml             pi@192.168.7.2:/etc/nicocast/config.toml

# 4. Make the binary executable on the Pi
ssh pi@192.168.7.2 "sudo chmod +x /usr/local/bin/nicocast"
```

### What the Dockerfile does

| Stage | Action |
|---|---|
| **builder** | Installs Debian arm64 GStreamer dev packages via `dpkg --add-architecture arm64`, then sets `PKG_CONFIG_LIBDIR` to the arm64 pkg-config path so the gstreamer-rs build script resolves the correct libraries. |
| **builder** | Compiles a stub binary first to cache all Cargo dependencies (layer cache). |
| **builder** | Compiles the real binary with `-C target-cpu=cortex-a53` for BCM2710A1. |
| **runtime** | Copies only the binary + GStreamer runtime plugins into a clean `debian:bookworm-slim` image. |

---

## Installing as a systemd Service

> **Recommended:** use `./setup.sh` — it creates and enables the service automatically.

Create `/etc/systemd/system/nicocast.service` on the Pi:

```ini
[Unit]
Description=NicoCast Miracast Sink
After=network.target dbus.service wpa_supplicant.service
Wants=dbus.service wpa_supplicant.service

[Service]
ExecStart=/usr/local/bin/nicocast
Restart=on-failure
RestartSec=5s
StandardOutput=journal
StandardError=journal
SyslogIdentifier=nicocast

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now nicocast
# Check status
sudo systemctl status nicocast
# Or follow logs via journald
sudo journalctl -u nicocast -f
```

---

## HDMI Status Monitor

The `status_monitor/` directory contains a small Python script that watches the
NicoCast log and displays the current Miracast state as a full-screen graphic on
the HDMI output of the Raspberry Pi.

### States

| Display text | Meaning |
|---|---|
| **Bereit** (green) | P2P discovery active — waiting for a Miracast source |
| **Verbinde…** (amber) | RTSP session in progress — source connected, negotiating |
| **Streaming** (blue) | GStreamer pipeline active — video is playing |

The image also shows the device name (`NicoCast-Sink`) and the current IP
addresses of `usb0` and `wlan0`.

### Requirements

```bash
sudo apt-get install python3-pil fbi
```

### Manual installation

> **Recommended:** use `./setup.sh` — it installs and enables the service automatically.

```bash
# Copy the script
sudo cp status_monitor/nicocast_status.py /usr/local/bin/nicocast_status.py
sudo chmod +x /usr/local/bin/nicocast_status.py

# Install and enable the systemd service
sudo cp status_monitor/nicocast-status.service \
  /etc/systemd/system/nicocast-status.service
sudo systemctl daemon-reload
sudo systemctl enable --now nicocast-status

# Follow its logs
sudo journalctl -u nicocast-status -f
```

---

## Configuration Reference

All settings live in `config.toml`.  The binary searches these paths **in
order** and uses the first file it finds:

1. `/etc/nicocast/config.toml` — system-wide location (installed by the
   Dockerfile / `scp` workflow)
2. `./config.toml` — current working directory (useful during development)

If neither file exists the built-in defaults shown below are used.  Every
field is optional.

| Key | Default | Description |
|---|---|---|
| `device_name` | `"NicoCast-Sink"` | Name shown in Samsung Smart View |
| `wifi_interface` | `"wlan0"` | Network interface for WiFi Direct |
| `usb_interface` | `"usb0"` | Network interface for USB Gadget management |
| `rtsp_port` | `7236` | RTSP control-plane port (Miracast spec) |
| `rtp_port` | `16384` | UDP port for incoming RTP/MPEG-TS payload |
| `log_file` | `"/var/log/miracast_rs.log"` | Persistent log destination |
| `p2p.wps_dev_type` | `"7-0050F204-1"` | WPS Primary Device Type (Display / TV) |
| `p2p.no_group_iface` | `false` | Corresponds to `p2p_no_group_iface=0` |
| `p2p.wfd_subelems` | `"000600111c4400c8"` | WFD IE subelements (hex) |
| `p2p.listen_secs` | `300` | P2P listen window duration in seconds |

---

## Samsung Smart View — How Discovery Works

1. **WFD IE injection** — at startup the P2P manager writes the `WFDIEs`
   byte array to `fi.w1.wpa_supplicant1.Interface.P2PDevice` via
   `org.freedesktop.DBus.Properties.Set` (D-Bus interface **v2** only — the
   legacy `fi.epitest.hostap.WPASupplicant` interface is never used).
   These bytes are broadcast in P2P beacons, signalling to Samsung devices
   that a Miracast Primary Sink is available.

2. **Device type** — `P2PDeviceConfig.PrimaryDeviceType` is set to the
   8-byte encoding of `7-0050F204-1` (Category 7 = Display, Sub 1 = TV).
   Samsung Smart View filters for this type when populating its device list.

3. **RTSP handshake** — when a Samsung phone connects, the RTSP server
   on port 7236 responds to the M1–M7 message sequence.  The key Samsung
   compatibility requirement is `wfd_content_protection: none` in the
   `GET_PARAMETER` response (no HDCP required).

4. **Keep-alives** — the RTSP server replies to `GET_PARAMETER` requests
   with an empty body (M16 keep-alives) so the source does not time out the
   session.

---

## Troubleshooting

### `usb0` does not appear after reboot

- Verify `dtoverlay=dwc2,dr_mode=peripheral` is present in
  `/boot/firmware/config.txt` and that `modules-load=dwc2,g_ether` is on
  the single line in `/boot/firmware/cmdline.txt`.
- Confirm you are using the **USB** port (OTG), not the **PWR IN** port.
- Check `dmesg | grep dwc2` and `lsmod | grep g_ether`.

### Cannot SSH to `192.168.7.2` from Linux host

```bash
# Check the RNDIS/ECM interface name on your laptop
ip link show | grep -E "usb|eth"
# Assign the host-side IP if missing
sudo ip addr add 192.168.7.1/24 dev <interface>
sudo ip link set <interface> up
```

### nicocast is not visible in Samsung Smart View

1. Verify wpa_supplicant is running with D-Bus support:
   ```bash
   systemctl status wpa_supplicant
   busctl tree fi.w1.wpa_supplicant1
   ```
2. Check that `WFDIEs` was set correctly:
   ```bash
   sudo journalctl -u nicocast | grep "WFD IEs"
   ```
3. Inspect P2P discovery:
   ```bash
   sudo wpa_cli p2p_find
   sudo wpa_cli p2p_peers
   ```

### GStreamer pipeline errors (`v4l2h264dec` not found)

The `v4l2h264dec` element is provided by the `video4linux2` plugin in
`gstreamer1.0-plugins-good`.  Install it on the Pi:

```bash
sudo apt install gstreamer1.0-plugins-good
```

Also ensure the V4L2 codec driver is loaded:

```bash
ls /dev/video*
v4l2-ctl --list-devices
```
