//! The engine on a dedicated thread, for adapters that serve requests from
//! other threads (e.g. an async MCP server). Capture backends need not be
//! `Send`, so the engine is built on its thread and never moves.

use std::io;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::{self, JoinHandle};

use command_api::{Command, CommandError, CommandHandler, CommandOutput, ErrorCode};

use crate::Engine;

/// Commands waiting for the engine; callers block beyond this.
const QUEUE: usize = 32;

type Reply = SyncSender<Result<CommandOutput, CommandError>>;

/// Owns the engine thread; dropping it lets queued commands finish, then
/// drops the engine (finalizing open outputs) and joins the thread.
pub struct EngineThread {
    handle: Option<EngineHandle>,
    thread: Option<JoinHandle<()>>,
}

/// Cloneable access to the engine thread.
#[derive(Clone)]
pub struct EngineHandle {
    requests: SyncSender<(Command, Reply)>,
}

impl EngineThread {
    pub fn spawn(make_engine: impl FnOnce() -> Engine + Send + 'static) -> io::Result<Self> {
        let (requests, queue) = sync_channel::<(Command, Reply)>(QUEUE);
        let thread = thread::Builder::new()
            .name("engine".to_owned())
            .spawn(move || {
                let mut engine = make_engine();
                for (command, reply) in queue {
                    // The caller may have given up; nothing to do then.
                    let _ = reply.send(engine.handle(command));
                }
            })?;
        Ok(Self {
            handle: Some(EngineHandle { requests }),
            thread: Some(thread),
        })
    }

    pub fn handle(&self) -> EngineHandle {
        self.handle
            .clone()
            .expect("the handle exists until the thread is dropped")
    }
}

impl Drop for EngineThread {
    fn drop(&mut self) {
        // The thread ends once every handle (this one and the clones) is gone.
        self.handle = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl EngineHandle {
    /// Run `command` on the engine thread and wait for its result. Blocks;
    /// call it from a thread that may block (e.g. `spawn_blocking`).
    pub fn call(&self, command: Command) -> Result<CommandOutput, CommandError> {
        let (reply, result) = sync_channel(1);
        let gone = || CommandError::new(ErrorCode::Internal, "the engine thread has stopped");
        self.requests.send((command, reply)).map_err(|_| gone())?;
        result.recv().map_err(|_| gone())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_run_on_the_engine_thread() {
        let thread = EngineThread::spawn(Engine::new).expect("spawn");
        let handle = thread.handle();
        let calls: Vec<_> = (0..4)
            .map(|_| {
                let handle = handle.clone();
                std::thread::spawn(move || handle.call(Command::GetStatus))
            })
            .collect();
        for call in calls {
            assert!(matches!(
                call.join().expect("join"),
                Ok(CommandOutput::Status(_))
            ));
        }
        drop(handle);
        drop(thread);
    }

    #[test]
    fn a_stopped_engine_is_an_internal_error() {
        let thread = EngineThread::spawn(|| panic!("no engine")).expect("spawn");
        let err = thread.handle().call(Command::GetStatus).unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
    }
}
