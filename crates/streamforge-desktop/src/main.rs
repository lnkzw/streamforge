//! `streamforge-desktop`: the desktop UI. It embeds the engine with the
//! platform capture backend, like the CLI and the headless engine do.

use std::path::PathBuf;
use std::process::ExitCode;

use engine::Engine;
use streamforge_desktop::app::DesktopApp;
use streamforge_desktop::model::Model;
use streamforge_desktop::worker::EngineWorker;

fn main() -> ExitCode {
    // One optional argument: where recordings go (default: the working
    // directory), as `--output-dir` for the CLI.
    let mut args = std::env::args_os().skip(1);
    let output_dir = match (args.next(), args.next()) {
        (None, _) => PathBuf::from("."),
        (Some(flag), Some(dir)) if flag == "--output-dir" => PathBuf::from(dir),
        _ => {
            eprintln!("usage: streamforge-desktop [--output-dir DIR]");
            return ExitCode::FAILURE;
        }
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("StreamForge")
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
                    Engine::with_capture(capture_platform::backend()).with_output_dir(output_dir)
                },
                move || ctx.request_repaint(),
            )?;
            Ok(Box::new(DesktopApp::new(Model::new(worker))))
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
