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
/// thread, runs its run loop, and returns `work`'s result once that
/// thread has ended; a panic in `work` continues on the calling thread.
/// Called on any other thread, which has no main run loop to run, it runs
/// `work` there.
pub fn run_with_main_run_loop<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    if MainThreadMarker::new().is_none() {
        return work();
    }
    let finished = AtomicBool::new(false);
    let joined = thread::scope(|scope| {
        let worker = thread::Builder::new()
            .name("work".to_owned())
            .stack_size(STACK_BYTES)
            .spawn_scoped(scope, work)
            .unwrap_or_else(|err| panic!("cannot start the work thread: {err}"));
        // Joined on a thread of its own, so that the main run loop still
        // runs while the work thread ends: its thread-local destructors may
        // wait for the main queue. Then the stop goes through the main
        // queue too, which runs only inside the loop, so a stop sent before
        // the loop runs is not lost.
        let joiner = thread::Builder::new()
            .name("work-join".to_owned())
            .spawn_scoped(scope, || {
                let joined = worker.join();
                finished.store(true, Ordering::Release);
                DispatchQueue::main().exec_async(|| {
                    if let Some(main) = CFRunLoop::main() {
                        main.stop();
                    }
                });
                joined
            })
            .unwrap_or_else(|err| panic!("cannot start the joining thread: {err}"));
        // The main dispatch queue keeps the main run loop's default mode
        // from being empty, so `run` returns only when stopped: by the
        // joining thread, or by anything else, which the flag tells apart.
        while !finished.load(Ordering::Acquire) {
            CFRunLoop::run();
        }
        joiner.join()
    });
    joined
        .and_then(|joined| joined)
        .unwrap_or_else(|payload| panic::resume_unwind(payload))
}
