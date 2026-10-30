//! `streamforge-desktop`: the desktop UI. It embeds the engine with the
//! platform capture backend, like the CLI and the headless engine do.

// Windows: a window program, without a console window of its own
// (console.rs).
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use engine::Engine;
use streamforge_desktop::app::DesktopApp;
use streamforge_desktop::console::ParentConsole;
use streamforge_desktop::folder;
use streamforge_desktop::identity;
use streamforge_desktop::model::Model;
use streamforge_desktop::worker::EngineWorker;

const USAGE: &str = "usage: streamforge-desktop [--output-dir DIR] [--project-dir DIR]\n       streamforge-desktop --version | --help";

fn main() -> ExitCode {
    let console = ParentConsole::borrow();
    // Where recordings and project files go, as `--output-dir` and
    // `--project-dir` for the CLI and the engine; both default to the
    // working directory (the home folder when that is the root).
    let mut output_dir = None;
    let mut project_dir = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(flag) = args.next() {
        let slot = if flag == "--output-dir" {
            &mut output_dir
        } else if flag == "--project-dir" {
            &mut project_dir
        } else if flag == "--version" || flag == "-V" {
            println!("streamforge-desktop {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        } else if flag == "--help" || flag == "-h" {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        } else {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        };
        match args.next() {
            Some(dir) if slot.is_none() => *slot = Some(PathBuf::from(dir)),
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    // A home folder that is not one (a stale HOME) would stop the app at
    // the check below, with the message in a stderr nobody sees from the
    // Finder; then the working directory stays.
    let default_dir = folder::default_dir(
        std::env::current_dir().ok().as_deref(),
        std::env::home_dir().filter(|home| home.is_dir()).as_deref(),
    );
    let output_dir = output_dir.unwrap_or_else(|| default_dir.clone());
    let project_dir = project_dir.unwrap_or(default_dir);
    // As the engine does: a mistyped directory stops here, not at the
    // first save.
    if !project_dir.is_dir() {
        eprintln!(
            "streamforge-desktop: --project-dir {} is not a directory",
            project_dir.display()
        );
        return ExitCode::FAILURE;
    }
    // Shown in the project dialogs, so the operator knows where files are
    // (`absolute`, not `canonicalize`: no `\\?\` prefix on Windows).
    let project_folder = std::path::absolute(&project_dir).unwrap_or_else(|_| project_dir.clone());
    // From here on the window is the interface.
    console.give_back();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("StreamForge")
            .with_app_id(identity::APP_ID)
            .with_icon(identity::window_icon())
            .with_inner_size([1400.0, 900.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "StreamForge",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let worker = EngineWorker::spawn(
                move || {
                    Engine::with_capture(capture_platform::capture())
                        .with_output_dir(output_dir)
                        .with_project_dir(project_dir)
                },
                move || ctx.request_repaint(),
            )?;
            Ok(Box::new(
                DesktopApp::new(&cc.egui_ctx, Model::new(worker))
                    .with_project_folder(project_folder)
                    .with_quit_signal(quit_on_signal(cc.egui_ctx.clone())),
            ))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Where the program was started, as the startup errors above.
            let _console = ParentConsole::borrow();
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Ctrl+C or SIGTERM close the window without asking, so running outputs
/// are finalized as at any close; a second one exits at once. Without a
/// handler, a signal ends the program unfinalized, as before.
fn quit_on_signal(ctx: eframe::egui::Context) -> Arc<AtomicBool> {
    let signal = Arc::new(AtomicBool::new(false));
    let set = Arc::clone(&signal);
    let installed = quit_signal::on_quit("streamforge-desktop", move || {
        set.store(true, Ordering::Relaxed);
        ctx.request_repaint();
        let _ = writeln!(
            io::stderr(),
            "streamforge-desktop: quitting; signal again to quit at once"
        );
    });
    if let Err(err) = installed {
        eprintln!("streamforge-desktop: cannot handle quit signals: {err}");
    }
    signal
}
