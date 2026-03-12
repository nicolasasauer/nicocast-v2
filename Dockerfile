# ╔══════════════════════════════════════════════════════════════════════════╗
# ║  nicocast-v2 — Cross-compilation Dockerfile                             ║
# ║  Target : Raspberry Pi Zero 2W (BCM2710A1, aarch64 / Cortex-A53)       ║
# ║  Host   : any x86_64 Linux machine or CI runner                         ║
# ║                                                                          ║
# ║  Build command (run from repo root):                                     ║
# ║    docker build --platform linux/amd64 -t nicocast:latest .              ║
# ║                                                                          ║
# ║  The resulting binary is extracted from the image and copied to the Pi: ║
# ║    docker create --name nc nicocast:latest                               ║
# ║    docker cp nc:/usr/local/bin/nicocast ./nicocast-aarch64               ║
# ║    docker rm nc                                                          ║
# ║    scp nicocast-aarch64 pi@192.168.7.2:/usr/local/bin/nicocast          ║
# ╚══════════════════════════════════════════════════════════════════════════╝

# ── Stage 1: Cross-compilation builder ────────────────────────────────────────
#
# Base: Debian Bookworm (same base as Raspberry Pi OS Bookworm) so the libc
# and system-library ABIs match exactly.
FROM debian:bookworm-slim AS builder

# Enable the arm64 package architecture so we can install arm64 development
# libraries (headers + .so stubs) alongside the native x86_64 toolchain.
RUN dpkg --add-architecture arm64

RUN apt-get update && apt-get install -y --no-install-recommends \
    # ── Native C toolchain (required for Rust build scripts / proc-macros) ── #
    # Rust build scripts and proc-macro crates (e.g. quote, proc-macro2, libc)
    # are always compiled for the HOST architecture, even during a cross-build.
    # They invoke the native linker `cc`; without gcc/libc6-dev cargo fails:
    #   "error: linker `cc` not found"
    #   "rust-lld: error: cannot open crtn.o: No such file or directory"
    gcc \
    libc6-dev \
    # ── Cross-compilation toolchain ──────────────────────────────────────── #
    gcc-aarch64-linux-gnu \
    g++-aarch64-linux-gnu \
    binutils-aarch64-linux-gnu \
    # ── pkg-config (host tool, queries arm64 .pc files via env vars) ──────── #
    pkg-config \
    # ── GStreamer development headers + link stubs (arm64) ────────────────── #
    # gstreamer-1.0          → core library used by the `gstreamer` crate
    # gstreamer-plugins-base → provides gst-plugins-base-1.0.pc (needed by
    #                          gstreamer-rs even when not using the extra API)
    libgstreamer1.0-dev:arm64 \
    libgstreamer-plugins-base1.0-dev:arm64 \
    # glib-2.0 and gobject-2.0 are transitive deps of GStreamer
    libglib2.0-dev:arm64 \
    # ── Binary introspection (used to verify the cross-compiled ELF) ──────── #
    file \
    # ── Rust installer prerequisites ──────────────────────────────────────── #
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# ── Install Rust via rustup ───────────────────────────────────────────────────
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y \
        --no-modify-path \
        --profile minimal \
        --default-toolchain stable \
        --target aarch64-unknown-linux-gnu

# ── Cross-compilation environment ────────────────────────────────────────────
#
# CARGO_TARGET_*_LINKER  — use the aarch64 GCC cross-linker.
# CC / CXX / AR          — expose the cross-tools to any C build scripts.
# PKG_CONFIG_ALLOW_CROSS — let pkg-config run during a cross-build.
# PKG_CONFIG_LIBDIR      — point pkg-config *exclusively* at the arm64 .pc
#                          files installed by the :arm64 packages above.
#                          Overriding LIBDIR (not PATH) prevents pkg-config
#                          from accidentally picking up x86_64 .pc files.
# PKG_CONFIG_SYSROOT_DIR — empty: Debian multiarch installs headers under
#                          /usr/include (architecture-independent) and libs
#                          under /usr/lib/aarch64-linux-gnu, so no sysroot
#                          prefix is needed.
# RUSTFLAGS              — optimise for the Cortex-A53 core on BCM2710A1.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_LIBDIR=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
    PKG_CONFIG_SYSROOT_DIR="" \
    RUSTFLAGS="-C target-cpu=cortex-a53"

WORKDIR /build

# ── Dependency pre-fetch layer (Docker cache optimisation) ───────────────────
#
# Copy only the manifest; compile a stub main.rs so cargo downloads and
# compiles all dependencies once.  When source files change, this layer is
# reused and only the final `cargo build` step re-runs.
COPY Cargo.toml ./
# Cargo.lock is optional on the first build (cargo will generate it).
# The glob Cargo.loc[k] matches only when the file is present and does not
# fail the build if it is absent.
COPY Cargo.loc[k] ./

RUN mkdir -p src && printf 'fn main(){}' > src/main.rs \
    && cargo build --release --target aarch64-unknown-linux-gnu \
    # Remove only the stub artefacts; keep the compiled deps in the cache.
    && rm -f target/aarch64-unknown-linux-gnu/release/nicocast \
             target/aarch64-unknown-linux-gnu/release/deps/nicocast-* \
    && rm -rf src

# ── Build the real application ────────────────────────────────────────────────
COPY src/      ./src/
COPY config.toml ./

# `touch` forces cargo to re-link even if timestamps look up-to-date.
RUN touch src/main.rs \
    && cargo build --release --target aarch64-unknown-linux-gnu

# Confirm the binary is an aarch64 ELF (catches misconfigured cross-linkers).
RUN file target/aarch64-unknown-linux-gnu/release/nicocast \
    | grep -q "ELF 64-bit LSB.*aarch64" \
    || { echo "ERROR: binary is not aarch64!"; exit 1; }

# ── Stage 2: Runtime image ────────────────────────────────────────────────────
#
# This image is intended to run *on* the Raspberry Pi Zero 2W.
# It installs only the GStreamer runtime plugins needed for the pipeline:
#
#   udpsrc        — gstreamer1.0-plugins-good  (gio/udp plugin)
#   tsdemux       — gstreamer1.0-plugins-bad   (mpegtsdemux plugin)
#   h264parse     — gstreamer1.0-plugins-bad   (videoparsersbad plugin)
#   v4l2h264dec   — gstreamer1.0-plugins-good  (video4linux2 plugin)
#   autovideosink — gstreamer1.0-plugins-base  (playback plugin)
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    gstreamer1.0-tools \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    # dbus-daemon must be running on the Pi; this ensures the client libs exist
    dbus \
    libdbus-1-3 \
    # iproute2 for optional interface diagnostics
    iproute2 \
    && rm -rf /var/lib/apt/lists/*

# Create the log directory the application writes to.
RUN mkdir -p /var/log /etc/nicocast

# Copy the cross-compiled binary and the default configuration.
COPY --from=builder \
    /build/target/aarch64-unknown-linux-gnu/release/nicocast \
    /usr/local/bin/nicocast

COPY --from=builder /build/config.toml /etc/nicocast/config.toml

ENTRYPOINT ["/usr/local/bin/nicocast"]
