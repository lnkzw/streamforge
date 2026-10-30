//! The main run loop for programs without a UI framework (ADR 0020).
//!
//! AppKit-free programs (the CLI, the engine process, the MCP server)
//! would otherwise never run the main thread's run loop, so nothing
//! scheduled on the main dispatch queue or the main run loop (such as
//! ScreenCaptureKit's picker) would ever run. Their work
//! moves to a thread of its own while the main thread runs the loop.

use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_core_foundation::CFRunLoop;

/// The work thread's stack: what macOS gives the main thread, where the
/// work ran before.
const STACK_BYTES: usize = 8 << 20;

/// Runs `work` on a thread of its own while the calling thread, the main
/// thread, runs its run loop, and returns `work`'s result once it ends;
/// a panic in `work` continues on the calling thread. Called on any other
/// thread, which has no main run loop to run, it runs `work` there.
pub fn run_with_main_run_loop<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    if MainThreadMarker::new().is_none() {
        return work();
    }
    let finished = AtomicBool::new(false);
    let joined = thread::scope(|scope| {
        let worker = thread::Builder::new()
            .name("work".to_owned())
            .stack_size(STACK_BYTES)
            .spawn_scoped(scope, || {
                let _stop = StopMainRunLoop(&finished);
                work()
            })
            .unwrap_or_else(|err| panic!("cannot start the work thread: {err}"));
        // The main dispatch queue keeps the main run loop's default mode
        // from being empty, so `run` returns only when stopped: by
        // `StopMainRunLoop`, or by anything else, which the flag tells
        // apart.
        while !finished.load(Ordering::Acquire) {
            CFRunLoop::run();
        }
        worker.join()
    });
    joined.unwrap_or_else(|payload| panic::resume_unwind(payload))
}

/// Stops the main run loop when the work ends, also by a panic. The stop
/// goes through the main queue, which runs only inside the loop: a stop
/// sent before the loop runs would be lost.
struct StopMainRunLoop<'a>(&'a AtomicBool);

impl Drop for StopMainRunLoop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
        DispatchQueue::main().exec_async(|| {
            if let Some(main) = CFRunLoop::main() {
                main.stop();
            }
        });
    }
}
