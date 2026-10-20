//! In-process RTMP ingest for tests: FFmpeg's RTMP protocol in listen mode
//! accepts one publisher and reports what arrived. Linux only (like the
//! FFmpeg outputs); test-only, never shipped.
//!
//! FFmpeg's listener accepts any stream name, so keys cannot be checked
//! here; `StreamTarget` unit tests cover how keys are joined.

#[cfg(target_os = "linux")]
mod server;

#[cfg(target_os = "linux")]
pub use server::{ServerOptions, ServerReport, TestServer, TrackReport, free_port};
