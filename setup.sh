#!/usr/bin/env bash
# =============================================================================
# setup.sh — nicocast-v2 first-run setup
#
# What this script does:
#
#   Native mode (default on aarch64 / Raspberry Pi):
#   1. Downloads the pre-built aarch64 binary from GitHub Releases
#      (falls back to cargo build --release if download fails or --build used)
#   2. Installs the binary to /usr/local/bin and config to /etc/nicocast
#   3. Installs GStreamer runtime dependencies via apt
#   4. Creates and enables the nicocast systemd service (auto-starts at boot)
#   5. Installs the status monitor script and its systemd service
#   6. Configures USB Ethernet Gadget (usb0) for persistent SSH/log access
#
#   Docker mode (default on x86_64 — cross-compiles and deploys over SSH):
#   1. Builds the nicocast binary via Docker (cross-compilation for aarch64)
#   2. Copies the binary and config to the Raspberry Pi over SSH
#   3. Installs GStreamer runtime dependencies on the Pi
#   4. Creates and enables the nicocast systemd service
#   5. Configures log rotation
#   6. Deploys the status monitor script and its systemd service
#   7. Enables the USB Ethernet Gadget (usb0) for persistent log access
#
# Requirements (Native mode, run on the Pi itself):
#   - curl (pre-installed on Raspberry Pi OS)
#   - apt (Raspberry Pi OS / Debian)
#   - sudo privileges
#   - internet access (to download binary + GStreamer packages)
#   - Rust / cargo only needed when --build is passed (or download fails)
#
# Requirements (Docker mode, run on a developer laptop):
#   - Docker (running, accessible without sudo — or with sudo)
#   - ssh / scp
#   - The Raspberry Pi must be reachable over the network (WiFi or Ethernet)
#   - The Pi user must have passwordless sudo (default on Raspberry Pi OS)
#
# Usage:
#   ./setup.sh                        # auto-detect mode (Pi → native, x86 → Docker)
#   ./setup.sh --native               # force native mode on any arch
#   ./setup.sh --build                # native mode but compile from source
#   ./setup.sh pi@192.168.1.42        # Docker mode; use a specific Pi address
#   ./setup.sh --logs pi@192.168.7.2  # connect and follow live logs only
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY_LOCAL="$SCRIPT_DIR/nicocast-aarch64"

# ── Sudo helper ───────────────────────────────────────────────────────────────
# When already running as root, avoid spawning unnecessary sudo processes.
if [[ $EUID -eq 0 ]]; then
  SUDO=""
else
  SUDO="sudo"
fi

# ── Colors ────────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
  RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
  BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
else
  RED=''; GREEN=''; YELLOW=''; BLUE=''; BOLD=''; NC=''
fi

step()    { echo -e "\n${BOLD}${BLUE}──── $* ${NC}"; }
info()    { echo -e "  ${BLUE}·${NC} $*"; }
success() { echo -e "  ${GREEN}✓${NC} $*"; }
warn()    { echo -e "  ${YELLOW}⚠${NC}  $*"; }
error()   { echo -e "  ${RED}✗${NC}  $*" >&2; }
die()     { error "$*"; exit 1; }

# ── apt-get update with retry ─────────────────────────────────────────────────
# Transient mirror-sync errors cause apt-get update to exit non-zero; retry up
# to 3 times with exponential back-off before giving up.
apt_get_update() {
  local attempt=1 max=3 delay=10
  while [[ $attempt -le $max ]]; do
    if $SUDO apt-get update -qq; then
      return 0
    fi
    if [[ $attempt -lt $max ]]; then
      warn "apt-get update failed (attempt $attempt/$max) — retrying in ${delay}s…"
      sleep "$delay"
      delay=$(( delay * 2 ))
    fi
    (( attempt++ ))
  done
  die "apt-get update failed after $max attempts. Check your network and apt sources."
}

# ── Banner ────────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}  nicocast-v2 — Miracast Sink for Raspberry Pi Zero 2W${NC}"
echo    "  First-Run Setup Script"
echo    "  ─────────────────────────────────────────────────────"

# ── Parse arguments ───────────────────────────────────────────────────────────
LOGS_ONLY=false
NATIVE_MODE=false
FORCE_BUILD=false
PI_HOST=""

# Auto-detect native mode: if we are already running on aarch64 assume we are
# on the Raspberry Pi and can build natively without Docker.
ARCH="$(uname -m)"
if [[ "$ARCH" == "aarch64" || "$ARCH" == "arm64" ]]; then
  NATIVE_MODE=true
