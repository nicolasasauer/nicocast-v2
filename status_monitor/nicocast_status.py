#!/usr/bin/env python3
"""NicoCast Status Monitor — displays the current Miracast state on HDMI.

Monitors /var/log/miracast_rs.log, detects the following states, and renders
a 1920×1080 status image that is shown on the framebuffer via ``fbi``:

  Bereit       — P2P discovery active, waiting for a Miracast source
  Verbinde…    — RTSP session in progress (source connected, negotiating)
  Streaming    — GStreamer pipeline is playing the video stream

Run as a systemd service (see nicocast-status.service).
"""

import fcntl
import os
import re
import signal
import socket
import struct
import subprocess
import sys
import time
from typing import Generator, Optional, Union

from PIL import Image, ImageDraw, ImageFont

# ── configuration ──────────────────────────────────────────────────────────────

LOG_FILE = "/var/log/miracast_rs.log"
DEVICE_NAME = "NicoCast-Sink"
FRAMEBUFFER = "/dev/fb0"
TMP_IMAGE = "/tmp/nicocast_status.png"

IMG_WIDTH = 1920
IMG_HEIGHT = 1080

FONT_SIZE_STATUS = 120
FONT_SIZE_INFO = 48

# Colour scheme: dark navy background, coloured status text, subtle info text
BG_COLOUR = (20, 20, 40)
COLOUR_BEREIT = (80, 200, 80)       # green  — ready / idle
COLOUR_VERBINDE = (255, 200, 50)    # amber  — connecting
COLOUR_STREAMING = (50, 180, 255)   # blue   — active stream
COLOUR_INFO = (160, 160, 190)       # muted  — device name / IP

# ── status transition rules ────────────────────────────────────────────────────
# Each rule is (compiled_regex, new_status_label, status_colour).
# Rules are evaluated in order; the first match wins.
# Priority matters: GStreamer PLAYING must come before RTSP patterns so that
# a "PLAYING" line does not fall through to a lower-priority RTSP rule.

STATUS_RULES = [
    # Streaming: GStreamer pipeline reached PLAYING state
    (re.compile(r"GStreamer pipeline PLAYING"),
     "Streaming", COLOUR_STREAMING),

    # Streaming: EOS or teardown → fall back to Bereit
    (re.compile(r"GStreamer bus: End-of-Stream|GStreamer pipeline stopped"),
     "Bereit", COLOUR_BEREIT),

    # Verbinde: RTSP connection established (source is connecting)
    (re.compile(r"RTSP: new connection from"),
     "Verbinde\u2026", COLOUR_VERBINDE),

    # Bereit: RTSP session ended → back to waiting
    (re.compile(r"RTSP: TEARDOWN|RTSP: connection closed"),
     "Bereit", COLOUR_BEREIT),

    # Bereit: P2P discovery / manager active
    (re.compile(r"P2P manager active|P2P discovery started"),
     "Bereit", COLOUR_BEREIT),
]

# ── font helpers ───────────────────────────────────────────────────────────────

_FONT_CANDIDATES = [
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/truetype/freefont/FreeSansBold.ttf",
]


def _load_font(size: int) -> Union[ImageFont.FreeTypeFont, ImageFont.ImageFont]:
    """Return the first available TrueType bold font at *size*, or the PIL
    bitmap default (which ignores *size*) if no TTF file is found."""
    for path in _FONT_CANDIDATES:
        if os.path.exists(path):
            return ImageFont.truetype(path, size)
    return ImageFont.load_default()


# ── IP address helpers ─────────────────────────────────────────────────────────

_SIOCGIFADDR = 0x8915


def get_ip(iface: str) -> str:
    """Return the first IPv4 address on *iface*, or ``'N/A'`` on failure."""
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            packed = fcntl.ioctl(
                sock.fileno(),
                _SIOCGIFADDR,
                struct.pack("256s", iface[:15].encode()),
            )
            return socket.inet_ntoa(packed[20:24])
    except OSError:
        return "N/A"


# ── image renderer ─────────────────────────────────────────────────────────────

