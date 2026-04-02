//! Integration test for the RTSP control-plane server.
//!
//! Simulates a complete Miracast source (M1–M7) connecting to the sink's RTSP
//! server over a real TCP loopback connection.  No hardware (wpa_supplicant,
//! GStreamer, V4L2) is required.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use nicocast_v2::config::Config;
use nicocast_v2::health::AppState;
use nicocast_v2::rtsp;

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Read bytes from `stream` until a complete RTSP response header (ending with
/// `\r\n\r\n`) is available and return the raw response string.
///
/// Times out after 5 seconds to prevent tests from hanging on unexpected
/// server behaviour.
async fn read_rtsp_response(stream: &mut TcpStream) -> String {
    let mut buf = Vec::<u8>::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for RTSP response; received so far:\n{}",
            String::from_utf8_lossy(&buf)
        );

        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout(remaining, stream.read(&mut chunk))
            .await
            .expect("read timeout")
            .expect("read error");

        assert!(n > 0, "connection closed before complete response received");
        buf.extend_from_slice(&chunk[..n]);

        if String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
            break;
        }
    }

    String::from_utf8(buf).expect("non-UTF-8 response")
}

// ─── tests ───────────────────────────────────────────────────────────────────

/// Verify the complete M1→M7 Miracast handshake.
///
/// The test drives the RTSP server through the full sequence a Samsung Smart
/// View source would execute and asserts on the key fields in every response.
#[tokio::test]
async fn full_m1_to_m7_handshake() {
    let cfg = Config::default();

    // Bind on port 0 so the OS picks a free ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind test listener");
    let port = listener.local_addr().unwrap().port();

    // Start the RTSP server in a background task.
    let state = Arc::new(AppState::default());
    let cfg_srv = cfg.clone();
    let state_srv = Arc::clone(&state);
    tokio::spawn(async move {
        rtsp::serve(listener, &cfg_srv, state_srv)
            .await
            .expect("RTSP serve failed");
    });

    // Give the spawned task time to run.
    tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("failed to connect to RTSP server");

    // ── M1 OPTIONS ───────────────────────────────────────────────────────────
    stream
        .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n")
        .await
        .unwrap();
    let resp = read_rtsp_response(&mut stream).await;
    assert!(
        resp.starts_with("RTSP/1.0 200 OK"),
        "M1 OPTIONS: expected 200 OK, got:\n{resp}"
    );
    assert!(
        resp.contains("org.wfa.wfd1.0"),
        "M1 OPTIONS: Public header must include org.wfa.wfd1.0"
    );
    assert!(
        resp.contains("GET_PARAMETER"),
        "M1 OPTIONS: Public header must list GET_PARAMETER"
    );
    assert!(
        resp.contains("SETUP"),
        "M1 OPTIONS: Public header must list SETUP"
    );

    // ── M3 GET_PARAMETER ─────────────────────────────────────────────────────
    let body = "wfd_content_protection\r\nwfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n";
    let m3 = format!(
        "GET_PARAMETER rtsp://127.0.0.1:{port}/wfd1.0 RTSP/1.0\r\n\
         CSeq: 3\r\n\
         Content-Type: text/parameters\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(m3.as_bytes()).await.unwrap();
    let resp = read_rtsp_response(&mut stream).await;
    assert!(
        resp.starts_with("RTSP/1.0 200 OK"),
        "M3 GET_PARAMETER: expected 200 OK, got:\n{resp}"
    );
    assert!(
        resp.contains("wfd_content_protection: none"),
        "M3: must advertise no HDCP (wfd_content_protection: none)"
    );
    assert!(
        resp.contains("wfd_client_rtp_ports:"),
        "M3: must include RTP port info"
    );
    assert!(
        resp.contains(&cfg.rtp_port.to_string()),
        "M3: RTP port must match config"
    );

    // ── M4 SET_PARAMETER (negotiated parameters) ─────────────────────────────
    let body4 = "wfd_video_formats: 00 00 02 02 00008400 00000000 00000000 00 0000 0000 00 none none\r\n";
    let m4 = format!(
        "SET_PARAMETER rtsp://127.0.0.1:{port}/wfd1.0 RTSP/1.0\r\n\
         CSeq: 4\r\n\
         Content-Type: text/parameters\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {body4}",
        body4.len()
    );
    stream.write_all(m4.as_bytes()).await.unwrap();
    let resp = read_rtsp_response(&mut stream).await;
    assert!(
        resp.starts_with("RTSP/1.0 200 OK"),
        "M4 SET_PARAMETER: expected 200 OK, got:\n{resp}"
    );

    // ── M5 SET_PARAMETER (trigger SETUP) ─────────────────────────────────────
    let body5 = "wfd_trigger_method: SETUP\r\n";
    let m5 = format!(
        "SET_PARAMETER rtsp://127.0.0.1:{port}/wfd1.0 RTSP/1.0\r\n\
         CSeq: 5\r\n\
         Content-Type: text/parameters\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {body5}",
        body5.len()
    );
    stream.write_all(m5.as_bytes()).await.unwrap();
    let resp = read_rtsp_response(&mut stream).await;
    assert!(
        resp.starts_with("RTSP/1.0 200 OK"),
        "M5 SET_PARAMETER (trigger): expected 200 OK, got:\n{resp}"
    );

    // ── M6 SETUP ─────────────────────────────────────────────────────────────
    let m6 = format!(
        "SETUP rtsp://127.0.0.1:{port}/wfd1.0/streamid=0 RTSP/1.0\r\n\
         CSeq: 6\r\n\
         Transport: RTP/AVP/UDP;unicast;client_port={}-{}\r\n\
         \r\n",
        cfg.rtp_port,
        cfg.rtp_port + 1
    );
    stream.write_all(m6.as_bytes()).await.unwrap();
    let resp = read_rtsp_response(&mut stream).await;
    assert!(
        resp.starts_with("RTSP/1.0 200 OK"),
        "M6 SETUP: expected 200 OK, got:\n{resp}"
    );
    assert!(
        resp.contains("Session:"),
        "M6 SETUP: response must include a Session header"
    );
    assert!(
        resp.contains("Transport:"),
        "M6 SETUP: response must echo the Transport header"
    );

    // ── M7 PLAY ───────────────────────────────────────────────────────────────
    // Extract session ID from M6 response
    let session_id = resp
        .lines()
        .find(|l| l.starts_with("Session:"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().split(';').next().unwrap_or("").to_owned())
        .expect("could not parse Session ID from M6 response");

    let m7 = format!(
        "PLAY rtsp://127.0.0.1:{port}/wfd1.0/streamid=0 RTSP/1.0\r\n\
         CSeq: 7\r\n\
         Session: {session_id}\r\n\
         \r\n"
    );
    stream.write_all(m7.as_bytes()).await.unwrap();
    let resp = read_rtsp_response(&mut stream).await;
    assert!(
        resp.starts_with("RTSP/1.0 200 OK"),
        "M7 PLAY: expected 200 OK, got:\n{resp}"
    );

    // ── M16 GET_PARAMETER keep-alive ──────────────────────────────────────────
    let m16 = format!(
        "GET_PARAMETER rtsp://127.0.0.1:{port}/wfd1.0 RTSP/1.0\r\n\
         CSeq: 16\r\n\
         Session: {session_id}\r\n\
         Content-Length: 0\r\n\
         \r\n"
    );
    stream.write_all(m16.as_bytes()).await.unwrap();
    let resp = read_rtsp_response(&mut stream).await;
    assert!(
        resp.starts_with("RTSP/1.0 200 OK"),
        "M16 keep-alive: expected 200 OK, got:\n{resp}"
    );
    // Keep-alive response body must be empty
    assert!(
        resp.contains("Content-Length: 0"),
        "M16 keep-alive: body must be empty (Content-Length: 0)"
    );

    // ── TEARDOWN ──────────────────────────────────────────────────────────────
    let teardown = format!(
        "TEARDOWN rtsp://127.0.0.1:{port}/wfd1.0/streamid=0 RTSP/1.0\r\n\
         CSeq: 17\r\n\
         Session: {session_id}\r\n\
         \r\n"
    );
    stream.write_all(teardown.as_bytes()).await.unwrap();
    let resp = read_rtsp_response(&mut stream).await;
    assert!(
        resp.starts_with("RTSP/1.0 200 OK"),
        "TEARDOWN: expected 200 OK, got:\n{resp}"
    );
}

/// Verify that an unknown RTSP method returns 501 Not Implemented.
#[tokio::test]
async fn unknown_method_returns_501() {
    let cfg = Config::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(AppState::default());
    tokio::spawn(async move {
        rtsp::serve(listener, &cfg, state).await.ok();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    stream
        .write_all(b"DESCRIBE rtsp://127.0.0.1/wfd1.0 RTSP/1.0\r\nCSeq: 1\r\n\r\n")
        .await
        .unwrap();
    let resp = read_rtsp_response(&mut stream).await;
    assert!(
        resp.contains("501"),
        "unknown method must return 501, got:\n{resp}"
    );
}