fi

while [[ $# -gt 0 ]]; do
  case "$1" in
    --logs)
      LOGS_ONLY=true
      shift
      ;;
    --native)
      NATIVE_MODE=true
      shift
      ;;
    --build)
      FORCE_BUILD=true
      NATIVE_MODE=true   # --build implies native mode
      shift
      ;;
    -*)
      die "Unknown option: $1  (usage: ./setup.sh [--logs] [--native] [--build] [pi@<address>])"
      ;;
    *)
      PI_HOST="$1"
      shift
      ;;
  esac
done

# ── --logs mode: just tail the live log ──────────────────────────────────────
if $LOGS_ONLY; then
  if [[ -z "$PI_HOST" ]]; then
    read -rp "  Enter Raspberry Pi SSH address [pi@192.168.7.2]: " PI_HOST
    PI_HOST="${PI_HOST:-pi@192.168.7.2}"
  fi
  echo ""
  info "Connecting to $PI_HOST and following live nicocast log…"
  info "(Press Ctrl-C to stop)"
  echo ""
  exec ssh "$PI_HOST" "sudo journalctl -u nicocast -f --no-pager -o short-iso"
fi

# ── Prompt for Pi address if not supplied (Docker mode only) ──────────────────
if ! $NATIVE_MODE; then
  if [[ -z "$PI_HOST" ]]; then
    read -rp "  Enter Raspberry Pi SSH address [pi@raspberrypi.local]: " PI_HOST
    PI_HOST="${PI_HOST:-pi@raspberrypi.local}"
  fi
  info "Target Pi: ${BOLD}${PI_HOST}${NC}"
fi

# =============================================================================
# ── NATIVE MODE ───────────────────────────────────────────────────────────────
# Runs entirely on the Raspberry Pi without Docker or SSH.
# By default, downloads the pre-built aarch64 binary published by the GitHub
# Actions workflow.  Pass --build to compile from source instead.
# =============================================================================
if $NATIVE_MODE; then
  if $FORCE_BUILD; then
    info "Mode: ${BOLD}Native Build (from source)${NC}  (arch: ${ARCH})"
  else
    info "Mode: ${BOLD}Native Install (pre-built binary)${NC}  (arch: ${ARCH})"
  fi

  # URL of the pre-built binary published by the GitHub Actions release workflow.
  # Uses the direct tag-based path (/releases/download/<tag>/) rather than the
  # /releases/latest/download/ redirect, because the rolling "latest" release is
  # published as a pre-release and GitHub's /releases/latest redirect only resolves
  # to non-prerelease releases (causing a 404 otherwise).
  RELEASE_URL="https://github.com/nicolasasauer/nicocast-v2/releases/download/latest/nicocast-aarch64"

  # ===========================================================================
  # Native Step 1/5 — Obtain the nicocast binary
  # ===========================================================================
  step "Step 1/5 — Obtaining nicocast binary"

  NATIVE_BINARY="/tmp/nicocast-download-$$"
  DOWNLOADED=false

  if ! $FORCE_BUILD; then
    info "Downloading pre-built binary from GitHub Releases…"
    if curl -fsSL --max-time 120 -o "$NATIVE_BINARY" "$RELEASE_URL"; then
      chmod +x "$NATIVE_BINARY"
      DOWNLOADED=true
      success "Downloaded pre-built binary  ($(du -sh "$NATIVE_BINARY" | cut -f1))"
    else
      warn "Download failed — falling back to building from source."
    fi
  fi

  if ! $DOWNLOADED; then
    # ── Check cargo ──────────────────────────────────────────────────────────
    if ! command -v cargo &>/dev/null; then
      echo ""
      error "cargo (Rust) is not installed."
      echo ""
      echo "  Install Rust via rustup:"
      echo ""
      echo "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
      echo "    source \$HOME/.cargo/env"
      echo ""
      echo "  Then re-run this script.  Alternatively, wait for a GitHub Actions"
      echo "  release build and re-run without --build to download it instead."
      exit 1
    fi
    success "cargo OK  ($(cargo --version))"

    info "Installing build dependencies via apt…"
    apt_get_update
    $SUDO apt-get install -y -qq \
      build-essential \
      pkg-config \
      libgstreamer1.0-dev \
      libgstreamer-plugins-base1.0-dev \
      libdbus-1-dev
    success "Build dependencies installed"

    info "Running: cargo build --release"
    echo ""
    cd "$SCRIPT_DIR"
    cargo build --release \
      || die "cargo build --release failed. Check the error output above."

    NATIVE_BINARY="$SCRIPT_DIR/target/release/nicocast"
    success "Binary ready: $NATIVE_BINARY  ($(du -sh "$NATIVE_BINARY" | cut -f1))"
  fi

  # ===========================================================================
  # Native Step 2/5 — Install binary and config
  # ===========================================================================
  step "Step 2/5 — Installing binary and config"

  $SUDO mkdir -p /etc/nicocast /usr/local/bin
  $SUDO cp "$NATIVE_BINARY" /usr/local/bin/nicocast
  $SUDO chmod +x /usr/local/bin/nicocast
  $SUDO cp "$SCRIPT_DIR/config.toml" /etc/nicocast/config.toml

  success "Binary installed at /usr/local/bin/nicocast"
  success "Config installed at /etc/nicocast/config.toml"

  # ===========================================================================
  # Native Step 3/6 — Install GStreamer runtime dependencies
  # ===========================================================================
  step "Step 3/6 — Installing GStreamer runtime dependencies"
  info "This requires internet access and may take a minute…"

  apt_get_update
  $SUDO apt-get install -y -qq \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-tools

  success "GStreamer dependencies installed"

  # ===========================================================================
  # Native Step 4/6 — systemd service + log rotation
  # ===========================================================================
  step "Step 4/6 — Creating systemd service and log rotation"

  $SUDO tee /etc/systemd/system/nicocast.service > /dev/null <<'SERVICE'
