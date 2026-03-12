//! RTSP control-plane server for Miracast (Wi-Fi Display).
//!
//! Implements the **M1–M7** handshake required by the Wi-Fi Display
//! specification and handles **GET_PARAMETER keep-alives** (M16) so that
//! Samsung Smart View doesn't time out the session.
//!
//! # Miracast RTSP message flow (sink-side)
//!
//! ```text
//! Source → Sink  OPTIONS  *  RTSP/1.0        (M1)
//! Sink   → Src   200 OK   (Public: …)
//!
//! Source → Sink  GET_PARAMETER rtsp://…  (M3 – parameter query)
//! Sink   → Src   200 OK   (body: wfd parameter list)
//!
//! Source → Sink  SET_PARAMETER rtsp://…  (M4 – negotiated params)
//! Sink   → Src   200 OK
//!
//! Source → Sink  SET_PARAMETER rtsp://…  (M5 – trigger SETUP)
//! Sink   → Src   200 OK
//!
//! Source → Sink  SETUP    rtsp://…/wfd1.0/streamid=0  (M6)
//! Sink   → Src   200 OK   (Session: …; Transport: …)
//!
//! Source → Sink  PLAY     rtsp://…/wfd1.0/streamid=0  (M7)
//! Sink   → Src   200 OK
//!
//! Source → Sink  GET_PARAMETER (keep-alive)            (M16)
//! Sink   → Src   200 OK
//! ```

use anyhow::{Context, Result};
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

use crate::config::Config;

// ─── constants ───────────────────────────────────────────────────────────────

const RTSP_VERSION: &str = "RTSP/1.0";
const CRLF: &str = "\r\n";
/// Maximum RTSP message size we are willing to read (64 KiB).
const MAX_MSG_BYTES: usize = 65_536;

// ─── public entry point ──────────────────────────────────────────────────────

/// Bind the RTSP listener and handle incoming connections.
///
/// Each connection is serviced in its own `tokio` task so multiple
/// sources can connect simultaneously (though Miracast typically uses
/// a single source at a time).
pub async fn serve(cfg: &Config) -> Result<()> {
    let addr = format!("0.0.0.0:{}", cfg.rtsp_port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding RTSP listener on {addr}"))?;
    info!("RTSP server listening on {addr}");

    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .context("accepting RTSP connection")?;
        info!("RTSP: new connection from {peer}");

        let rtp_port = cfg.rtp_port;
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, rtp_port).await {
                error!("RTSP connection error ({peer}): {e:#}");
            }
        });
    }
}

// ─── connection handler ───────────────────────────────────────────────────────

async fn handle_connection(mut stream: TcpStream, rtp_port: u16) -> Result<()> {
    let mut buf = BytesMut::with_capacity(4096);
    let mut session_id: Option<String> = None;

    loop {
        // Read data from the source
        let n = stream
            .read_buf(&mut buf)
            .await
            .context("reading from RTSP socket")?;

        if n == 0 {
            info!("RTSP: connection closed by peer");
            return Ok(());
        }

        if buf.len() > MAX_MSG_BYTES {
            warn!("RTSP: message too large ({} bytes), closing", buf.len());
            return Ok(());
        }

        // Attempt to parse a complete RTSP message
        let raw = std::str::from_utf8(&buf).unwrap_or("");
        if !raw.contains("\r\n\r\n") {
            // Header not complete yet — keep reading
            continue;
        }

        let msg_len = match raw.find("\r\n\r\n") {
            Some(idx) => idx + 4,
            None => continue,
        };

        let request_str = raw[..msg_len].to_owned();
        buf.clear(); // consume the message

        debug!("RTSP ← {}", request_str.trim());

        let request = match RtspRequest::parse(&request_str) {
            Some(r) => r,
            None => {
                warn!("RTSP: failed to parse request, ignoring");
                continue;
            }
        };

        let cseq = request.header("CSeq").unwrap_or("0").to_owned();
        let response = dispatch(&request, &cseq, rtp_port, &mut session_id);

        debug!("RTSP → {}", response.trim());
        stream
            .write_all(response.as_bytes())
            .await
            .context("writing RTSP response")?;
    }
}

// ─── request dispatcher ──────────────────────────────────────────────────────