def render_frame(status: str, status_colour: tuple) -> Image.Image:
    """Generate a 1920×1080 status image for *status*."""
    img = Image.new("RGB", (IMG_WIDTH, IMG_HEIGHT), BG_COLOUR)
    draw = ImageDraw.Draw(img)

    ip_usb = get_ip("usb0")
    ip_wlan = get_ip("wlan0")

    font_status = _load_font(FONT_SIZE_STATUS)
    font_info = _load_font(FONT_SIZE_INFO)

    # ── status text — centred in the upper half ──────────────────────────────
    bbox_s = draw.textbbox((0, 0), status, font=font_status)
    sw, sh = bbox_s[2] - bbox_s[0], bbox_s[3] - bbox_s[1]
    draw.text(
        ((IMG_WIDTH - sw) // 2, (IMG_HEIGHT // 2 - sh) // 2),
        status,
        font=font_status,
        fill=status_colour,
    )

    # ── device name — centred just below the midline ─────────────────────────
    bbox_d = draw.textbbox((0, 0), DEVICE_NAME, font=font_info)
    dw, dh = bbox_d[2] - bbox_d[0], bbox_d[3] - bbox_d[1]
    device_y = IMG_HEIGHT // 2 + 40
    draw.text(
        ((IMG_WIDTH - dw) // 2, device_y),
        DEVICE_NAME,
        font=font_info,
        fill=COLOUR_INFO,
    )

    # ── IP addresses — one line below the device name ─────────────────────────
    ip_line = f"usb0: {ip_usb}    wlan0: {ip_wlan}"
    bbox_i = draw.textbbox((0, 0), ip_line, font=font_info)
    iw = bbox_i[2] - bbox_i[0]
    draw.text(
        ((IMG_WIDTH - iw) // 2, device_y + dh + 20),
        ip_line,
        font=font_info,
        fill=COLOUR_INFO,
    )

    return img


# ── framebuffer display ────────────────────────────────────────────────────────

_fbi_proc: Optional[subprocess.Popen] = None


def show_image(img: Image.Image) -> None:
    """Save *img* and display it on the framebuffer with ``fbi``."""
    global _fbi_proc

    img.save(TMP_IMAGE, "PNG")

    # Terminate any running fbi instance before launching a new one.
    if _fbi_proc is not None and _fbi_proc.poll() is None:
        _fbi_proc.terminate()
        try:
            _fbi_proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            _fbi_proc.kill()
            _fbi_proc.wait()

    # -T 1        : use virtual console 1
    # -d /dev/fb0 : target framebuffer device
    # --noverbose : suppress on-screen text from fbi itself
    # -a          : auto-fit image to screen
    # -1          : show image once (no slideshow loop)
    _fbi_proc = subprocess.Popen(
        ["fbi", "-T", "1", "-d", FRAMEBUFFER, "--noverbose", "-a", "-1", TMP_IMAGE],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


# ── log tail ───────────────────────────────────────────────────────────────────

def tail_log(path: str) -> Generator[str, None, None]:
    """Yield new lines from *path* as they are appended (``tail -f`` semantics).

    Blocks until the file exists, then seeks to EOF and yields each line as it
    arrives.  Never returns under normal operation.
    """
    while not os.path.exists(path):
        time.sleep(1)

    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        fh.seek(0, os.SEEK_END)
        while True:
            line = fh.readline()
            if line:
                yield line
            else:
                time.sleep(0.2)


# ── signal handlers ────────────────────────────────────────────────────────────

def _shutdown(_signum: int, _frame: object) -> None:
    global _fbi_proc
    if _fbi_proc is not None and _fbi_proc.poll() is None:
        _fbi_proc.terminate()
        try:
            _fbi_proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            _fbi_proc.kill()
    sys.exit(0)


# ── entry point ────────────────────────────────────────────────────────────────

def main() -> None:
    signal.signal(signal.SIGTERM, _shutdown)
    signal.signal(signal.SIGINT, _shutdown)

    # Show the initial "Bereit" screen immediately on startup.
    current_status = "Bereit"
    current_colour = COLOUR_BEREIT
    show_image(render_frame(current_status, current_colour))

    for line in tail_log(LOG_FILE):
        for pattern, label, colour in STATUS_RULES:
            if pattern.search(line):
                if label != current_status:
                    current_status = label
                    current_colour = colour
                    show_image(render_frame(current_status, current_colour))
                break


if __name__ == "__main__":
    main()
