//! GStreamer hardware-accelerated video pipeline for Miracast on RPi Zero 2W.
//!
//! The pipeline receives an MPEG-TS stream over UDP and decodes H.264.
//!
//! On Raspberry Pi hardware the BCM2710A1 V4L2 codec is used:
//!
//! ```text
//! udpsrc port=<rtp_port>
//!   → tsdemux          (dynamic pads — H.264 ES linked in pad-added handler)
//!   → h264parse
//!   → v4l2h264dec      ← BCM2710A1 hardware decode (preferred)
//!   → autovideosink
//! ```
//!
//! On machines without `v4l2h264dec` (e.g. a development laptop) the pipeline
//! automatically falls back to the software decoder:
//!
//! ```text
//! udpsrc → tsdemux → h264parse → avdec_h264 → autovideosink
//! ```
//!
//! # gstreamer-rs 0.25 compatibility notes
//!
//! * `Bin::add_many()` accepts `&[&impl IsA<Element>]`.
//! * Static element linking uses `src.link(dest)` (method call on trait object).
//! * In a `connect_pad_added` handler, `element.sync_state_with_parent()` must
//!   be called after the dynamic link is established so the new sub-pipeline
//!   transitions to PLAYING along with the bin.
//! * `Bus::timed_pop(timeout)` semantics were corrected in 0.25.0 — a
//!   `ClockTime` timeout is now honoured consistently.

use anyhow::{bail, Context, Result};
use gstreamer::{
    prelude::*,
    ClockTime, Element, ElementFactory, MessageView, Pipeline, State,
};
use std::sync::{atomic::Ordering, Arc};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::health::{AppState, STATE_IDLE, STATE_PLAYING};

// ─── drop-safe pipeline guard ─────────────────────────────────────────────────

/// Wraps a [`Pipeline`] and ensures `set_state(Null)` is called when dropped,
/// even if the owning task is cancelled or aborted.  This prevents GStreamer
/// warnings about elements being disposed while still in PLAYING/PAUSED state.
struct PipelineGuard(Pipeline);

impl Drop for PipelineGuard {
    fn drop(&mut self) {
        let _ = self.0.set_state(State::Null);
    }
}

/// Build and run the GStreamer pipeline.
///
/// Blocks inside the async executor (via `yield_now`) until the pipeline
/// reaches EOS, encounters an unrecoverable error, or the process exits.
/// Updates `state.video` with [`STATE_PLAYING`] when streaming begins and
/// resets it to [`STATE_IDLE`] when the pipeline stops.
pub async fn run_pipeline(cfg: &Config, state: Arc<AppState>) -> Result<()> {
    let pipeline = build_pipeline(cfg).context("building GStreamer pipeline")?;

    info!(
        "GStreamer pipeline built — waiting for UDP MPEG-TS on port {}",
        cfg.rtp_port
    );

    pipeline
        .set_state(State::Playing)
        .context("setting pipeline to PLAYING")?;
    info!("GStreamer pipeline PLAYING");
    state.video.store(STATE_PLAYING, Ordering::Relaxed);

    // PipelineGuard ensures set_state(Null) is called even if this task is
    // aborted or cancelled before the loop reaches its normal exit.
    let guard = PipelineGuard(pipeline);
    let bus = guard.0.bus().context("getting pipeline bus")?;

    let exit_reason = loop {
        // Poll with a 500 ms timeout; yield between polls so tokio can
        // service other tasks.  Bus::timed_pop semantics were fixed in
        // gstreamer-rs 0.25.0.
        match bus.timed_pop(ClockTime::from_mseconds(500)) {
            Some(msg) => match msg.view() {
                MessageView::Eos(_) => {
                    info!("GStreamer bus: End-of-Stream received — pipeline loop exiting");
                    break "EOS";
                }
                MessageView::Error(err) => {
                    let src = err
                        .src()
                        .map(|s| s.path_string().to_string())
                        .unwrap_or_else(|| "<unknown>".into());
                    let gst_err = err.error();
                    let dbg = err.debug().unwrap_or_default();
                    error!(
                        "GStreamer bus: Error from '{src}' — {gst_err} (debug: {dbg}); \
                         pipeline loop exiting"
                    );
                    // Explicitly transition to NULL before the guard drops so
                    // the error message is the last thing logged.
                    let _ = guard.0.set_state(State::Null);
                    bail!("GStreamer pipeline error: {gst_err}");
                }
                MessageView::Warning(w) => {
                    let dbg = w.debug().unwrap_or_default();
                    warn!("GStreamer warning: {} ({dbg})", w.error());
                }
                MessageView::StateChanged(sc) => {
                    if sc
                        .src()
                        .map(|s| s == guard.0.upcast_ref::<gstreamer::Object>())
                        .unwrap_or(false)
                    {
                        debug!(
                            "Pipeline state: {:?} → {:?}",
                            sc.old(),
                            sc.current()
                        );
                    }
                }
                _ => {}
            },
            None => {
                // timed_pop already waited up to 500 ms with no messages.
                // Sleep briefly before the next poll so this task does not
                // monopolise its tokio worker thread.
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }
        }
    };

    // Transition to NULL; the guard's Drop will be a no-op after this.
    guard
        .0
        .set_state(State::Null)
        .context("stopping GStreamer pipeline")?;
    state.video.store(STATE_IDLE, Ordering::Relaxed);
    info!("GStreamer pipeline stopped (exit reason: {exit_reason})");
    Ok(())
}