fn dispatch(
    req: &RtspRequest,
    cseq: &str,
    rtp_port: u16,
    session_id: &mut Option<String>,
) -> String {
    match req.method.as_str() {
        "OPTIONS" => handle_options(cseq),
        "GET_PARAMETER" => handle_get_parameter(req, cseq, rtp_port, session_id),
        "SET_PARAMETER" => handle_set_parameter(req, cseq, session_id),
        "SETUP" => handle_setup(req, cseq, rtp_port, session_id),
        "PLAY" => handle_play(cseq, session_id),
        "TEARDOWN" => handle_teardown(cseq, session_id),
        other => {
            warn!("RTSP: unhandled method '{other}'");
            rtsp_response(cseq, 501, "Not Implemented", None, None)
        }
    }
}

// ─── method handlers ─────────────────────────────────────────────────────────

/// M1 — OPTIONS: advertise all supported methods.
fn handle_options(cseq: &str) -> String {
    rtsp_response(
        cseq,
        200,
        "OK",
        Some(&[
            "Public: org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER, SETUP, PLAY, TEARDOWN",
        ]),
        None,
    )
}

/// M3 — GET_PARAMETER: return WFD capability parameters.
///
/// Samsung Smart View queries the parameters listed in the request body.
/// We respond with the values this sink supports.  The critical ones are:
///
/// * `wfd_content_protection` → `none` (no HDCP)
/// * `wfd_video_formats`      → H.264 CBP Level 3.1 @ 1920×1080p30
/// * `wfd_audio_codecs`       → LPCM 44100 Hz stereo
/// * `wfd_client_rtp_ports`   → our UDP RTP port
fn handle_get_parameter(
    req: &RtspRequest,
    cseq: &str,
    rtp_port: u16,
    session_id: &mut Option<String>,
) -> String {
    // Empty body = keep-alive ping (M16). Respond with empty 200 OK.
    let body = req.body.trim();
    if body.is_empty() {
        // Bind the formatted header to a local so `extra_headers` can borrow
        // it without leaking memory.
        let session_hdr = session_id
            .as_deref()
            .map(|sid| format!("Session: {sid}"));
        let extra_headers: Vec<&str> = session_hdr.iter().map(String::as_str).collect();
        return rtsp_response(cseq, 200, "OK", Some(&extra_headers), None);
    }

    // Build a response body with the WFD parameters we support.
    // We only include parameters that were actually queried.
    let mut response_params = Vec::new();

    for line in body.lines() {
        let param = line.trim();
        match param {
            "wfd_content_protection" => {
                // Signal that HDCP is NOT required — required for Samsung compatibility
                response_params.push("wfd_content_protection: none".to_owned());
            }
            "wfd_video_formats" => {
                // H.264 CBP Level 3.1, 1920×1080p30, native index 0
                // Format: <native> <preferred> <profile> <level>
                //         <CEA-res> <VESA-res> <HH-res> <latency>
                //         <min-slice> <slice-enc> <frame-rate-ctrl>
                response_params.push(
                    "wfd_video_formats: 00 00 02 02 00008400 00000000 00000000 00 0000 0000 00 none none"
                        .to_owned(),
                );
            }
            "wfd_audio_codecs" => {
                // LPCM 44.1 kHz, 2 channels (stereo), latency 0
                response_params.push("wfd_audio_codecs: LPCM 00000003 00".to_owned());
            }
            "wfd_client_rtp_ports" => {
                // UDP transport, RTP on rtp_port, RTCP on rtp_port+1
                response_params.push(format!(
                    "wfd_client_rtp_ports: RTP/AVP/UDP;unicast {rtp_port} {} mode=play",
                    rtp_port + 1
                ));
            }
            "wfd_presentation_URL" => {
                response_params.push("wfd_presentation_URL: none none".to_owned());
            }
            "wfd_display_edid" => {
                response_params.push("wfd_display_edid: none".to_owned());
            }
            "wfd_coupled_sink" => {
                response_params.push("wfd_coupled_sink: none".to_owned());
            }
            "wfd_uibc_capability" => {
                response_params.push("wfd_uibc_capability: none".to_owned());
            }
            "wfd_standby_resume_capability" => {
                response_params.push("wfd_standby_resume_capability: none".to_owned());
            }
            _ => {
                debug!("GET_PARAMETER: ignoring unknown param '{param}'");
            }
        }
    }

    let body_str = response_params.join(CRLF) + CRLF;
    rtsp_response(cseq, 200, "OK", None, Some(&body_str))
}

