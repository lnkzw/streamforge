//! M7 acceptance: through the UI model (the layer the widgets call), create
//! a scene, add a source and record; no widget, no business logic here.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use engine::Engine;
use streamforge_desktop::model::{Level, Model, SourceForm, SourceType};
use streamforge_desktop::worker::EngineWorker;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("streamforge-desktop-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn model(dir: &TempDir) -> Model {
    let dir = dir.0.clone();
    let worker =
        EngineWorker::spawn(move || Engine::new().with_output_dir(dir), || {}).expect("worker");
    Model::new(worker)
}

/// Apply responses until `done` holds (bounded).
fn wait(model: &mut Model, what: &str, done: impl Fn(&Model) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        model.update();
        if done(model) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}; log: {:?}", model.log);
}

fn errors(model: &Model) -> Vec<&str> {
    model
        .log
        .iter()
        .filter(|entry| entry.level == Level::Error)
        .map(|entry| entry.text.as_str())
        .collect()
}

#[test]
fn ui_creates_a_scene_adds_a_source_and_records() {
    let dir = TempDir::new("acceptance");
    let mut model = model(&dir);
    wait(&mut model, "the initial scene list", |m| m.scenes.is_some());

    model.create_scene("Show");
    wait(&mut model, "the new scene", |m| {
        m.scenes
            .as_ref()
            .is_some_and(|list| list.scenes.iter().any(|scene| scene.name == "Show"))
    });
    model.add_source(
        "Show",
        &SourceForm {
            name: "red".to_owned(),
            kind: SourceType::Solid,
            color: [255, 0, 0, 255],
            rect: None,
            opacity: 1.0,
        },
    );
    wait(&mut model, "the new source", |m| {
        m.scenes.as_ref().is_some_and(|list| {
            list.scenes
                .iter()
                .any(|scene| scene.name == "Show" && scene.sources.len() == 1)
        })
    });
    model.activate_scene("Show");
    wait(&mut model, "the scene to be active", |m| {
        m.scenes.as_ref().is_some_and(|list| list.active == "Show")
    });

    {
        model.start_recording("ui.mkv");
        wait(&mut model, "the recording to start", |m| {
            m.recording.is_some()
        });
        wait(&mut model, "the status to show it", |m| {
            m.status
                .as_ref()
                .is_some_and(|status| status.recording_active)
        });
        thread::sleep(Duration::from_millis(500));
        model.stop_recording();
        wait(&mut model, "the recording to finish", |m| {
            m.recording.is_none()
                && m.log
                    .iter()
                    .any(|entry| entry.text.starts_with("Recording saved"))
        });
        let bytes = std::fs::metadata(dir.0.join("ui.mkv"))
            .expect("recorded file")
            .len();
        assert!(bytes > 1_000, "{bytes} bytes");
    }
    assert!(errors(&model).is_empty(), "{:?}", model.log);
}

#[test]
fn engine_errors_reach_the_log() {
    let dir = TempDir::new("errors");
    let mut model = model(&dir);
    model.create_scene("Twice");
    model.create_scene("Twice");
    wait(&mut model, "the duplicate to be rejected", |m| {
        errors(m).iter().any(|text| text.contains("already exists"))
    });
    // The active scene cannot be deleted, even when confirmed.
    model.delete_scene_confirmed("Test pattern");
    wait(&mut model, "the delete to be refused", |m| {
        errors(m)
            .iter()
            .any(|text| text.starts_with("delete_scene") && text.contains("active"))
    });
}

#[test]
fn preview_frames_reach_the_ui() {
    let dir = TempDir::new("preview");
    let mut model = model(&dir);
    model.set_preview(true);
    wait(&mut model, "the preview to start", |m| m.preview_enabled);
    let frames = std::sync::Arc::clone(model.preview_frames());
    let deadline = Instant::now() + Duration::from_secs(5);
    let frame = loop {
        if let Some(frame) = frames.take() {
            break frame;
        }
        assert!(Instant::now() < deadline, "no preview frame");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!((frame.width(), frame.height()), (480, 270));
    // Levels flow while the pipeline runs (polled, as the UI does).
    let deadline = Instant::now() + Duration::from_secs(10);
    while !model
        .mixer
        .as_ref()
        .is_some_and(|mixer| mixer.running && mixer.strips[0].peak_dbfs.is_some())
    {
        assert!(
            Instant::now() < deadline,
            "no live levels; log: {:?}",
            model.log
        );
        model.refresh_live();
        model.update();
        thread::sleep(Duration::from_millis(20));
    }
    model.set_preview(false);
    wait(&mut model, "the preview to stop", |m| !m.preview_enabled);
}
