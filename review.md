# Code Audit — nicocast-v2

> Datum: 2026-04-02  
> Scope: Vollständiger Quellcode (`src/`, `tests/`, `status_monitor/`, `setup.sh`, `Dockerfile`)  
> Ziel: Analyse der Funktionsweise, Datenflüsse, Abfragen, Abhängigkeiten und reibungsloser Ablauf

---

## 1. Architektur-Überblick

nicocast-v2 ist ein Miracast-Sink (Empfänger) für den Raspberry Pi Zero 2W, geschrieben in Rust (Tokio async runtime). Die Anwendung startet fünf parallele Tasks:

```
main()
 ├── health::serve          → HTTP GET /health (Port 8080)
 ├── video::run_pipeline    → GStreamer-Pipeline (UDP RTP → H.264 → Display)
 ├── rtsp::serve            → RTSP-Control-Plane (M1–M7, Keepalives)
 ├── p2p::P2pManager::run   → WiFi-Direct via wpa_supplicant D-Bus
 └── airplay::run_uxplay    → UxPlay-Subprocess (optional, AirPlay)
```

Der gemeinsame Zustand wird über `Arc<AppState>` (drei `AtomicU8`-Felder: `p2p`, `rtsp`, `video`) zwischen den Tasks geteilt, **ohne Mutex** — korrekt und effizient.

Externe Abhängigkeiten:
- `wpa_supplicant` (D-Bus v2 / `fi.w1.wpa_supplicant1`) — P2P-Steuerung
- `GStreamer` (C-Bibliothek via `gstreamer-rs 0.25`) — Video-Dekodierung
- `uxplay` (optionaler externer Prozess) — AirPlay

---

## 2. Datenflusse

### 2.1 Startup-Sequenz (bewusst geordnet)

```
Config laden → RTSP-Port binden → Health-Task starten
→ Video-Task starten → RTSP-Task starten → P2P-Task starten
→ AirPlay-Task starten
```

**Positiv**: RTSP-Port wird *vor* P2P-Advertising gebunden. Damit ist der Port garantiert offen, wenn Samsung Smart View nach der P2P-Entdeckung sofort eine Verbindung aufbaut. Dies verhindert die „Connection refused"-Race-Condition, die manche Samsung-Firmware nicht wiederholt.

### 2.2 Miracast-Verbindungsfluss

```
Samsung TV                        nicocast
   │──── WiFi-Direct Probe ──────►│  (P2P: WFD IEs gesetzt, Find+Listen aktiv)
   │◄─── P2P-Response ────────────│
   │                              │
   │──── RTSP OPTIONS (M1) ──────►│  handle_options()
   │◄─── 200 OK + Public-Header ──│
   │──── RTSP GET_PARAMETER (M3) ►│  handle_get_parameter()
   │◄─── 200 OK + WFD-Params ─────│  (video_formats, audio_codecs, rtp_ports)
   │──── RTSP SET_PARAMETER (M4) ►│  handle_set_parameter()
   │◄─── 200 OK ──────────────────│
   │──── RTSP SET_PARAMETER (M5) ►│  handle_set_parameter() (trigger: SETUP)
   │◄─── 200 OK ──────────────────│
   │──── RTSP SETUP (M6) ────────►│  handle_setup() → Session-ID generiert
   │◄─── 200 OK + Session+Transp ─│
   │──── RTSP PLAY (M7) ─────────►│  handle_play()
   │◄─── 200 OK ──────────────────│
   │                              │
   │────── RTP/UDP-Stream ───────►│  GStreamer: udpsrc → tsdemux → h264parse
   │                              │            → v4l2h264dec → autovideosink
   │──── GET_PARAMETER (M16) ────►│  Keepalive alle ~30 s
   │◄─── 200 OK (leerer Body) ────│
```

### 2.3 GStreamer-Pipeline-Datenfluss

```
UDP-Datagramm (MPEG-TS, 188-Byte-Pakete)
  → udpsrc (port=rtp_port, caps=video/mpegts)
  → tsdemux (dynamischer Pad: pad-added-Handler verknüpft H.264-ES)
  → h264parse
  → v4l2h264dec  (HW-bevorzugt) | avdec_h264 (SW-Fallback)
  → autovideosink (sync=false — kein A/V-Sync, da kein Audio)
```

