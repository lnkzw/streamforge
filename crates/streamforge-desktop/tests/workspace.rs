//! The production workspace under simulated input (egui_kittest), on the
//! real model and engine: what the operator clicks reaches the engine, and
//! consequential actions ask first. No rendering is involved, so these run
//! everywhere.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui::accesskit::{Role, Toggled};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use engine::Engine;
use streamforge_desktop::app::DesktopApp;
use streamforge_desktop::model::{Model, SourceForm, SourceType};
use streamforge_desktop::worker::EngineWorker;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "streamforge-desktop-workspace-{}-{name}",
            std::process::id()
        ));
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

/// A model on a fresh engine whose program scene has one source.
fn model(dir: &TempDir) -> Model {
    let out = dir.0.clone();
    let worker =
        EngineWorker::spawn(move || Engine::new().with_output_dir(out), || {}).expect("worker");
    let mut model = Model::new(worker);
    wait(&mut model, "the scene list", |m| m.scenes.is_some());
    let program = model.scenes.as_ref().expect("scenes").active_id.clone();
    model.add_source(
        &program,
        &SourceForm {
            name: "lower third".to_owned(),
            kind: SourceType::Solid,
            color: [220, 60, 40, 255],
            rect: Some([80.0, 860.0, 900.0, 120.0]),
            opacity: 1.0,
            screen_source: None,
        },
    );
    wait(&mut model, "the source", |m| {
        m.scenes.as_ref().is_some_and(|list| {
            list.scenes
                .iter()
                .any(|scene| scene.sources.iter().any(|s| s.name == "lower third"))
        })
    });
    model
}

fn harness(model: Model) -> Harness<'static, DesktopApp> {
    let mut harness = Harness::builder()
        .with_size([1400.0, 900.0])
        .build_eframe(move |cc| DesktopApp::new(&cc.egui_ctx, model));
    // Fonts apply from the second pass; let the first polls answer.
    settle(&mut harness, 5);
    harness
}

fn settle(harness: &mut Harness<'static, DesktopApp>, frames: usize) {
    for _ in 0..frames {
        harness.step();
        thread::sleep(Duration::from_millis(10));
    }
}

/// Whether a button with `label` is shown and can be clicked. A transport
/// button reads "Stop recording" while its start still waits for the
/// engine, but cannot be clicked until the start is answered.
fn clickable(harness: &Harness<'static, DesktopApp>, label: &str) -> bool {
    harness
        .query_by_label(label)
        .is_some_and(|node| !node.accesskit_node().is_disabled())
}

