#!/usr/bin/env bash
# =============================================================================
# setup.sh — nicocast-v2 first-run setup
#
# What this script does:
#   1. Builds the nicocast binary via Docker (cross-compilation for aarch64)
#   2. Copies the binary and config to the Raspberry Pi over SSH
#   3. Installs GStreamer runtime dependencies on the Pi
#   4. Creates and enables the nicocast systemd service
#   5. Configures log rotation
#   6. Enables the USB Ethernet Gadget (usb0) for persistent log access
#
# Requirements (on your machine):
#   - Docker (running, accessible without sudo — or with sudo)
#   - ssh / scp
#   - The Raspberry Pi must be reachable over the network (WiFi or Ethernet)
#   - The Pi user must have passwordless sudo (default on Raspberry Pi OS)
#
# Usage:
#   ./setup.sh                      # prompts for Pi SSH address
#   ./setup.sh pi@192.168.1.42      # use a specific address
#   ./setup.sh --logs pi@192.168.7.2  # connect and follow live logs only
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY_LOCAL="$SCRIPT_DIR/nicocast-aarch64"

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

# ── Banner ────────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}  nicocast-v2 — Miracast Sink for Raspberry Pi Zero 2W${NC}"
echo    "  First-Run Setup Script"
echo    "  ─────────────────────────────────────────────────────"

# ── Parse arguments ───────────────────────────────────────────────────────────
LOGS_ONLY=false
PI_HOST=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --logs)
      LOGS_ONLY=true
      shift
      ;;
    -*)
      die "Unknown option: $1  (usage: ./setup.sh [--logs] [pi@<address>])"
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

# ── Prompt for Pi address if not supplied ─────────────────────────────────────
if [[ -z "$PI_HOST" ]]; then
  read -rp "  Enter Raspberry Pi SSH address [pi@raspberrypi.local]: " PI_HOST
  PI_HOST="${PI_HOST:-pi@raspberrypi.local}"
fi
info "Target Pi: ${BOLD}${PI_HOST}${NC}"

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
step "Step 5/7 — Installing GStreamer runtime dependencies"
info "This requires internet access on the Pi and may take a minute…"

ssh "$PI_HOST" bash <<'REMOTE'
set -euo pipefail
sudo apt-get update -qq
sudo apt-get install -y -qq \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-tools
REMOTE

success "GStreamer dependencies installed"

# =============================================================================
# Step 6 — systemd service + log rotation
# =============================================================================
step "Step 6/7 — Creating systemd service and log rotation"

ssh "$PI_HOST" bash <<'REMOTE'
set -euo pipefail

# ── systemd unit file ────────────────────────────────────────────────────────
sudo tee /etc/systemd/system/nicocast.service > /dev/null <<'SERVICE'
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
# Step 7 — USB Ethernet Gadget
# =============================================================================
step "Step 7/7 — Configuring USB Ethernet Gadget (usb0)"
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
echo ""

echo -e "  ${BOLD}Follow live logs:${NC}"
echo ""
echo "    ./setup.sh --logs pi@192.168.7.2"
echo "    # or directly via SSH:"
echo "    ssh pi@192.168.7.2 'sudo journalctl -u nicocast -f'"
echo ""
echo "  See README.md for the full configuration reference and troubleshooting guide."
echo ""
