//! In-process RTMP ingest for tests: FFmpeg's RTMP protocol in listen mode
//! accepts one publisher and reports what arrived. Builds wherever the
//! FFmpeg outputs do (Linux, Windows, macOS); test-only, never shipped.
//!
//! FFmpeg's listener accepts any stream name, so keys cannot be checked
//! here; `StreamTarget` unit tests cover how keys are joined.

mod server;

pub use server::{ServerOptions, ServerReport, TestServer, TrackReport, free_port};