// ─── pipeline construction ────────────────────────────────────────────────────

/// Construct the element graph:
///
/// `udpsrc → tsdemux ⟿ h264parse → <h264dec> → autovideosink`
///
/// The `<h264dec>` is `v4l2h264dec` on Raspberry Pi hardware and
/// `avdec_h264` (libav software) on any other platform.
/// The `⟿` arrow denotes a dynamic pad connection wired in the
/// `connect_pad_added` handler.
fn build_pipeline(cfg: &Config) -> Result<Pipeline> {
    let pipeline = Pipeline::new();

    // ── create elements ───────────────────────────────────────────────────────
    let udpsrc    = make_element("udpsrc",       "src")?;
    let tsdemux   = make_element("tsdemux",      "demux")?;
    let h264parse = make_element("h264parse",    "parse")?;
    let h264dec   = make_h264_decoder()?;
    let videosink = make_element("autovideosink","sink")?;

    // ── configure element properties ─────────────────────────────────────────
    // udpsrc: receive MPEG-TS datagrams on the configured RTP port
    udpsrc.set_property("port", cfg.rtp_port as i32);
    udpsrc.set_property("caps", &mpeg_ts_caps());

    // autovideosink: no A/V sync needed (audio not carried in this pipeline)
    videosink.set_property("sync", false);

    // ── add elements to the pipeline (gstreamer-rs 0.25: pass a slice) ───────
    pipeline
        .add_many(&[&udpsrc, &tsdemux, &h264parse, &h264dec, &videosink])
        .context("adding elements to pipeline")?;

    // ── static links ──────────────────────────────────────────────────────────
    // udpsrc  →  tsdemux   (static; tsdemux exposes dynamic pads below)
    udpsrc.link(&tsdemux).context("linking udpsrc → tsdemux")?;
    // h264parse → h264dec → autovideosink
    h264parse.link(&h264dec).context("linking h264parse → h264dec")?;
    h264dec.link(&videosink).context("linking h264dec → autovideosink")?;

    // ── dynamic pad: tsdemux → h264parse ─────────────────────────────────────
    // tsdemux adds a pad for each elementary stream it finds in the MPEG-TS.
    // We hook into `pad-added` to link the first H.264 ES to h264parse.
    //
    // gstreamer-rs 0.25: after linking the pad, call sync_state_with_parent()
    // on the downstream element so it transitions to PLAYING with the bin.
    let h264parse_weak = h264parse.downgrade();
    tsdemux.connect_pad_added(move |_demux, src_pad| {
        let pad_name = src_pad.name();
        debug!("tsdemux: new pad '{pad_name}'");

        // Determine caps — prefer already-negotiated caps, fall back to a query.
        let caps = src_pad
            .current_caps()
            .unwrap_or_else(|| src_pad.query_caps(None));

        let is_h264 = caps
            .iter()
            .any(|s| s.name().as_str() == "video/x-h264");

        if !is_h264 {
            debug!("tsdemux: pad '{pad_name}' is not H.264, skipping");
            return;
        }

        let h264parse = match h264parse_weak.upgrade() {
            Some(e) => e,
            None => {
                error!("h264parse element was dropped before pad-added fired");
                return;
            }
        };

        let sink_pad = match h264parse.static_pad("sink") {
            Some(p) => p,
            None => {
                error!("h264parse has no static sink pad");
                return;
            }
        };

        if sink_pad.is_linked() {
            debug!("h264parse sink already linked; ignoring pad '{pad_name}'");
            return;
        }

        if let Err(e) = src_pad.link(&sink_pad) {
            error!("Failed to link tsdemux::{pad_name} → h264parse::sink: {e:?}");
            return;
        }
        info!("Linked tsdemux::{pad_name} → h264parse::sink");

        // Required in gstreamer-rs 0.25 for dynamically linked elements:
        // bring the element up to the current state of the parent bin.
        if let Err(e) = h264parse.sync_state_with_parent() {
            error!("sync_state_with_parent failed for h264parse: {e}");
        }
    });

    Ok(pipeline)
}

