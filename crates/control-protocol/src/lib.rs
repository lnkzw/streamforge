//! Local JSON-lines control protocol, version 1.
//!
//! One UTF-8 JSON object per line in each direction. Requests carry a
//! client-chosen `id` that is echoed in the matching response. The normative
//! description lives in `docs/protocol.md`.
//!
//! This crate only translates between bytes and [`command_api`] types. It holds
//! no engine state and drives whatever [`command_api::CommandHandler`] it is
//! given, so the same engine can be served over stdio now and a socket later.

mod message;
mod server;

pub use message::{ErrorCode, RequestId, Response, ResponseError};
pub use server::{handle_line, serve};

/// Protocol version sent in every response as `v`.
pub const PROTOCOL_VERSION: u64 = 1;

/// Maximum request line length in bytes, excluding the terminating `\n`.
///
/// Longer lines are discarded without buffering them and answered with
/// [`ErrorCode::RequestTooLarge`].
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