[Unit]
Description=NicoCast Miracast Sink
After=network.target network-online.target dbus.service wpa_supplicant.service
Wants=network-online.target dbus.service wpa_supplicant.service

[Service]
ExecStart=/usr/local/bin/nicocast
Restart=on-failure
RestartSec=5s
StandardOutput=journal
StandardError=journal
SyslogIdentifier=nicocast
Environment=XDG_RUNTIME_DIR=/run/user/1000

[Install]
WantedBy=multi-user.target
SERVICE

  $SUDO systemctl daemon-reload
  $SUDO systemctl enable nicocast

  $SUDO tee /etc/logrotate.d/nicocast > /dev/null <<'LOGROTATE'
/var/log/miracast_rs.log {
    daily
    rotate 7
    compress
    missingok
    notifempty
    create 640 root adm
    postrotate
        systemctl kill -s HUP nicocast.service 2>/dev/null || true
    endscript
}
LOGROTATE

  success "systemd service enabled (auto-starts at boot)"
  success "Log rotation configured (/etc/logrotate.d/nicocast)"

  # ===========================================================================
  # Native Step 5/6 — Status monitor (HDMI framebuffer)
  # ===========================================================================
  step "Step 5/6 — Installing NicoCast status monitor"
  info "Displays Bereit / Verbinde… / Streaming on the HDMI output via fbi."

  # Install Pillow (PIL) and fbi
  apt_get_update
  $SUDO apt-get install -y -qq python3-pil fbi

  # Copy the status monitor script
  $SUDO cp "$SCRIPT_DIR/status_monitor/nicocast_status.py" /usr/local/bin/nicocast_status.py
  $SUDO chmod +x /usr/local/bin/nicocast_status.py

  # Install the systemd unit
  $SUDO cp "$SCRIPT_DIR/status_monitor/nicocast-status.service" \
    /etc/systemd/system/nicocast-status.service
  $SUDO systemctl daemon-reload
  $SUDO systemctl enable nicocast-status

  success "Status monitor installed at /usr/local/bin/nicocast_status.py"
  success "nicocast-status service enabled (auto-starts at boot)"

  # ===========================================================================
  # Native Step 6/6 — USB Ethernet Gadget
  # ===========================================================================
  step "Step 6/6 — Configuring USB Ethernet Gadget (usb0)"
  info "Enables persistent SSH/log access on 192.168.7.2, independent of WiFi."

  NATIVE_GADGET_CHANGED=0

  if [[ -f /boot/firmware/config.txt ]]; then
    CONFIG_TXT="/boot/firmware/config.txt"
  else
    CONFIG_TXT="/boot/config.txt"
  fi

  if ! grep -q "dtoverlay=dwc2" "$CONFIG_TXT" 2>/dev/null; then
    echo "dtoverlay=dwc2,dr_mode=peripheral" | $SUDO tee -a "$CONFIG_TXT" > /dev/null
    NATIVE_GADGET_CHANGED=1
  fi

  if [[ -f /boot/firmware/cmdline.txt ]]; then
    CMDLINE="/boot/firmware/cmdline.txt"
  else
    CMDLINE="/boot/cmdline.txt"
  fi

  if ! grep -q "g_ether" "$CMDLINE" 2>/dev/null; then
    $SUDO sed -i '1 s/$/ modules-load=dwc2,g_ether/' "$CMDLINE"
    NATIVE_GADGET_CHANGED=1
  fi

  if ! nmcli connection show "USB Gadget" &>/dev/null 2>&1; then
    $SUDO nmcli connection add \
      type ethernet \
      ifname usb0 \
      con-name "USB Gadget" \
      ipv4.method manual \
      ipv4.addresses 192.168.7.2/24 \
      connection.autoconnect yes 2>/dev/null || true
  fi

  if [[ "$NATIVE_GADGET_CHANGED" == "1" ]]; then
    success "USB Gadget kernel modules configured (reboot required)"
    NATIVE_REBOOT_NEEDED=true
  else
    success "USB Gadget already configured"
    NATIVE_REBOOT_NEEDED=false
  fi

  # ===========================================================================
  # Done (Native mode)
  # ===========================================================================
  echo ""
  echo -e "${BOLD}${GREEN}  ✓ Native setup complete!${NC}"
  echo    "  ─────────────────────────────────────────────────────"
  echo ""

  if $NATIVE_REBOOT_NEEDED; then
    echo -e "  ${BOLD}Next step: reboot the Pi${NC} to activate the USB Ethernet Gadget."
    echo ""
    echo "    sudo reboot"
    echo ""
  fi

  echo -e "  ${BOLD}Start nicocast:${NC}"
  echo ""
  echo "    sudo systemctl start nicocast"
  echo "    sudo systemctl start nicocast-status"
  echo ""

  echo -e "  ${BOLD}Follow live logs:${NC}"
  echo ""
  echo "    sudo journalctl -u nicocast -f"
  echo "    sudo journalctl -u nicocast-status -f"
  echo ""
  echo "  See README.md for the full configuration reference and troubleshooting guide."
  echo ""
  exit 0