/// M4 / M5 — SET_PARAMETER: accept negotiated parameters or trigger commands.
fn handle_set_parameter(
    req: &RtspRequest,
    cseq: &str,
    session_id: &mut Option<String>,
) -> String {
    for line in req.body.lines() {
        let line = line.trim();
        if line.starts_with("wfd_trigger_method:") {
            let trigger = line.trim_start_matches("wfd_trigger_method:").trim();
            info!("RTSP: trigger method received: '{trigger}'");
        }
    }

    let extra_headers: Vec<String> = if let Some(sid) = session_id.as_deref() {
        vec![format!("Session: {sid}")]
    } else {
        vec![]
    };
    let refs: Vec<&str> = extra_headers.iter().map(String::as_str).collect();
    rtsp_response(cseq, 200, "OK", Some(&refs), None)
}

/// M6 — SETUP: negotiate transport and allocate a session.
fn handle_setup(
    req: &RtspRequest,
    cseq: &str,
    rtp_port: u16,
    session_id: &mut Option<String>,
) -> String {
    // Allocate a new session ID if we don't have one yet.
    let sid = session_id
        .get_or_insert_with(|| format!("{:016x}", rand_session_id()))
        .clone();

    // Echo back the transport line from the request, substituting our port.
    let transport = req
        .header("Transport")
        .map(|t| {
            if t.contains("client_port=") {
                t.to_owned()
            } else {
                format!("{t};server_port={rtp_port}-{}", rtp_port + 1)
            }
        })
        .unwrap_or_else(|| {
            format!(
                "RTP/AVP/UDP;unicast;client_port={rtp_port}-{};server_port={rtp_port}-{}",
                rtp_port + 1,
                rtp_port + 1
            )
        });

    info!("RTSP: SETUP — session={sid} transport={transport}");

    rtsp_response(
        cseq,
        200,
        "OK",
        Some(&[
            &format!("Session: {sid};timeout=60"),
            &format!("Transport: {transport}"),
        ]),
        None,
    )
}

/// M7 — PLAY: acknowledge that streaming should begin.
fn handle_play(cseq: &str, session_id: &mut Option<String>) -> String {
    info!("RTSP: PLAY received — streaming should begin");
    let extra_headers: Vec<String> = if let Some(sid) = session_id.as_deref() {
        vec![format!("Session: {sid}")]
    } else {
        vec![]
    };
    let refs: Vec<&str> = extra_headers.iter().map(String::as_str).collect();
    rtsp_response(cseq, 200, "OK", Some(&refs), None)
}

/// TEARDOWN: end the session.
fn handle_teardown(cseq: &str, session_id: &mut Option<String>) -> String {
    info!("RTSP: TEARDOWN — session ended");
    *session_id = None;
    rtsp_response(cseq, 200, "OK", None, None)
}

// ─── response builder ─────────────────────────────────────────────────────────

/// Build a complete RTSP response string.
fn rtsp_response(
    cseq: &str,
    status: u16,
    reason: &str,
    extra_headers: Option<&[&str]>,
    body: Option<&str>,
) -> String {
    let body_str = body.unwrap_or("");
    let mut out = format!("{RTSP_VERSION} {status} {reason}{CRLF}");
    out.push_str(&format!("CSeq: {cseq}{CRLF}"));
    out.push_str(&format!("Server: nicocast/1.0{CRLF}"));

    if let Some(headers) = extra_headers {
        for h in headers {
            if !h.is_empty() {
                out.push_str(h);
                out.push_str(CRLF);
            }
        }
    }

    if !body_str.is_empty() {
        out.push_str(&format!("Content-Type: text/parameters{CRLF}"));
        out.push_str(&format!("Content-Length: {}{CRLF}", body_str.len()));
    } else {
        out.push_str(&format!("Content-Length: 0{CRLF}"));
    }

    out.push_str(CRLF);
    out.push_str(body_str);
    out
}

// ─── RTSP request parser ──────────────────────────────────────────────────────