/// Step until `done` holds for the UI (bounded): the engine answers on
/// its own thread. A few more frames follow, as a person needs: a dialog
/// is measured in its first frame and placed in the next.
fn step_until(
    harness: &mut Harness<'static, DesktopApp>,
    what: &str,
    done: impl Fn(&Harness<'static, DesktopApp>) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        harness.step();
        if done(harness) {
            settle(harness, 3);
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

/// Whether the switch or choice labelled `label` shows as on: the UI
/// draws it from the engine's answer, so this is the engine's state.
fn is_on(harness: &Harness<'static, DesktopApp>, label: &str) -> bool {
    harness
        .query_by_label(label)
        .is_some_and(|node| node.accesskit_node().toggled() == Some(Toggled::True))
}

/// Open the routing matrix and add a bus through its dialog.
fn add_bus(harness: &mut Harness<'static, DesktopApp>, name: &str, assigned: bool) {
    if harness.query_by_label("Add a bus").is_none() {
        harness.get_by_label("Routing").click();
    }
    step_until(harness, "the matrix", |h| clickable(h, "Add a bus"));
    harness.get_by_label("Add a bus").click();
    step_until(harness, "the bus dialog", |h| {
        h.query_by_label("New bus").is_some()
    });
    harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("the name field")
        .focus();
    harness.step();
    harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("the name field")
        .type_text(name);
    harness.step();
    if assigned {
        harness
            .get_by_role_and_label(Role::CheckBox, "Feed the program bus")
            .click();
        harness.step();
    }
    harness.get_by_label("Add bus").click();
    let send = format!("Send lower third to {name}");
    let column = format!("Remove bus {name}…");
    step_until(harness, "the new bus", |h| {
        h.query_by_label(&column).is_some() && h.query_by_label("New bus").is_none()
    });
    assert!(
        harness.query_by_label(&send).is_none(),
        "sends are per channel"
    );
}

#[test]
fn selecting_a_source_opens_it_in_the_inspector() {
    let dir = TempDir::new("inspector");
    let mut harness = harness(model(&dir));
    assert!(harness.query_by_label("Remove source…").is_none());
    harness.get_by_label("lower third").click();
    step_until(&mut harness, "the inspector", |h| {
        h.query_by_label("Remove source…").is_some()
    });
}

#[test]
fn dragging_a_position_in_the_inspector_moves_the_source() {
    let dir = TempDir::new("drag");
    let mut harness = harness(model(&dir));
    harness.get_by_label("lower third").click();
    step_until(&mut harness, "the inspector", |h| {
        h.query_by_label("Remove source…").is_some()
    });
    // X is the first number field; drag it 50 px to the right.
    let x = harness
        .get_all_by_role(Role::SpinButton)
        .next()
        .expect("the X field")
        .rect()
        .center();
    harness.hover_at(x);
    harness.step();
    harness.drag_at(x);
    harness.step();
    for step in 1..=5 {
        harness.hover_at(x + egui::vec2(10.0 * step as f32, 0.0));
        harness.step();
    }
    harness.drop_at(x + egui::vec2(50.0, 0.0));
    // The engine takes the dragged value; the field shows its answer.
    step_until(&mut harness, "the moved source", |h| {
        h.get_all_by_role(Role::SpinButton)
            .next()
            .and_then(|field| field.value())
            .is_some_and(|value| value.trim() == "130")
    });
}

#[test]
fn stopping_a_recording_asks_first() {
    let dir = TempDir::new("stop");
    let mut harness = harness(model(&dir));
    harness.get_by_label("Start recording").click();
    step_until(&mut harness, "the recording", |h| {
        clickable(h, "Stop recording")
    });

    // The transport asks; cancelling keeps the recording.
    harness.get_by_label("Stop recording").click();
    step_until(&mut harness, "the confirmation", |h| {
        h.query_by_label("Stop recording?").is_some()
    });
    harness.get_by_label("Cancel").click();
    step_until(&mut harness, "the confirmation to close", |h| {
        h.query_by_label("Stop recording?").is_none()
    });
    settle(&mut harness, 10);
    assert!(
        harness.query_by_label("Start recording").is_none(),
        "cancelled, so still recording"
    );

    // Confirming stops it: the dialog's button, not the transport's.
    harness.get_by_label("Stop recording").click();
    step_until(&mut harness, "the confirmation", |h| {
        h.query_by_label("Stop recording?").is_some()
    });
    harness
        .get_all_by_label("Stop recording")
        .last()
        .expect("the dialog's action")
        .click();
    step_until(&mut harness, "the recording to stop", |h| {
        clickable(h, "Start recording")
    });
    let files: Vec<_> = std::fs::read_dir(&dir.0)
        .expect("output dir")
        .flatten()
        .filter(|entry| entry.file_name() == "recording.mkv")
        .collect();
    assert_eq!(files.len(), 1, "the recording was written");
}

#[test]
fn going_live_without_a_target_asks_for_one() {
    let dir = TempDir::new("live");
    let mut harness = harness(model(&dir));
    harness.get_by_label("Go live").click();
    step_until(&mut harness, "the stream settings", |h| {
        h.query_by_label("Server URL").is_some()
    });
    // A URL the engine would refuse is flagged as it is typed.
    let url = harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("the URL field");
    url.focus();
    harness.step();
    harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("the URL field")
        .type_text("http://example.com/app");
    step_until(&mut harness, "the URL check", |h| {
        h.query_by_label_contains("must start with rtmp://")
            .is_some()
    });
    // Cancelling starts nothing.
    harness.get_by_label("Cancel").click();
    step_until(&mut harness, "the dialog to close", |h| {
        h.query_by_label("Server URL").is_none()
    });
    assert!(harness.query_by_label("End the stream").is_none());
}

#[test]
fn a_send_to_a_new_bus_is_switched_tapped_and_removed_with_it() {
    let dir = TempDir::new("routing");
    let mut harness = harness(model(&dir));
    add_bus(&mut harness, "Voice", true);
    assert!(
        !is_on(&harness, "Send Tone to Voice"),
        "a new bus starts off"
    );
    assert!(is_on(&harness, "Feed Voice to the program bus"));

    harness.get_by_label("Send Tone to Voice").click();
    step_until(&mut harness, "the send", |h| is_on(h, "Send Tone to Voice"));
    // Tone now reaches program directly and through Voice.
    step_until(&mut harness, "the warning", |h| {
        h.query_by_label("1 routing warning").is_some()
    });
    assert!(
        harness
            .query_by_label("Level of Tone to Voice (dB)")
            .is_some()
    );

    // Post-fader by default; a click on the tap makes it pre-fader.
    harness
        .get_by_label("Tone to Voice: after the fader; click for before it")
        .click();
    step_until(&mut harness, "the tap", |h| {
        h.query_by_label("Tone to Voice: before the fader; click for after it")
            .is_some()
    });

    // A separate mix does not reach program: the warning goes.
    harness
        .get_by_label("Feed Voice to the program bus")
        .click();
    step_until(&mut harness, "the separate mix", |h| {
        h.query_by_label("1 routing warning").is_none()
            && !is_on(h, "Feed Voice to the program bus")
    });

    // Removing asks first, and takes the sends with it.
    harness.get_by_label("Remove bus Voice…").click();
    step_until(&mut harness, "the confirmation", |h| {
        h.query_by_label("Remove bus “Voice”?").is_some()
    });
    harness.get_by_label("Remove bus").click();
    step_until(&mut harness, "the bus to go", |h| {
        h.query_by_label("Send Tone to Voice").is_none()
    });
}

#[test]
fn output_mixes_tracks_and_the_monitor_reach_the_engine() {
    let dir = TempDir::new("outputs");
    let mut harness = harness(model(&dir));
    add_bus(&mut harness, "Headphones", false);
    assert!(is_on(&harness, "Monitor takes Program"), "the default mix");

    harness.get_by_label("Monitor takes Headphones").click();
    step_until(&mut harness, "the monitor mix", |h| {
        is_on(h, "Monitor takes Headphones") && !is_on(h, "Monitor takes Program")
    });
    harness.get_by_label("Monitor on").click();
    step_until(&mut harness, "the monitor", |h| is_on(h, "Monitor on"));

    harness
        .get_by_label("Record Headphones as a further track")
        .click();
    step_until(&mut harness, "the track", |h| {
        h.query_by_label("Headphones is track 2; click to stop recording it as a track")
            .is_some()
    });
    // A bus is the mix or a track: taking it as the mix drops the track.
    harness.get_by_label("Record takes Headphones").click();
    step_until(&mut harness, "the recording's mix", |h| {
        is_on(h, "Record takes Headphones")
            && h.query_by_label("Record Program as a further track")
                .is_some()
    });
}

#[test]
fn a_bus_strip_mutes_and_buses_stay_while_the_pipeline_runs() {
    let dir = TempDir::new("console");
    let mut harness = harness(model(&dir));
    add_bus(&mut harness, "Voice", true);
    harness.get_by_label("Console").click();
    step_until(&mut harness, "the console", |h| {
        h.query_by_label("Mute Voice").is_some()
    });
    harness.get_by_label("Mute Voice").click();
    step_until(&mut harness, "the mute", |h| is_on(h, "Mute Voice"));

    // Structure changes need an idle pipeline (ADR 0017).
    harness
        .get_by_role_and_label(Role::CheckBox, "Preview")
        .click();
    step_until(&mut harness, "the preview", |h| {
        h.query_by_role_and_label(Role::CheckBox, "Preview")
            .is_some_and(|node| node.accesskit_node().toggled() == Some(Toggled::True))
    });
    harness.get_by_label("Routing").click();
    step_until(&mut harness, "the matrix", |h| {
        h.query_by_label("Add a bus").is_some()
    });
    step_until(&mut harness, "the running mixer", |h| {
        !clickable(h, "Add a bus") && !clickable(h, "Remove bus Voice…")
    });
}