fi

# =============================================================================
# ── DOCKER BUILD MODE (default) ───────────────────────────────────────────────
# =============================================================================

# =============================================================================
# Step 1 — Prerequisites
# =============================================================================
step "Step 1/7 — Checking prerequisites"

if ! command -v docker &>/dev/null; then
  die "Docker is not installed or not in PATH.\n       Install it from: https://docs.docker.com/get-docker/"
fi
if ! docker info &>/dev/null 2>&1; then
  die "Docker daemon is not running or not accessible.\n       Start Docker Desktop, or run: sudo systemctl start docker"
fi
success "Docker OK  ($(docker --version | head -1))"

if ! command -v ssh &>/dev/null || ! command -v scp &>/dev/null; then
  die "ssh / scp not found. Install an SSH client (openssh-client)."
fi
success "ssh / scp OK"

# =============================================================================
# Step 2 — Build the binary via Docker
# =============================================================================
step "Step 2/7 — Building nicocast for aarch64 via Docker"
info "Docker layer caching makes subsequent builds much faster."
echo ""

cd "$SCRIPT_DIR"
docker build --platform linux/amd64 -t nicocast:latest . \
  || die "Docker build failed. Check the Dockerfile and try again."

info "Extracting aarch64 binary from image…"
CONTAINER_ID=$(docker create nicocast:latest)
docker cp "${CONTAINER_ID}:/usr/local/bin/nicocast" "$BINARY_LOCAL"
docker rm "${CONTAINER_ID}" >/dev/null

success "Binary ready: $BINARY_LOCAL  ($(du -sh "$BINARY_LOCAL" | cut -f1))"

# =============================================================================
# Step 3 — Verify SSH connection to Pi
# =============================================================================
step "Step 3/7 — Connecting to Raspberry Pi"
info "Trying SSH to ${PI_HOST}…"

if ! ssh -o ConnectTimeout=15 "$PI_HOST" "echo 'SSH OK'" 2>/dev/null; then
  echo ""
  error "Cannot reach ${PI_HOST}."
  echo ""
  echo "  Troubleshooting checklist:"
  echo "    • Is the Pi powered on and booted?"
  echo "    • Is it on the same network as this machine?"
  echo "    • Try: ping $(echo "$PI_HOST" | sed 's/.*@//')"
  echo "    • Ensure SSH is enabled on the Pi (raspi-config → Interface Options → SSH)"
  echo "    • If connecting over USB (usb0), did you assign 192.168.7.1/24 to the host"
  echo "      USB interface first? (Step 3 in the USB Gadget setup)"
  exit 1