/// A minimally-parsed RTSP request.
#[derive(Debug)]
struct RtspRequest {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl RtspRequest {
    fn parse(raw: &str) -> Option<Self> {
        let mut lines = raw.lines();

        // Request line: METHOD uri RTSP/1.0
        let request_line = lines.next()?;
        let mut parts = request_line.splitn(3, ' ');
        let method = parts.next()?.to_owned();
        let uri = parts.next()?.to_owned();
        // version — we accept it but don't validate strictly

        let mut headers = Vec::new();
        let mut content_length: usize = 0;

        for line in lines.by_ref() {
            if line.is_empty() {
                break; // blank line separates headers from body
            }
            if let Some((k, v)) = line.split_once(':') {
                let key = k.trim().to_owned();
                let val = v.trim().to_owned();
                if key.eq_ignore_ascii_case("content-length") {
                    content_length = val.parse().unwrap_or(0);
                }
                headers.push((key, val));
            }
        }

        // Collect body lines
        let body_lines: Vec<&str> = lines.collect();
        let body = body_lines.join("\r\n");
        let _ = content_length; // already captured for future use

        Some(Self {
            method,
            uri,
            headers,
            body,
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

// ─── tiny utilities ───────────────────────────────────────────────────────────

/// Generate a pseudo-random 64-bit session ID from the system clock.
/// This is sufficient for a single-peer Miracast session.
fn rand_session_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 20))
        .unwrap_or(0xDEAD_BEEF_1234_5678)
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_options_request() {
        let raw = "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let req = RtspRequest::parse(raw).expect("parse failed");
        assert_eq!(req.method, "OPTIONS");
        assert_eq!(req.uri, "*");
        assert_eq!(req.header("CSeq"), Some("1"));
    }

    #[test]
    fn parse_get_parameter_keepalive() {
        let raw = "GET_PARAMETER rtsp://192.168.1.2/wfd1.0 RTSP/1.0\r\nCSeq: 5\r\nContent-Length: 0\r\n\r\n";
        let req = RtspRequest::parse(raw).expect("parse failed");
        assert_eq!(req.method, "GET_PARAMETER");
        assert!(req.body.trim().is_empty());
    }

    #[test]
    fn options_response_contains_wfd() {
        let resp = handle_options("1");
        assert!(resp.contains("org.wfa.wfd1.0"));
        assert!(resp.contains("GET_PARAMETER"));
        assert!(resp.contains("SET_PARAMETER"));
        assert!(resp.contains("SETUP"));
        assert!(resp.contains("PLAY"));
        assert!(resp.contains("TEARDOWN"));
    }

    #[test]
    fn get_parameter_keepalive_returns_200() {
        let raw = "GET_PARAMETER rtsp://192.168.1.2/wfd1.0 RTSP/1.0\r\nCSeq: 5\r\nContent-Length: 0\r\n\r\n";
        let req = RtspRequest::parse(raw).unwrap();
        let cseq = req.header("CSeq").unwrap_or("5").to_owned();
        let mut sid = None;
        let resp = handle_get_parameter(&req, &cseq, 16384, &mut sid);
        assert!(resp.starts_with("RTSP/1.0 200 OK"));
    }

    #[test]
    fn get_parameter_returns_hdcp_none() {
        let body = "wfd_content_protection\r\n";
        let raw = format!(
            "GET_PARAMETER rtsp://192.168.1.2/wfd1.0 RTSP/1.0\r\nCSeq: 3\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let req = RtspRequest::parse(&raw).unwrap();
        let cseq = req.header("CSeq").unwrap_or("3").to_owned();
        let mut sid = None;
        let resp = handle_get_parameter(&req, &cseq, 16384, &mut sid);
        assert!(
            resp.contains("wfd_content_protection: none"),
            "expected hdcp=none in response, got:\n{resp}"
        );
    }

    #[test]
    fn rtsp_response_format() {
        let r = rtsp_response("7", 200, "OK", Some(&["Session: abc123"]), None);
        assert!(r.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(r.contains("CSeq: 7\r\n"));
        assert!(r.contains("Session: abc123\r\n"));
        assert!(r.contains("Content-Length: 0\r\n"));
    }

    #[test]
    fn teardown_clears_session() {
        let mut sid = Some("abc123".to_owned());
        let resp = handle_teardown("9", &mut sid);
        assert!(resp.starts_with("RTSP/1.0 200 OK"));
        assert!(sid.is_none());
    }
}
