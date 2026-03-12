//! Persistent, file-based logging to `/var/log/miracast_rs.log`.
//!
//! Combines a rolling file appender (from `tracing-appender`) with a
//! console layer so that all `tracing` macros write to both sinks.

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialise the global tracing subscriber.
///
/// Logs are written to `log_path` **and** to `stderr`.  The returned
/// `WorkerGuard` must be kept alive for the duration of the process;
/// dropping it flushes and closes the background log-writer thread.
///
/// # Log levels
///
/// The level filter honours the `RUST_LOG` environment variable (e.g.
/// `RUST_LOG=debug`).  When absent it defaults to `info`.
pub fn init(log_path: &str) -> Result<WorkerGuard> {
    // Split the log path into directory and file-name components so that
    // tracing-appender can create the file (and its parent directory).
    let (dir, file_name) = split_log_path(log_path);

    let file_appender = tracing_appender::rolling::never(dir, file_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // File layer: JSON-free, full timestamps, no ANSI colours
    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true);

    // Console layer: coloured, human-friendly
    let console_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(true)
        .with_target(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(console_layer)
        .try_init()
        .context("initialising tracing subscriber")?;

    Ok(guard)
}

/// Split a full file path into `(directory, filename)`.
///
/// Falls back to `(".", filename)` when there is no parent component.
fn split_log_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(idx) if idx > 0 => (&path[..idx], &path[idx + 1..]),
        _ => (".", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_absolute_path() {
        let (dir, file) = split_log_path("/var/log/miracast_rs.log");
        assert_eq!(dir, "/var/log");
        assert_eq!(file, "miracast_rs.log");
    }

    #[test]
    fn split_relative_path() {
        let (dir, file) = split_log_path("logs/app.log");
        assert_eq!(dir, "logs");
        assert_eq!(file, "app.log");
    }

    #[test]
    fn split_filename_only() {
        let (dir, file) = split_log_path("app.log");
        assert_eq!(dir, ".");
        assert_eq!(file, "app.log");
    }
}