fi
success "SSH connection OK"

# =============================================================================
# Step 4 — Deploy binary and config
# =============================================================================
step "Step 4/7 — Deploying binary and config to Pi"

scp -q "$BINARY_LOCAL" "${PI_HOST}:/tmp/nicocast"
scp -q "$SCRIPT_DIR/config.toml" "${PI_HOST}:/tmp/nicocast_config.toml"

ssh "$PI_HOST" bash <<'REMOTE'
set -euo pipefail
sudo mkdir -p /etc/nicocast /usr/local/bin
sudo mv /tmp/nicocast /usr/local/bin/nicocast
sudo chmod +x /usr/local/bin/nicocast
sudo mv /tmp/nicocast_config.toml /etc/nicocast/config.toml
REMOTE

success "Binary installed at /usr/local/bin/nicocast"
success "Config installed at /etc/nicocast/config.toml"

# =============================================================================
# Step 5 — Install GStreamer runtime dependencies
# =============================================================================
step "Step 5/8 — Installing GStreamer runtime dependencies"
info "This requires internet access on the Pi and may take a minute…"

ssh "$PI_HOST" bash <<'REMOTE'
set -euo pipefail
attempt=1; delay=10
while [[ $attempt -le 3 ]]; do
  if sudo apt-get update -qq; then break; fi
  if [[ $attempt -lt 3 ]]; then
    echo "  ⚠  apt-get update failed (attempt $attempt/3) — retrying in ${delay}s…" >&2
    sleep "$delay"; delay=$(( delay * 2 ))
  else
    echo "  ✗  apt-get update failed after 3 attempts." >&2; exit 1
  fi
  (( attempt++ ))
done
sudo apt-get install -y -qq \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-tools \
  python3-pil \
  fbi
REMOTE

success "GStreamer dependencies installed"

# =============================================================================
# Step 6 — systemd service + log rotation
# =============================================================================
step "Step 6/8 — Creating systemd service and log rotation"

ssh "$PI_HOST" bash <<'REMOTE'
set -euo pipefail

# ── systemd unit file ────────────────────────────────────────────────────────
sudo tee /etc/systemd/system/nicocast.service > /dev/null <<'SERVICE'
[Unit]
Description=NicoCast Miracast Sink
After=network.target network-online.target dbus.service wpa_supplicant.service
Wants=network-online.target dbus.service wpa_supplicant.service

[Service]
ExecStart=/usr/local/bin/nicocast
Restart=on-failure
RestartSec=5s
StandardOutput=journal
StandardError=journal
SyslogIdentifier=nicocast
Environment=XDG_RUNTIME_DIR=/run/user/1000

[Install]
WantedBy=multi-user.target
SERVICE

sudo systemctl daemon-reload
sudo systemctl enable nicocast

# ── log rotation ─────────────────────────────────────────────────────────────
sudo tee /etc/logrotate.d/nicocast > /dev/null <<'LOGROTATE'
/var/log/miracast_rs.log {
    daily
    rotate 7
    compress
    missingok
    notifempty
    create 640 root adm
    postrotate
        systemctl kill -s HUP nicocast.service 2>/dev/null || true
    endscript
}
LOGROTATE
REMOTE

success "systemd service enabled (auto-starts at boot)"
success "Log rotation configured (/etc/logrotate.d/nicocast)"

# =============================================================================
# Step 7 — Status monitor (HDMI framebuffer)
# =============================================================================
step "Step 7/8 — Deploying NicoCast status monitor"
info "Displays Bereit / Verbinde… / Streaming on the HDMI output via fbi."

scp -q "$SCRIPT_DIR/status_monitor/nicocast_status.py" "${PI_HOST}:/tmp/nicocast_status.py"
scp -q "$SCRIPT_DIR/status_monitor/nicocast-status.service" \
  "${PI_HOST}:/tmp/nicocast-status.service"

ssh "$PI_HOST" bash <<'REMOTE'
set -euo pipefail
sudo mv /tmp/nicocast_status.py /usr/local/bin/nicocast_status.py
sudo chmod +x /usr/local/bin/nicocast_status.py
sudo mv /tmp/nicocast-status.service /etc/systemd/system/nicocast-status.service
sudo systemctl daemon-reload
sudo systemctl enable nicocast-status
REMOTE

