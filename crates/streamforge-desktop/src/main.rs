//! `streamforge-desktop`: the desktop UI. It embeds the engine with the
//! platform capture backend, like the CLI and the headless engine do.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use engine::Engine;
use streamforge_desktop::app::DesktopApp;
use streamforge_desktop::identity;
use streamforge_desktop::model::Model;
use streamforge_desktop::worker::EngineWorker;

const USAGE: &str = "usage: streamforge-desktop [--output-dir DIR] [--project-dir DIR]";

fn main() -> ExitCode {
    // Where recordings and project files go, as `--output-dir` and
    // `--project-dir` for the CLI and the engine; both default to the
    // working directory.
    let mut output_dir = None;
    let mut project_dir = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(flag) = args.next() {
        let slot = if flag == "--output-dir" {
            &mut output_dir
        } else if flag == "--project-dir" {
            &mut project_dir
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
    let output_dir = output_dir.unwrap_or_else(|| PathBuf::from("."));
    let project_dir = project_dir.unwrap_or_else(|| PathBuf::from("."));
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
    let project_folder = std::path::absolute(&project_dir)
        .unwrap_or_else(|_| project_dir.clone())
        .display()
        .to_string();
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
                    Engine::with_capture(capture_platform::backend())
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
        eprintln!("streamforge-desktop: quitting; signal again to quit at once");
        ctx.request_repaint();
    });
    if let Err(err) = installed {
        eprintln!("streamforge-desktop: cannot handle quit signals: {err}");
    }
    signal
}
