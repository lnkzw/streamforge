//! The engine on its own thread ([`EngineThread`]), so the UI thread never
//! blocks on a command (starting a stream can take seconds; screen sources
//! wait for the picker). The UI submits and polls; it never waits.

use std::sync::Arc;

use command_api::{Command, CommandError, CommandOutput, ErrorCode};
use engine::{Engine, EngineHandle, EngineThread, PendingReply};
use media_core::FrameMailbox;
use serde_json::Value;

pub type RequestId = u64;

pub struct Response {
    pub id: RequestId,
    pub name: String,
    pub result: Result<CommandOutput, CommandError>,
}

/// The engine has more commands waiting than its queue holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

pub struct EngineWorker {
    handle: EngineHandle,
    pending: Vec<(RequestId, String, PendingReply)>,
    /// Answers known without the engine (e.g. invalid parameters).
    ready: Vec<Response>,
    next_id: RequestId,
    preview: Arc<FrameMailbox>,
    /// Dropped last: shuts the engine down (queued commands are cancelled,
    /// the running one finishes, open outputs are finalized).
    _thread: EngineThread,
}

impl EngineWorker {
    /// Build the engine with `make_engine` on the `engine` thread. `notify`
    /// runs after every reply (e.g. to ask the UI to repaint).
    pub fn spawn(
        make_engine: impl FnOnce() -> Engine + Send + 'static,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let thread = EngineThread::spawn_with_notify(make_engine, Some(Box::new(notify)))?;
        Ok(Self {
            handle: thread.handle(),
            pending: Vec::new(),
            ready: Vec::new(),
            next_id: 1,
            preview: thread.preview_frames(),
            _thread: thread,
        })
    }

    /// Queue a command (same names and parameters as the JSON-lines
    /// protocol); never blocks.
    pub fn submit(&mut self, name: &str, params: Value) -> Result<RequestId, Busy> {
        let id = self.next_id;
        // Decoding is cheap and pure; validation errors return at once.
        let submitted =
            Command::decode(name, Some(&params)).and_then(|command| self.handle.submit(command));
        match submitted {
            Ok(reply) => self.pending.push((id, name.to_owned(), reply)),
            Err(error) if error.code == ErrorCode::Unavailable => return Err(Busy),
            Err(error) => self.ready.push(Response {
                id,
                name: name.to_owned(),
                result: Err(error),
            }),
        }
        self.next_id += 1;
        Ok(id)
    }

    /// Responses that arrived since the last call; never blocks.
    pub fn poll(&mut self) -> Vec<Response> {
        let mut responses = std::mem::take(&mut self.ready);
        self.pending
            .retain(|(id, name, reply)| match reply.try_take() {
                Some(result) => {
                    responses.push(Response {
                        id: *id,
                        name: name.clone(),
                        result,
                    });
                    false
                }
                None => true,
            });
        responses
    }

    /// Newest preview frame while the preview is enabled.
    pub fn preview(&self) -> &Arc<FrameMailbox> {
        &self.preview
    }
}
