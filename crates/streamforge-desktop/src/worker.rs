//! The engine on its own thread, so the UI thread never blocks on a command
//! (starting a stream can take seconds; screen sources wait for the picker).

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};

use command_api::{Command, CommandError, CommandHandler, CommandOutput};
use engine::Engine;
use media_core::FrameMailbox;
use serde_json::Value;

/// Requests waiting for the engine; beyond this the UI reports "busy"
/// instead of queueing without bound.
const QUEUE: usize = 64;

pub type RequestId = u64;

struct Request {
    id: RequestId,
    name: String,
    params: Value,
}

pub struct Response {
    pub id: RequestId,
    pub name: String,
    pub result: Result<CommandOutput, CommandError>,
}

/// The engine is busy with more requests than the queue holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

pub struct EngineWorker {
    requests: Option<SyncSender<Request>>,
    responses: Option<Receiver<Response>>,
    preview: Arc<FrameMailbox>,
    next_id: RequestId,
    thread: Option<JoinHandle<()>>,
}

impl EngineWorker {
    /// Build the engine with `make_engine` on the `engine` thread (capture
    /// backends need not be `Send`, so the engine never changes threads).
    /// `notify` runs after every response (e.g. to ask the UI to repaint).
    pub fn spawn(
        make_engine: impl FnOnce() -> Engine + Send + 'static,
        notify: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (request_tx, request_rx) = sync_channel::<Request>(QUEUE);
        let (response_tx, response_rx) = sync_channel(QUEUE);
        let (preview_tx, preview_rx) = sync_channel(1);
        let thread = thread::Builder::new()
            .name("engine".to_owned())
            .spawn(move || {
                let mut engine = make_engine();
                if preview_tx.send(engine.preview_frames()).is_err() {
                    return;
                }
                for request in request_rx {
                    // Same path as the CLI and the JSON-lines server: decode
                    // (validation) and the engine's command handler.
                    let result = Command::decode(&request.name, Some(&request.params))
                        .and_then(|command| engine.handle(command));
                    let response = Response {
                        id: request.id,
                        name: request.name,
                        result,
                    };
                    if response_tx.send(response).is_err() {
                        break;
                    }
                    notify();
                }
                // Dropping the engine here finalizes open outputs.
            })?;
        let preview = preview_rx
            .recv()
            .map_err(|_| std::io::Error::other("the engine thread ended while starting"))?;
        Ok(Self {
            requests: Some(request_tx),
            responses: Some(response_rx),
            preview,
            next_id: 1,
            thread: Some(thread),
        })
    }

    /// Queue a command; never blocks.
    pub fn submit(&mut self, name: &str, params: Value) -> Result<RequestId, Busy> {
        let id = self.next_id;
        let request = Request {
            id,
            name: name.to_owned(),
            params,
        };
        match self.requests.as_ref().map(|tx| tx.try_send(request)) {
            Some(Ok(())) => {
                self.next_id += 1;
                Ok(id)
            }
            Some(Err(TrySendError::Full(_) | TrySendError::Disconnected(_))) | None => Err(Busy),
        }
    }

    /// Responses that arrived since the last call; never blocks.
    pub fn poll(&self) -> Vec<Response> {
        self.responses
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default()
    }

    /// Newest preview frame while the preview is enabled.
    pub fn preview(&self) -> &Arc<FrameMailbox> {
        &self.preview
    }
}

impl Drop for EngineWorker {
    fn drop(&mut self) {
        // Closing both channels ends the thread at its next request or
        // response (nobody reads the rest); the engine then finalizes
        // recordings and streams as it is dropped.
        self.requests = None;
        self.responses = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
