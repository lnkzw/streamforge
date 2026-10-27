//! A raised SIGTERM reaches the handler (one per process, so one test).
#![cfg(unix)]

use std::sync::mpsc;
use std::time::Duration;

use signal_hook::consts::SIGTERM;

#[test]
fn the_first_quit_signal_runs_the_handler() {
    let (quit, quitting) = mpsc::channel();
    quit_signal::on_quit("test", move || {
        let _ = quit.send(());
    })
    .expect("install");
    signal_hook::low_level::raise(SIGTERM).expect("raise");
    quitting
        .recv_timeout(Duration::from_secs(5))
        .expect("the handler ran");
}