Der dynamische Pad-Handler prüft `video/x-h264`-Caps und verknüpft nur den ersten H.264-Stream. Weitere Streams (z.B. Audio) werden still ignoriert.

### 2.4 P2P D-Bus-Datenfluss

```
P2pManager::new()
  → Connection::system() [D-Bus System-Bus]
  → WpaSupplicantProxy::get_interface(wifi_interface)
     (Retry-Schleife: bis zu connect_retries Mal)
  → optional CreateInterface() bei "InterfaceUnknown"

P2pManager::run()
  → configure_wfd_ies()
     → D-Bus: WpaP2PDeviceProxy.set_wfd_ies(bytes)
     → Fallback: wpa_cli wfd_subelem_set
  → configure_p2p_device_config()
     → D-Bus: set_p2p_device_config({DeviceName, PrimaryDeviceType, NoGroupIface})
  → start_discovery()
     → D-Bus: P2PDevice.Find({})
     → D-Bus: P2PDevice.Listen(listen_secs)
  → Loop: alle listen_secs Sekunden start_discovery() wiederholen
```

---

## 3. Modul-Analyse

### 3.1 `config.rs` ✅

- Alle Felder haben sinnvolle Defaults; TOML-Parsing mit `serde`.
- Validation prüft `wfd_subelems` auf gültige, geradzahlige Hex-Länge.
- `Config::load()` verkettet korrekt: Datei lesen → TOML parsen → validieren.
- Unit-Tests decken Happy Path und alle Fehlerfälle ab.

**Beobachtung**: Nur `wfd_subelems` wird validiert. Felder wie `device_name` (leer?), `rtsp_port` (0?), `rtp_port` (Kollision mit RTSP-Port?) werden nicht geprüft.

### 3.2 `p2p.rs` ✅ (mit Anmerkungen)

- D-Bus-Proxies werden korrekt über `zbus`-Makros generiert.
- Retry-Logik für wpa_supplicant-Startup ist durchdacht.
- WFD-IE D-Bus → wpa_cli-Fallback ist praktisch für ältere wpa_supplicant-Builds.
- `parse_wfd_subelement` ist korrekt: 1-Byte-ID + 1-Byte-Länge (wpa_supplicant-Format).

**Problem — Proxy wird bei jedem Aufruf neu gebaut**: `p2p_proxy()` erstellt bei jedem Aufruf (`configure_wfd_ies`, `configure_p2p_device_config`, `start_discovery`, und bei jedem Refresh-Zyklus) einen neuen `WpaP2PDeviceProxy`. Das verursacht unnötige D-Bus-Introspektions-Roundtrips in einer Endlosschleife.

```rust
// src/p2p.rs:696 — jede Methode ruft das auf
async fn p2p_proxy(&self) -> Result<WpaP2PDeviceProxy<'_>> {
    WpaP2PDeviceProxy::builder(&self.conn)
        .path(self.iface_path.clone())  // clone() bei jedem Aufruf
        ...
        .build().await  // D-Bus-Introspection jedes Mal
}
```

**Problem — `listen_secs` als `i32`**: `listen_secs` ist `u32`, wird aber als `i32` an `proxy.listen()` übergeben. Werte > 2.147.483.647 (ca. 68 Jahre) würden wrappen — in der Praxis kein Problem, aber ein stilles Cast-Risiko.

```rust
// src/p2p.rs:679
let timeout_secs = self.cfg.p2p.listen_secs as i32;  // i32-Wrapping bei >MAX_I32
```

### 3.3 `rtsp.rs` ✅ (mit Anmerkungen)

- Vollständige M1–M7-Implementierung inkl. Keepalive (M16) und TEARDOWN.
- `MAX_MSG_BYTES`-Cap (64 KiB) verhindert Memory-Exhaustion durch manipulierte Content-Length.
- `State`-Updates via `AtomicU8` sind korrekt.
- Gute Unit-Tests + vollständiger Integrationstest in `tests/rtsp_handshake.rs`.

