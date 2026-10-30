//! The main run loop for programs without a UI framework (ADR 0020).
//!
//! AppKit-free programs (the CLI, the engine process, the MCP server)
//! would otherwise never run the main thread's run loop, so nothing
//! scheduled on the main dispatch queue or the main run loop (such as
//! ScreenCaptureKit's picker) would ever run. Their work
//! moves to a thread of its own while the main thread runs the loop.

use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, ScopedJoinHandle};

use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_core_foundation::CFRunLoop;

/// The work thread's stack: what macOS gives the main thread, where the
/// work ran before.
const STACK_BYTES: usize = 8 << 20;

/// Set while [`run_with_main_run_loop`] runs the main run loop.
static RUNS_MAIN_RUN_LOOP: AtomicBool = AtomicBool::new(false);

/// Whether the main run loop runs, or is about to (the work may start
/// before the loop does): this program's, or AppKit's in the desktop.
pub(crate) fn main_run_loop_runs() -> bool {
    RUNS_MAIN_RUN_LOOP.load(Ordering::Acquire)
        || CFRunLoop::main().is_some_and(|main| main.current_mode().is_some())
}

/// Runs `work` on a thread of its own while the calling thread, the main
/// thread, runs its run loop, and returns `work`'s result once that
/// thread has ended; a panic in `work` continues on the calling thread.
/// Called on any other thread, which has no main run loop to run, it runs
/// `work` there.
pub fn run_with_main_run_loop<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    if MainThreadMarker::new().is_none() {
        return work();
    }
    let _runs = RunsMainRunLoop::start();
    let finished = Arc::new(AtomicBool::new(false));
    let outcome = thread::scope(|scope| {
        // Starts the work and joins it, so that the main run loop still
        // runs while the work thread ends (its thread-local destructors may
        // wait for the main queue), and stops the loop afterwards, also
        // when the work thread cannot start or the work panics. Started
        // first: nothing runs yet if it cannot start.
        let stop = StopMainRunLoop(Arc::clone(&finished));
        let supervisor = thread::Builder::new()
            .name("work-join".to_owned())
            .spawn_scoped(scope, move || {
                let _stop = stop;
                thread::Builder::new()
                    .name("work".to_owned())
                    .stack_size(STACK_BYTES)
                    .spawn_scoped(scope, work)
                    .map(ScopedJoinHandle::join)
            })
            .unwrap_or_else(|err| panic!("cannot start the work's supervisor thread: {err}"));
        // The main dispatch queue keeps the main run loop's default mode
        // from being empty, so `run` returns only when stopped: by the
        // supervisor's stop, which sets the flag first, or by anything
        // else, after which the loop runs again.
        while !finished.load(Ordering::Acquire) {
            CFRunLoop::run();
        }
        supervisor.join()
    });
    match outcome {
        Ok(Ok(Ok(value))) => value,
        Ok(Ok(Err(payload))) | Err(payload) => panic::resume_unwind(payload),
        Ok(Err(err)) => panic!("cannot start the work thread: {err}"),
    }
}

/// Marks the main run loop as run by [`run_with_main_run_loop`], from
/// before the work starts until it returns or panics.
struct RunsMainRunLoop;

impl RunsMainRunLoop {
    fn start() -> Self {
        RUNS_MAIN_RUN_LOOP.store(true, Ordering::Release);
        Self
    }
}

impl Drop for RunsMainRunLoop {
    fn drop(&mut self) {
        RUNS_MAIN_RUN_LOOP.store(false, Ordering::Release);
    }
}

/// Stops the main run loop when dropped. The stop goes through the main
/// queue, which runs only inside the loop, so a stop sent before the loop
/// runs is not lost; and it sets the flag the loop checks on the main
/// thread itself, so the loop ends only once the stop has run and none
/// stays queued for a later run loop.
struct StopMainRunLoop(Arc<AtomicBool>);

impl Drop for StopMainRunLoop {
    fn drop(&mut self) {
        let finished = Arc::clone(&self.0);
        DispatchQueue::main().exec_async(move || {
            finished.store(true, Ordering::Release);
            if let Some(main) = CFRunLoop::main() {
                main.stop();
            }
        });
    }
}