/// Build GStreamer `Caps` for MPEG-TS over UDP (188-byte packets).
fn mpeg_ts_caps() -> gstreamer::Caps {
    gstreamer::Caps::builder("video/mpegts")
        .field("systemstream", true)
        .field("packetsize", 188i32)
        .build()
}

/// Select the best available H.264 decoder element.
///
/// Prefers `v4l2h264dec` (Raspberry Pi V4L2 hardware codec).  Falls back to
/// `avdec_h264` (software, from `gstreamer1.0-libav`) on any machine that
/// does not have the V4L2 codec driver — useful for development and testing
/// on a regular laptop without Raspberry Pi hardware.
fn make_h264_decoder() -> Result<Element> {
    if let Ok(el) = make_element("v4l2h264dec", "decode") {
        info!("H.264 decoder: using hardware v4l2h264dec");
        return Ok(el);
    }
    info!(
        "v4l2h264dec not available — falling back to software decoder avdec_h264. \
         Install gstreamer1.0-libav on the target if avdec_h264 is also missing."
    );
    make_element("avdec_h264", "decode")
}

/// Convenience wrapper: create an element by factory name.
/// Returns a descriptive error when the plugin is missing.
fn make_element(factory: &str, name: &str) -> Result<Element> {
    ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| {
            format!(
                "creating GStreamer element '{factory}' — \
                 is the '{factory}' plugin installed on this device?"
            )
        })
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn init_gst() {
        let _ = gstreamer::init();
    }

    #[test]
    fn mpeg_ts_caps_correct() {
        init_gst();
        let caps = mpeg_ts_caps();
        let s = caps.structure(0).expect("caps must have at least one structure");
        assert_eq!(s.name().as_str(), "video/mpegts");
        assert_eq!(s.get::<bool>("systemstream").unwrap(), true);
        assert_eq!(s.get::<i32>("packetsize").unwrap(), 188);
    }

    #[test]
    fn make_element_unknown_returns_error() {
        init_gst();
        assert!(make_element("nonexistent_plugin_xyzzy", "test").is_err());
    }

    #[test]
    fn make_element_udpsrc_succeeds() {
        init_gst();
        assert!(make_element("udpsrc", "test_src").is_ok());
    }

    /// Verifies that `make_h264_decoder` returns *some* element (hardware or
    /// software) without panicking, as long as at least one H.264 decoder
    /// plugin is installed.  This test is skipped (passes vacuously) when
    /// neither decoder is available in the test environment.
    #[test]
    fn make_h264_decoder_returns_some_decoder() {
        init_gst();
        // If neither v4l2h264dec nor avdec_h264 is installed, skip gracefully.
        let hw = ElementFactory::find("v4l2h264dec").is_some();
        let sw = ElementFactory::find("avdec_h264").is_some();
        if !hw && !sw {
            eprintln!("Skipping: neither v4l2h264dec nor avdec_h264 available");
            return;
        }
        assert!(
            make_h264_decoder().is_ok(),
            "make_h264_decoder must succeed when at least one decoder is installed"
        );
    }
}