**Problem — CSeq der M2-Anfrage ist hartkodiert**:

```rust
// src/rtsp.rs:234
"CSeq: 2{CRLF}\"
```

Der Sink sendet bei `rtsp_send_m2 = true` immer `CSeq: 2`. Es gibt keinen Sequenzzähler. Falls der Source zufällig ebenfalls CSeq 2 verwendet, könnten manche Implementierungen verwirrt werden. RFC 2326 schreibt vor, dass sink-initiierte Anfragen eigene, aufsteigende CSeq-Nummern haben müssen.

**Problem — Session-ID: geringe Entropie**:

```rust
// src/rtsp.rs:587–592
fn rand_session_id() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 20))
        .unwrap_or(0xDEAD_BEEF_1234_5678)
}
```

Die Session-ID basiert auf `subsec_nanos` (~30 Bit Entropie) XOR einem verschobenen Sekundenwert. Sie ist **nicht kryptografisch sicher**. Für einen lokalen Miracast-Sink ohne Netzwerksicherheitsanforderungen ist das vertretbar, sollte aber dokumentiert sein.

**Problem — M2-Response-Body wird nicht vollständig gelesen**: Der Lese-Loop in `send_m2_get_parameter` bricht ab, sobald `\r\n\r\n` im Buffer steht, ignoriert aber den `Content-Length`-Header der Response. Schickt der Source einen Body > verfügbarem Buffer, wird er teilweise in den Hauptlese-Buffer gerissen (da `buf` geteilt ist).

**Beobachtung — RTSP-Parser ignoriert `content_length`**:

```rust
// src/rtsp.rs:564
let _ = content_length; // already captured for future use
```

Der geparste `content_length`-Wert wird nicht genutzt, um den Body korrekt zu begrenzen. Stattdessen werden einfach alle restlichen Zeilen als Body gesammelt. Dies ist für Miracast ausreichend, da die Nachrichten klein sind, entspricht aber nicht RFC 2326.

### 3.4 `video.rs` ✅

- `PipelineGuard` garantiert `set_state(Null)` auch bei Task-Abbruch — robustes RAII.
- `connect_pad_added`-Handler prüft `is_linked()` korrekt, um Mehrfachverknüpfungen zu vermeiden.
- `sync_state_with_parent()` nach dynamischer Pad-Verknüpfung ist für gstreamer-rs 0.25 korrekt.
- Hardware/Software-Decoder-Fallback ist transparent.

**Beobachtung — kein Audio**: Der Pipeline fehlt ein Audio-Zweig. Miracast-Streams (MPEG-TS) enthalten typischerweise einen AAC- oder LPCM-Audio-Track. Das GStreamer-Plugin `tsdemux` wird die Audio-Pads erzeugen, aber da kein Handler sie verknüpft, fließt kein Audio. Das `M3`-Antwort-Parameter `wfd_audio_codecs: LPCM 00000003 00` signalisiert LPCM-Unterstützung, die Pipeline liefert sie aber nicht — das erzeugt keine Fehler, aber auch keinen Ton.

**Beobachtung — `state.video` wird zu früh auf `STATE_PLAYING` gesetzt**: Der Zustand wird direkt nach `set_state(Playing)` gesetzt, noch bevor GStreamer tatsächlich Daten dekodiert. Korrekt wäre es bei `MessageView::StateChanged` auf PLAYING.

### 3.5 `health.rs` ✅

- Saubere Lock-freie Zustands-Serialisierung über `AtomicU8`.
- `health_port = 0` deaktiviert den Endpunkt sauber durch `pending::<()>()`.
- Einfache, funktionale HTTP-Implementierung ohne externe HTTP-Crates.

**Beobachtung — kein Timeout beim HTTP-Request-Lesen**: Der Health-Server liest mit einem 512-Byte-Buffer, wartet aber unbegrenzt auf Daten. Ein langsamer Client könnte einen Tokio-Task dauerhaft blockieren. Da aber jeder Request in einem eigenen `tokio::spawn` läuft, betrifft das nicht andere Clients.

### 3.6 `airplay.rs` ⚠️

