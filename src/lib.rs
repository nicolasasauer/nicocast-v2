//! nicocast-v2 library crate.
//!
//! Re-exports all public modules so that integration tests in `tests/`
//! can access them via `nicocast_v2::rtsp`, `nicocast_v2::config`, etc.
//! The binary (`src/main.rs`) compiles its own copy of these modules.

pub mod airplay;
pub mod config;
pub mod health;
pub mod logger;
pub mod p2p;
pub mod rtsp;
pub mod video;
