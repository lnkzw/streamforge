//! The engine on a dedicated thread, for adapters that serve requests from
//! other threads (an async MCP server, a UI). The engine is built on its
//! thread and never moves, so what it holds need not be `Send`.
//!
//! Commands queue in a bounded channel; a full queue is reported as busy
//! instead of blocking the caller. Shutting down cancels queued commands,
//! lets the running one finish, then drops the engine (which finalizes open
//! recordings and streams).

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use command_api::{Command, CommandError, CommandHandler, CommandOutput, ErrorCode};
use media_core::FrameMailbox;

use crate::Engine;

/// Commands waiting for the engine; beyond this, `submit` reports busy.
const QUEUE: usize = 32;

type Reply = Result<CommandOutput, CommandError>;
type Notify = Box<dyn Fn() + Send + Sync>;

enum Job {
    // Boxed: commands are large and the queue holds up to `QUEUE` jobs.
    Run(Box<Command>, SyncSender<Reply>),
    Shutdown,
}

struct Shared {
    closing: AtomicBool,
    notify: Option<Notify>,
}

/// Owns the engine thread. Dropping it shuts the engine down (see the
/// module docs) even while handles are still around.
pub struct EngineThread {
    handle: EngineHandle,
    preview: Arc<FrameMailbox>,
    thread: Option<JoinHandle<()>>,
}

/// Cloneable access to the engine thread.
#[derive(Clone)]
pub struct EngineHandle {
    jobs: SyncSender<Job>,
    shared: Arc<Shared>,
}

/// The answer to a submitted command, once the engine has run it.
pub struct PendingReply(Receiver<Reply>);

impl EngineThread {
    pub fn spawn(make_engine: impl FnOnce() -> Engine + Send + 'static) -> io::Result<Self> {
        Self::spawn_with_notify(make_engine, None)
    }

    /// Like [`Self::spawn`]; `notify` runs after every reply (e.g. to wake a
    /// UI that polls [`PendingReply::try_take`]).
    pub fn spawn_with_notify(
        make_engine: impl FnOnce() -> Engine + Send + 'static,
        notify: Option<Notify>,
    ) -> io::Result<Self> {
        let (jobs, queue) = sync_channel::<Job>(QUEUE);
        let shared = Arc::new(Shared {
            closing: AtomicBool::new(false),
            notify,
        });
        let (preview_tx, preview_rx) = mpsc::channel();
        let thread = thread::Builder::new().name("engine".to_owned()).spawn({
            let shared = Arc::clone(&shared);
            move || {
                let engine = make_engine();
                if preview_tx.send(engine.preview_frames()).is_ok() {
                    serve(engine, &queue, &shared);
                }
            }
        })?;
        let preview = preview_rx
            .recv()
            .map_err(|_| io::Error::other("the engine failed while starting"))?;
        Ok(Self {
            handle: EngineHandle { jobs, shared },
            preview,
            thread: Some(thread),
        })
    }

    pub fn handle(&self) -> EngineHandle {
        self.handle.clone()
    }

    /// See [`Engine::preview_frames`].
    pub fn preview_frames(&self) -> Arc<FrameMailbox> {
        Arc::clone(&self.preview)
    }
}

impl Drop for EngineThread {
    fn drop(&mut self) {
        self.handle.shared.closing.store(true, Ordering::Release);
        // Queued jobs are answered at once now, so this send cannot wait
        // longer than the command that is running.
        let _ = self.handle.jobs.send(Job::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut engine: Engine, queue: &Receiver<Job>, shared: &Shared) {
    let mut panicked: Option<String> = None;
    for job in queue {
        let Job::Run(command, reply) = job else {
            break;
        };
        let result = if shared.closing.load(Ordering::Acquire) {
            Err(CommandError::new(
                ErrorCode::Cancelled,
                "the engine is shutting down; the command did not run",
            ))
        } else if let Some(message) = &panicked {
            Err(CommandError::new(
                ErrorCode::Internal,
                format!("the engine stopped after an internal failure: {message}"),
            ))
        } else {
            match panic::catch_unwind(AssertUnwindSafe(|| engine.handle(*command))) {
                Ok(result) => result,
                Err(payload) => {
                    let message = panic_message(payload.as_ref());
                    panicked = Some(message.clone());
                    Err(CommandError::new(
                        ErrorCode::Internal,
                        format!("the engine failed: {message}"),
                    ))
                }
            }
        };
        // The caller may have given up; nothing to do then.
        let _ = reply.send(result);
        if let Some(notify) = &shared.notify {
            notify();
        }
    }
    // Dropping the engine finalizes open outputs.
    drop(engine);
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".to_owned())
}

fn stopped() -> CommandError {
    CommandError::new(ErrorCode::Internal, "the engine thread has stopped")
}

impl EngineHandle {
    /// Queue `command`; never blocks. A full queue is `unavailable` (busy).
    pub fn submit(&self, command: Command) -> Result<PendingReply, CommandError> {
        if self.shared.closing.load(Ordering::Acquire) {
            return Err(CommandError::new(
                ErrorCode::Cancelled,
                "the engine is shutting down",
            ));
        }
        let (reply, answer) = sync_channel(1);
        match self.jobs.try_send(Job::Run(Box::new(command), reply)) {
            Ok(()) => Ok(PendingReply(answer)),
            Err(TrySendError::Full(_)) => Err(CommandError::new(
                ErrorCode::Unavailable,
                format!("the engine is busy ({QUEUE} commands waiting); try again later"),
            )),
            Err(TrySendError::Disconnected(_)) => Err(stopped()),
        }
    }

    /// Submit and wait for the reply. Blocks; call it from a thread that
    /// may block (e.g. `spawn_blocking`).
    pub fn call(&self, command: Command) -> Reply {
        self.submit(command)?.wait()
    }
}

impl PendingReply {
    pub fn wait(self) -> Reply {
        self.0.recv().unwrap_or_else(|_| Err(stopped()))
    }

    /// The reply if it has arrived; never blocks.
    pub fn try_take(&self) -> Option<Reply> {
        match self.0.try_recv() {
            Ok(reply) => Some(reply),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(stopped())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

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
    }

    #[test]
    fn shutdown_cancels_queued_commands_and_outlives_handles() {
        let thread = EngineThread::spawn(Engine::new).expect("spawn");
        let handle = thread.handle();
        // A command that takes a while (a tone-only mix needs no device).
        let slow =
            Command::decode("probe_audio_mix", Some(&json!({"duration_ms": 400}))).expect("decode");
        let running = handle.submit(slow).expect("submit");
        std::thread::sleep(Duration::from_millis(50));
        let queued: Vec<_> = (0..3)
            .map(|_| handle.submit(Command::GetStatus).expect("submit"))
            .collect();
        // A handle that outlives the thread does not block the shutdown.
        drop(thread);
        assert!(running.wait().is_ok());
        for reply in queued {
            assert_eq!(reply.wait().unwrap_err().code, ErrorCode::Cancelled);
        }
        assert!(handle.submit(Command::GetStatus).is_err());
    }

    #[test]
    fn a_failing_engine_start_is_an_error() {
        assert!(EngineThread::spawn(|| panic!("no engine")).is_err());
    }
}