- `run_uxplay` gibt bei `airplay_enabled = false` sofort `pending()` zurück — korrekt.
- Supervised-Process-Loop mit 5-Sekunden-Restart-Delay ist vernünftig.

**Problem — wlan0-Konflikt nicht verhindert**: Wenn `airplay_enabled = true` und der P2P-Manager aktiv ist, konkurrieren beide um `wlan0`. Der Kommentar im Code weist auf diesen Konflikt hin, es gibt aber keinen Startup-Guard:

```rust
// src/airplay.rs:20–22 (Kommentar)
// `wlan0` cannot be in P2P mode (Samsung Miracast) and in normal station /
// AP mode (AirPlay) simultaneously.
```

Dieses Szenario könnte zu nicht-offensichtlichen Netzwerkfehlern führen, ohne klare Fehlermeldung.

### 3.7 `logger.rs` ✅

- Dual-Sink-Logging (Datei + stderr) über `tracing-subscriber`.
- `WorkerGuard` korrekt an den Aufrufer zurückgegeben, damit der Hintergrund-Writer-Thread läuft.
- `split_log_path` deckt absolute, relative und Nur-Dateiname-Pfade korrekt ab.

### 3.8 `main.rs` ⚠️

**Problem — kein Graceful Shutdown der übrigen Tasks**: `tokio::select!` wartet auf den ersten abgeschlossenen Task. Die anderen `JoinHandle`s werden gedroppt — Tokio **bricht** einen gedropten JoinHandle **nicht** automatisch ab; die Tasks laufen als "detached" weiter, bis die Runtime beim Prozessende abbricht.

In der Praxis endet der Prozess sofort nach dem `select!`-Block (`info!(...); Ok(())`), womit die Runtime und alle Tasks terminiert werden. Das ist funktional korrekt, könnte aber bei künftiger Erweiterung (z.B. graceful cleanup mit `handle.abort()`) missverständlich sein.

```rust
// src/main.rs:159–202 — kein .abort() auf die anderen Handles
let exit_reason = tokio::select! {
    res = video_handle => ...,
    res = rtsp_handle  => ...,
    // video_handle, rtsp_handle etc. sind nach select! gedroppt, aber nicht aborted
};
```

**Beobachtung — `ensure_xdg_runtime_dir` wird nach `logger::init` mit einem `#[allow(deprecated)]` auf `set_var` aufgerufen**: Der `set_var`-Aufruf findet vor dem Spawn anderer Threads statt (GStreamer läuft noch nicht, Tokio-Runtime läuft aber bereits). Technisch gibt es bei einem Single-Threaded-Kontext kein Problem, aber die Annotation verschleiert, dass der Aufruf im Kontext eines Multi-Thread-Runtime geschieht.

---

## 4. Konfiguration

### 4.1 Dokumentationsfehler in `config.toml`

Das Byte-Layout des `wfd_subelems`-Kommentars ist irreführend:

```toml
# Byte layout (subelement 0x00 — WFD Device Information, 6 bytes):
#   00        subelement ID  = 0x00 (Device Info)
#   0006      length         = 6 bytes      ← FEHLER
```

Der Hex-String `000600111c4400c8` hat die Struktur:
- `00` → Subelement-ID (1 Byte)
- `06` → Länge (1 Byte) = 6
- `00 11 1c 44 00 c8` → 6-Byte-Payload

Der Kommentar zeigt `0006` für "length", was wie ein 2-Byte-Längenfeld aussieht. Tatsächlich ist `00` aber bereits als ID beschrieben, und nur `06` ist die Länge. Der Code in `parse_wfd_subelement` ist korrekt (`ie_bytes[1] as usize`), der Kommentar ist es nicht.

### 4.2 Fehlende Config-Optionen

- **Kein `mode`-Feld**: Laut Kommentar in `airplay.rs` sollte ein `mode`-Konfigurationsfeld vorhanden sein, um zwischen Miracast und AirPlay-Betrieb umzuschalten. Es existiert nicht.
- **Kein `rtp_port_range`**: Der RTCP-Port wird als `rtp_port + 1` berechnet, ist aber nicht konfigurierbar.
- **Keine Validierung von Port-Konflikten**: `rtsp_port`, `rtp_port` und `health_port` könnten denselben Wert haben — kein Check in `validate()`.