success "Status monitor installed at /usr/local/bin/nicocast_status.py"
success "nicocast-status service enabled (auto-starts at boot)"

# =============================================================================
# Step 8 — USB Ethernet Gadget
# =============================================================================
step "Step 8/8 — Configuring USB Ethernet Gadget (usb0)"
info "Enables persistent SSH/log access on 192.168.7.2, independent of WiFi."

# Capture what changed on the Pi so we know if a reboot is needed
GADGET_STATUS=$(ssh "$PI_HOST" bash <<'REMOTE'
set -euo pipefail
CHANGED=0

# ── /boot/firmware/config.txt ────────────────────────────────────────────────
if [[ -f /boot/firmware/config.txt ]]; then
  CONFIG_TXT="/boot/firmware/config.txt"
else
  CONFIG_TXT="/boot/config.txt"   # Bullseye fallback
fi

if ! grep -q "dtoverlay=dwc2" "$CONFIG_TXT" 2>/dev/null; then
  echo "dtoverlay=dwc2,dr_mode=peripheral" | sudo tee -a "$CONFIG_TXT" > /dev/null
  CHANGED=1
fi

# ── /boot/firmware/cmdline.txt ───────────────────────────────────────────────
if [[ -f /boot/firmware/cmdline.txt ]]; then
  CMDLINE="/boot/firmware/cmdline.txt"
else
  CMDLINE="/boot/cmdline.txt"
fi

if ! grep -q "g_ether" "$CMDLINE" 2>/dev/null; then
  sudo sed -i '1 s/$/ modules-load=dwc2,g_ether/' "$CMDLINE"
  CHANGED=1
fi

# ── Static IP for usb0 via NetworkManager (non-fatal — usb0 may not exist yet)
if ! nmcli connection show "USB Gadget" &>/dev/null 2>&1; then
  sudo nmcli connection add \
    type ethernet \
    ifname usb0 \
    con-name "USB Gadget" \
    ipv4.method manual \
    ipv4.addresses 192.168.7.2/24 \
    connection.autoconnect yes 2>/dev/null || true
fi

echo "$CHANGED"
REMOTE
)

if [[ "$GADGET_STATUS" == "1" ]]; then
  success "USB Gadget kernel modules configured (reboot required)"
  REBOOT_NEEDED=true
else
  success "USB Gadget already configured"
  REBOOT_NEEDED=false
fi

# =============================================================================
# Done — Print next steps
# =============================================================================
echo ""
echo -e "${BOLD}${GREEN}  ✓ Setup complete!${NC}"
echo    "  ─────────────────────────────────────────────────────"
echo ""

if $REBOOT_NEEDED; then
  echo -e "  ${BOLD}Next step: reboot the Pi${NC} to activate the USB Ethernet Gadget."
  echo ""
  echo "    ssh ${PI_HOST} 'sudo reboot'"
  echo ""
  echo "  After reboot, connect the Pi's OTG USB port to your laptop and"
  echo "  continue with the steps below."
  echo ""
fi

echo -e "  ${BOLD}Configure your laptop's USB interface (once per machine):${NC}"
echo ""
echo "    Linux:"
echo "      sudo ip addr add 192.168.7.1/24 dev usb0"
echo "      sudo ip link set usb0 up"
echo ""
echo "    macOS:"
echo "      System Preferences → Network → USB Ethernet (CDC ECM)"
echo "      Configure Manually: IP 192.168.7.1, Mask 255.255.255.0"
echo ""
echo "    Windows:"
echo "      Network Settings → RNDIS adapter → IPv4 Properties"
echo "      Use the following IP: 192.168.7.1, Subnet: 255.255.255.0"
echo ""

echo -e "  ${BOLD}Start nicocast:${NC}"
echo ""
echo "    ssh pi@192.168.7.2 'sudo systemctl start nicocast'"
echo "    ssh pi@192.168.7.2 'sudo systemctl start nicocast-status'"
echo ""

echo -e "  ${BOLD}Follow live logs:${NC}"
echo ""
echo "    ./setup.sh --logs pi@192.168.7.2"
echo "    # or directly via SSH:"
echo "    ssh pi@192.168.7.2 'sudo journalctl -u nicocast -f'"
echo "    ssh pi@192.168.7.2 'sudo journalctl -u nicocast-status -f'"
echo ""
echo "  See README.md for the full configuration reference and troubleshooting guide."
echo ""
