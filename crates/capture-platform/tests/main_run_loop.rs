//! `run_with_main_run_loop` called on the main thread, as the programs
//! call it. The default test harness runs tests on other threads, so this
//! test has its own `main` (`harness = false`).

use std::panic::{self, AssertUnwindSafe};
use std::thread;

use capture_platform::run_with_main_run_loop;

fn main() {
    the_result_comes_back_from_the_work();
    a_panic_in_the_work_continues_on_the_main_thread();
    #[cfg(target_os = "macos")]
    the_main_queue_runs_while_the_work_runs();
    another_thread_runs_the_work_itself();
    println!("main run loop: ok");
}

/// The thread the work runs on: its own on macOS, which keeps the main
/// thread for the run loop; elsewhere the main thread.
const WORK_THREAD: &str = if cfg!(target_os = "macos") {
    "work"
} else {
    "main"
};

fn the_result_comes_back_from_the_work() {
    // Twice: the run loop stops after each work and runs again.
    for code in [3_u8, 4] {
        let (result, thread) =
            run_with_main_run_loop(move || (code, thread::current().name().map(str::to_owned)));
        assert_eq!(result, code);
        assert_eq!(thread.as_deref(), Some(WORK_THREAD));
    }
}

fn a_panic_in_the_work_continues_on_the_main_thread() {
    let hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let caught = panic::catch_unwind(AssertUnwindSafe(|| {
        run_with_main_run_loop(|| -> u8 { panic!("the work failed") })
    }));
    panic::set_hook(hook);
    let payload = caught.expect_err("the panic reaches the caller");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"the work failed"));
    // And the next work runs as usual.
    assert_eq!(run_with_main_run_loop(|| 5_u8), 5);
}

/// What the main run loop is for: work scheduled on the main queue runs,
/// on the main thread, while the program's work waits for it.
#[cfg(target_os = "macos")]
fn the_main_queue_runs_while_the_work_runs() {
    use std::sync::mpsc;
    use std::time::Duration;

    let on_main = run_with_main_run_loop(|| {
        let (sender, receiver) = mpsc::channel();
        dispatch2::DispatchQueue::main().exec_async(move || {
            let _ = sender.send(thread::current().name() == Some("main"));
        });
        receiver.recv_timeout(Duration::from_secs(5))
    });
    assert_eq!(on_main, Ok(true));
}

fn another_thread_runs_the_work_itself() {
    let thread = thread::Builder::new()
        .name("other".to_owned())
        .spawn(|| run_with_main_run_loop(|| thread::current().name().map(str::to_owned)))
        .expect("spawn")
        .join()
        .expect("the work returns");
    assert_eq!(thread.as_deref(), Some("other"));
}