---

## 5. Abhängigkeiten

| Crate | Version | Bemerkung |
|---|---|---|
| `tokio` | 1 (full) | Stabil, bewährt |
| `zbus` | 4 | Aktuelle Version, korrekte async D-Bus-Implementierung |
| `zvariant` | 4 | Muss zu zbus 4 passen — passt |
| `gstreamer` | 0.25 | Neueste stabile Version; API-Änderungen in 0.25 korrekt berücksichtigt |
| `tracing` / `tracing-subscriber` / `tracing-appender` | 0.1 / 0.3 / 0.2 | Standard-Logging-Stack |
| `serde` + `toml` | 1 / 0.8 | Stabil |
| `anyhow` | 1 | Gute Wahl für Fehlerbehandlung in einer Anwendung |
| `bytes` | 1 | Nur für `BytesMut` in `rtsp.rs` |

**Keine sicherheitsrelevanten bekannten Schwachstellen** in den deklarierten Abhängigkeiten (nach aktuellem Advisory-Stand).

**Kein `rand`-Crate**: Die Session-ID-Generierung wurde manuell implementiert statt `rand` zu verwenden. Das erhöht die Komplexität ohne Gewinn.

---

## 6. Tests

| Test-Datei | Abdeckung |
|---|---|
| `src/config.rs` | Defaults, TOML-Parsing, Validation (Hex, Leerstring, ungültige Zeichen) |
| `src/p2p.rs` | `hex_to_bytes`, `wfd_ie_subelement_structure`, `wps_dev_type_to_bytes`, `parse_wfd_subelement` (inkl. Edge-Cases) |
| `src/rtsp.rs` | Request-Parser, OPTIONS, GET_PARAMETER (keepalive + params), Response-Format, TEARDOWN |
| `src/health.rs` | Standardzustand, JSON-Serialisierung |
| `src/video.rs` | MPEG-TS-Caps, Unbekanntes Plugin, H.264-Decoder-Auswahl |
| `tests/rtsp_handshake.rs` | Vollständiger M1→M7-Handshake + M16-Keepalive + TEARDOWN über echten TCP-Socket |

**Positiv**: Die Integrationstests in `rtsp_handshake.rs` simulieren den kompletten Miracast-Verbindungsablauf ohne Hardware. Gut.

**Lücken**:
- Kein Test für `configure_wfd_ies()` / `configure_p2p_device_config()` (D-Bus kann in Unit-Tests schwer gemockt werden)
- Kein Test für `ensure_xdg_runtime_dir()`
- Kein Test für den `rand_session_id()`-Fallback-Wert
- `video::make_element_udpsrc_succeeds` ist ein bekannter CI-Fehler (fehlendes GStreamer-Plugin in CI)

---

## 7. Status-Monitor (`status_monitor/nicocast_status.py`)

- Tail-basiertes Log-Parsing: funktioniert, ist aber fragil. Umbenennung von Log-Strings würde Status-Erkennung still brechen.
- Keine Reconnect-Logik: wenn die Log-Datei rotiert wird (`logrotate`), bleibt der File-Handle auf der alten Datei.
- `fbi` als Framebuffer-Viewer ist veraltet und nicht immer auf RPi OS vorinstalliert.
- `_fbi_proc` als globale Variable ohne Threading-Schutz — aber da es sich um ein Single-Threaded-Script handelt, ist das unbedenklich.

**Verbesserungsvorschlag**: Statt Log-Scraping den HTTP-Health-Endpunkt (`GET /health`) pollen — wartungsfreundlicher und entkoppelt von Log-Format-Änderungen.

---

## 8. Docker & Deployment

- **Zweistufiger Build** (builder + runtime) ist korrekt umgesetzt: nur die notwendigen Runtime-Bibliotheken im finalen Image.
- **ELF-Verifikation** nach dem Build (`grep -q "ELF 64-bit LSB.*aarch64"`) fängt fehlkonfigurierte Cross-Compiler.
- **Cache-Layer-Optimierung** mit Stub-`main.rs` ist eine bewährte Technik.
- `setup.sh` hat robuste Fehlerbehandlung (`set -euo pipefail`, `apt_get_update`-Retry).

