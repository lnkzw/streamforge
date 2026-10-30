//! `run_with_main_run_loop` called on the main thread, as the programs
//! call it. The default test harness runs tests on other threads, so this
//! test has its own `main` (`harness = false`). A watchdog fails it
//! instead of hanging when the main run loop never stops.

use std::env;
use std::panic::{self, AssertUnwindSafe};
use std::process::{self, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use capture_platform::run_with_main_run_loop;

const DEADLINE: Duration = Duration::from_secs(60);

fn main() {
    // A child process runs one program's way out (see `exits_as_before`).
    if let Some(mode) = env::args().nth(1).filter(|arg| arg.starts_with("child-")) {
        child(&mode);
    }
    thread::spawn(|| {
        thread::sleep(DEADLINE);
        eprintln!("main run loop: timed out (the main run loop did not stop?)");
        process::exit(1);
    });
    the_result_comes_back_from_the_work();
    a_panic_in_the_work_continues_on_the_main_thread();
    #[cfg(target_os = "macos")]
    a_thread_local_may_use_the_main_queue_as_the_work_ends();
    another_thread_runs_the_work_itself();
    exits_as_before();
    println!("main run loop: ok");
}

/// The thread the work runs on: its own on macOS, which keeps the main
/// thread for the run loop; elsewhere the main thread.
const WORK_THREAD: &str = if cfg!(target_os = "macos") {
    "work"
} else {
    "main"
};

/// On macOS, that the main queue runs, on the main thread, while the work
/// waits for it: what the main run loop is for.
fn through_the_main_queue() -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::sync::mpsc;

        let (sender, receiver) = mpsc::channel();
        dispatch2::DispatchQueue::main().exec_async(move || {
            let _ = sender.send(thread::current().name() == Some("main"));
        });
        receiver.recv_timeout(Duration::from_secs(5)) == Ok(true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

fn the_result_comes_back_from_the_work() {
    // Twice, each time through the main queue: the run loop stops after
    // the work and runs again for the next.
    for code in [3_u8, 4] {
        let (result, thread, served) = run_with_main_run_loop(move || {
            let served = through_the_main_queue();
            (code, thread::current().name().map(str::to_owned), served)
        });
        assert_eq!(result, code);
        assert_eq!(thread.as_deref(), Some(WORK_THREAD));
        assert!(served, "the main queue ran while the work waited");
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

/// The work thread's thread-local destructors run after the work, and may
/// wait for the main queue: the run loop keeps running for them.
#[cfg(target_os = "macos")]
fn a_thread_local_may_use_the_main_queue_as_the_work_ends() {
    struct UsesTheMainQueue;

    impl Drop for UsesTheMainQueue {
        fn drop(&mut self) {
            thread::sleep(Duration::from_millis(50));
            dispatch2::DispatchQueue::main().exec_sync(|| {});
        }
    }

    thread_local! {
        static LOCAL: UsesTheMainQueue = const { UsesTheMainQueue };
    }
    run_with_main_run_loop(|| LOCAL.with(|_| ()));
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

/// How the programs end through it, each in a child process: `exit`
/// from the work (as clap's errors do) and a panic in the work keep
/// their status and the output written before.
fn exits_as_before() {
    for (mode, status, stdout) in [
        ("child-exit", 7, "written"),
        ("child-panic", 101, "written\n"),
    ] {
        let child = Command::new(env::current_exe().expect("test binary"))
            .arg(mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the child");
        let output = wait(child, mode);
        assert_eq!(output.status.code(), Some(status), "{mode}: {output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), stdout, "{mode}");
    }
}

fn wait(mut child: process::Child, mode: &str) -> process::Output {
    let started = Instant::now();
    while child.try_wait().expect("wait for the child").is_none() {
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("{mode}: the child did not end");
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().expect("the child's output")
}

fn child(mode: &str) -> ! {
    match mode {
        "child-exit" => run_with_main_run_loop(|| {
            print!("written");
            process::exit(7);
        }),
        "child-panic" => {
            panic::set_hook(Box::new(|_| {}));
            run_with_main_run_loop(|| {
                println!("written");
                panic!("the work failed");
            });
        }
        _ => panic!("unknown child mode {mode}"),
    }
    // Only if the work's end did not end the process.
    process::exit(0)
}
