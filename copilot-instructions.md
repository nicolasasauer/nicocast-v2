# Coding Standards for Miracast Rust Project
- **Rust Edition:** Always use Rust Edition 2024 (or newest available).
- **Async Runtime:** Exclusively use `tokio` (latest version). No `async-std`.
- **D-Bus:** Use `zbus` with its procedural macros; avoid raw `dbus-rs` or manual XML parsing.
- **Error Handling:** Use `anyhow` for applications and `thiserror` for library components.
- **Logging:** Use the `tracing` ecosystem. Prefer structured logging over `println!`.
- **Idioms:** Use modern Rust patterns (e.g., `let-else` statements, `impl Trait` in arguments).
- **Miracast Specs:** Follow the Wi-Fi Display (WFD) Technical Specification v2.1.
