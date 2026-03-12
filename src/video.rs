//! GStreamer hardware-accelerated video pipeline for Miracast on RPi Zero 2W.
//!
//! The pipeline receives an MPEG-TS stream over UDP and decodes H.264 using
//! the Raspberry Pi's V4L2 hardware decoder (`v4l2h264dec`):
//!
//! ```text
//! udpsrc port=<rtp_port>
//!   → tsdemux
//!   → h264parse
//!   → v4l2h264dec           ← hardware H.264 on BCM2710A1
//!   → autovideosink
//! ```
//!
//! The pipeline is started once and runs until the process exits or an
//! error is received on the GStreamer bus.

use anyhow::{bail, Context, Result};
use gstreamer::{
    prelude::*,
    ClockTime, Element, ElementFactory, MessageView, Pipeline, State,
};
use tracing::{debug, error, info, warn};

use crate::config::Config;

/// Build and run the GStreamer pipeline.
///
/// This function blocks (inside the async executor) until the pipeline
/// reaches EOS, encounters an error, or the process is interrupted.
pub async fn run_pipeline(cfg: &Config) -> Result<()> {
    let pipeline = build_pipeline(cfg).context("building GStreamer pipeline")?;

    info!(
        "GStreamer pipeline built — waiting for UDP MPEG-TS on port {}",
        cfg.rtp_port
    );

    pipeline
        .set_state(State::Playing)
        .context("setting pipeline to PLAYING")?;

    info!("GStreamer pipeline PLAYING");

    let bus = pipeline
        .bus()
        .context("getting pipeline bus")?;

    // Poll the bus in a blocking-friendly async loop
    loop {
        // `timed_pop` with a 500 ms timeout so we don't block forever
        match bus.timed_pop(ClockTime::from_mseconds(500)) {
            Some(msg) => match msg.view() {
                MessageView::Eos(_) => {
                    info!("GStreamer: End-of-Stream received");
                    break;
                }
                MessageView::Error(err) => {
                    let src = err
                        .src()
                        .map(|s| s.path_string().to_string())
                        .unwrap_or_else(|| "<unknown>".into());
                    let gst_err = err.error();
                    let debug_info = err.debug().unwrap_or_default();
                    error!("GStreamer error from {src}: {gst_err} ({debug_info})");
                    // Attempt to shut down cleanly before propagating the error.
                    let _ = pipeline.set_state(State::Null);
                    bail!("GStreamer error: {gst_err}");
                }
                MessageView::Warning(w) => {
                    let debug_info = w.debug().unwrap_or_default();
                    warn!("GStreamer warning: {} ({debug_info})", w.error());
                }
                MessageView::StateChanged(sc) => {
                    if sc
                        .src()
                        .map(|s| s == pipeline.upcast_ref::<gstreamer::Object>())
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
                // No message within the timeout — yield to the async executor
                tokio::task::yield_now().await;
            }
        }
    }

    pipeline
        .set_state(State::Null)
        .context("stopping GStreamer pipeline")?;
    info!("GStreamer pipeline stopped");
    Ok(())
}

// ─── pipeline construction ────────────────────────────────────────────────────

/// Construct the GStreamer pipeline:
///
/// `udpsrc ! tsdemux ! h264parse ! v4l2h264dec ! autovideosink`
fn build_pipeline(cfg: &Config) -> Result<Pipeline> {
    let pipeline = Pipeline::new();

    // ── elements ──────────────────────────────────────────────────────────────
    let udpsrc = make_element("udpsrc", "src")?;
    let tsdemux = make_element("tsdemux", "demux")?;
    let h264parse = make_element("h264parse", "parse")?;
    let v4l2dec = make_element("v4l2h264dec", "decode")?;
    let videosink = make_element("autovideosink", "sink")?;

    // ── properties ────────────────────────────────────────────────────────────
    // udpsrc: listen on all interfaces for MPEG-TS
    udpsrc.set_property("port", cfg.rtp_port as i32);
    udpsrc.set_property("caps", &mpeg_ts_caps());

    // autovideosink: disable sync so there is no audio/video drift check
    videosink.set_property("sync", false);

    // ── add all elements to the pipeline ──────────────────────────────────────
    pipeline
        .add_many([&udpsrc, &tsdemux, &h264parse, &v4l2dec, &videosink])
        .context("adding elements to pipeline")?;

    // ── static links ──────────────────────────────────────────────────────────
    // udpsrc → tsdemux (static)
    Element::link(&udpsrc, &tsdemux).context("linking udpsrc → tsdemux")?;
    // h264parse → v4l2h264dec → autovideosink (static)
    Element::link_many([&h264parse, &v4l2dec, &videosink])
        .context("linking h264parse → v4l2h264dec → autovideosink")?;

    // tsdemux has dynamic pads; connect them to h264parse when they appear.
    let h264parse_clone = h264parse.clone();
    tsdemux.connect_pad_added(move |_demux, pad| {
        let pad_name = pad.name();
        debug!("tsdemux: new pad '{pad_name}'");

        // Only link video/x-h264 pads
        let caps = pad.current_caps().or_else(|| pad.query_caps(None));
        let is_h264 = caps
            .as_ref()
            .map(|c| {
                c.iter()
                    .any(|s| s.name() == "video/x-h264")
            })
            .unwrap_or(false);

        if !is_h264 {
            debug!("tsdemux: pad '{pad_name}' is not H.264, skipping");
            return;
        }

        let sink_pad = match h264parse_clone.static_pad("sink") {
            Some(p) => p,
            None => {
                error!("h264parse has no sink pad");
                return;
            }
        };

        if sink_pad.is_linked() {
            debug!("h264parse sink pad already linked, ignoring pad '{pad_name}'");
            return;
        }

        if let Err(e) = pad.link(&sink_pad) {
            error!("Failed to link tsdemux pad '{pad_name}' to h264parse: {e:?}");
        } else {
            info!("tsdemux pad '{pad_name}' linked to h264parse");
        }
    });

    Ok(pipeline)
}

/// Build GStreamer `Caps` for MPEG-TS over UDP.
fn mpeg_ts_caps() -> gstreamer::Caps {
    gstreamer::Caps::builder("video/mpegts")
        .field("systemstream", true)
        .field("packetsize", 188i32)
        .build()
}

/// Create a GStreamer element by factory name, returning an error if the
/// plugin is not installed.
fn make_element(factory: &str, name: &str) -> Result<Element> {
    ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| {
            format!(
                "creating GStreamer element '{factory}' — \
                 is the '{factory}' plugin installed?"
            )
        })
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn init_gst() {
        // gstreamer::init is idempotent and safe to call multiple times.
        let _ = gstreamer::init();
    }

    #[test]
    fn mpeg_ts_caps_correct() {
        init_gst();
        let caps = mpeg_ts_caps();
        let s = caps.structure(0).expect("caps must have at least one structure");
        assert_eq!(s.name(), "video/mpegts");
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
}