---

## 9. Zusammenfassung der Befunde

### Kritisch / Funktionsfehler

Keine gefunden. Der Kern-Ablauf (P2P-Entdeckung → RTSP-Handshake → GStreamer-Streaming) ist funktional korrekt implementiert.

### Mittlere Probleme

| # | Modul | Beschreibung |
|---|---|---|
| M1 | `rtsp.rs:234` | M2-Sink-Anfrage verwendet hartkodiertes `CSeq: 2` — kein Sequenzzähler |
| M2 | `rtsp.rs:586` | Session-ID aus Systemzeit — geringe Entropie, nicht kryptografisch sicher |
| M3 | `p2p.rs:696` | D-Bus-Proxy wird in jeder Refresh-Iteration (alle `listen_secs`) neu gebaut |
| M4 | `airplay.rs` | Kein Runtime-Guard gegen simultanen AirPlay + P2P-Betrieb auf `wlan0` |
| M5 | `video.rs:74` | `STATE_PLAYING` wird vor dem ersten Datenpaket gesetzt, nicht nach GStreamer-PLAYING-Zustand |

### Niedrige Priorität / Hinweise

| # | Modul | Beschreibung |
|---|---|---|
| L1 | `config.toml:48` | Kommentar zeigt `0006` als Längenfeld — korrekt sind `00` (ID) + `06` (Länge) |
| L2 | `p2p.rs:679` | `listen_secs as i32` — stilles Wrapping bei Werten > i32::MAX (in Praxis unkritisch) |
| L3 | `main.rs:159` | Übrige Tasks werden nach `select!` nicht explizit abgebrochen (`handle.abort()`) |
| L4 | `rtsp.rs:564` | Geparstes `content_length` wird nicht zur Body-Begrenzung genutzt |
| L5 | `config.rs:221` | Nur `wfd_subelems` validiert; Port-Kollisionen und leere Pflichtfelder nicht geprüft |
| L6 | `status_monitor` | Log-Scraping bricht bei Log-Umformulierungen; Health-Endpunkt wäre robuster |
| L7 | `video.rs` | Audio-Zweig fehlt; wfd_audio_codecs wird als LPCM angekündigt, aber nicht verarbeitet |

---

## 10. Empfehlungen (Priorisiert)

1. **CSeq-Zähler für M2** (`rtsp.rs`): Einen atomaren oder Mutex-geschützten Zähler für sink-initiierte Anfragen führen, damit CSeq aufsteigend und eindeutig ist.

2. **Session-ID-Entropie** (`rtsp.rs`): `getrandom`/`rand`-Crate verwenden, oder zumindest auf `std::collections::HashMap`-Hashing-Seed zurückgreifen. Alternativ explizit dokumentieren, dass keine Sicherheitsgarantie besteht.

3. **P2P-Proxy cachen** (`p2p.rs`): Den `WpaP2PDeviceProxy` einmal in `P2pManager::new()` anlegen und als Feld speichern, statt ihn in jedem Aufruf neu zu bauen.

4. **AirPlay/P2P-Konflikt absichern** (`config.rs`/`main.rs`): In `Config::validate()` oder in `main()` prüfen: wenn `airplay_enabled = true`, eine deutliche Warnung ausgeben (oder als Config-Fehler behandeln), da beide Modi `wlan0` exklusiv belegen.

5. **config.toml-Kommentar korrigieren**: `0006` → `06` für den Längen-Byte in der WFD-IE-Erklärung.

6. **Status-Monitor entkoppeln**: `nicocast_status.py` auf Health-Endpunkt-Polling (`http://localhost:8080/health`) umstellen, um Abhängigkeit von Log-Format zu eliminieren. Zusätzlich Log-Rotation berücksichtigen (`watchdog`-Bibliothek oder `tail -F`-Semantik).

7. **Audio-Pipeline dokumentieren**: Im `video.rs`-Modul und in der README klar festhalten, dass Audio bewusst nicht unterstützt wird — verhindert Verwirrung beim Debugging.
