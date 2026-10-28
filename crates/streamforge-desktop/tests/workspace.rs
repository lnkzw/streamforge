//! The production workspace under simulated input (egui_kittest), on the
//! real model and engine: what the operator clicks reaches the engine, and
//! consequential actions ask first. No rendering is involved, so these run
//! everywhere.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui::accesskit::{Role, Toggled};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable, by};
use engine::Engine;
use streamforge_desktop::app::DesktopApp;
use streamforge_desktop::model::{Level, Model, SourceForm, SourceType};
use streamforge_desktop::worker::EngineWorker;
use support::{FRONT_CAMERA, ListedCameras, ListedDevices};

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

/// A model on a fresh engine whose program scene has one source; the
/// engine lists audio inputs and cameras, and captures only the cameras.
fn model(dir: &TempDir) -> Model {
    model_with(dir, ListedCameras::default())
}

/// As [`model`], on `cameras`.
fn model_with(dir: &TempDir, cameras: ListedCameras) -> Model {
    let out = dir.0.clone();
    let worker = EngineWorker::spawn(
        move || {
            Engine::with_capture(capture_api::Capture {
                audio: Box::new(ListedDevices::new()),
                camera: Box::new(cameras),
                ..capture_api::Capture::unsupported("the tests capture nothing")
            })
            .with_output_dir(&out)
            .with_project_dir(out)
        },
        || {},
    )
    .expect("worker");
    let mut model = Model::new(worker);
    wait(&mut model, "the scene list", |m| m.scenes.is_some());
    let program = model
        .scenes
        .as_ref()
        .expect("scenes")
        .active_scene_id
        .clone();
    model.add_source(
        &program,
        &SourceForm {
            name: "lower third".to_owned(),
            kind: SourceType::Solid,
            color: [220, 60, 40, 255],
            rect: Some([80.0, 860.0, 900.0, 120.0]),
            opacity: 1.0,
            screen_source: None,
            image_file: None,
            camera_id: None,
            camera_mode: None,
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
    harness_sized(model, [1400.0, 900.0])
}

fn harness_sized(model: Model, size: [f32; 2]) -> Harness<'static, DesktopApp> {
    let mut harness = Harness::builder()
        .with_size(size)
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
        .query(by().label(label).predicate(|node| node.toggled().is_some()))
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
fn dragging_the_opacity_field_moves_the_slider_with_it() {
    let dir = TempDir::new("opacity");
    let mut harness = harness(model(&dir));
    harness.get_by_label("lower third").click();
    step_until(&mut harness, "the inspector", |h| {
        h.query_by_label("Opacity (%)").is_some()
    });
    let slider = |h: &Harness<'static, DesktopApp>| {
        h.get_by_label("Opacity")
            .accesskit_node()
            .numeric_value()
            .expect("a value")
    };
    assert_eq!(slider(&harness), 100.0);
    let field = harness.get_by_label("Opacity (%)").rect().center();
    harness.hover_at(field);
    harness.step();
    harness.drag_at(field);
    harness.step();
    for step in 1..=4 {
        harness.hover_at(field - egui::vec2(10.0 * step as f32, 0.0));
        harness.step();
    }
    // Mid-drag nothing is sent yet, but the slider shows the field's value.
    let dragged = slider(&harness);
    assert!(dragged < 100.0, "the slider follows the drag: {dragged}");
    assert_eq!(
        harness
            .get_by_label("Opacity (%)")
            .value()
            .as_deref()
            .map(str::trim),
        Some(format!("{dragged:.0} %").as_str())
    );
    harness.drop_at(field - egui::vec2(40.0, 0.0));
    step_until(&mut harness, "the engine's opacity", |h| slider(h) < 100.0);
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
    assert_eq!(recordings(&dir).len(), 1, "the recording was written");
}

/// The automatically named recordings in the output folder.
fn recordings(dir: &TempDir) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(&dir.0)
        .expect("output dir")
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("recording-") && name.ends_with(".mkv"))
        .collect();
    names.sort();
    names
}

/// Stop the running recording through its confirmation.
fn stop_recording(harness: &mut Harness<'static, DesktopApp>) {
    harness.get_by_label("Stop recording").click();
    step_until(harness, "the confirmation", |h| {
        h.query_by_label("Stop recording?").is_some()
    });
    harness
        .get_all_by_label("Stop recording")
        .last()
        .expect("the dialog's action")
        .click();
    step_until(harness, "the recording to stop", |h| {
        clickable(h, "Start recording")
    });
}

#[test]
fn every_recording_gets_a_new_file_named_by_its_start() {
    let dir = TempDir::new("takes");
    let mut harness = harness(model(&dir));
    // Two takes, most likely within the same second.
    for _ in 0..2 {
        harness.get_by_label("Start recording").click();
        step_until(&mut harness, "the recording", |h| {
            clickable(h, "Stop recording")
        });
        stop_recording(&mut harness);
    }
    let names = recordings(&dir);
    assert_eq!(names.len(), 2, "{names:?}");
    // E.g. recording-2026-10-27_15-30-05.mkv, then _2 within that second.
    let stamp = names[0]
        .strip_prefix("recording-")
        .and_then(|name| name.strip_suffix(".mkv"))
        .expect("the pattern");
    assert_eq!(stamp.len(), "2026-10-27_15-30-05".len(), "{names:?}");
}

#[test]
fn closing_while_recording_asks_and_stopping_closes() {
    let dir = TempDir::new("quit-rec");
    let mut harness = harness_without_changes(&dir);
    harness.get_by_label("Start recording").click();
    step_until(&mut harness, "the recording", |h| {
        clickable(h, "Stop recording")
    });
    request_close(&mut harness);
    harness.step();
    assert!(commands(&harness).contains(&egui::ViewportCommand::CancelClose));
    step_until(&mut harness, "the question", |h| {
        h.query_by_label("Quit while recording?").is_some()
    });
    assert!(
        harness
            .query_by_label("The recording stops and its file is finalized.")
            .is_some()
    );
    harness.get_by_label("Cancel").click();
    step_until(&mut harness, "the question to close", |h| {
        h.query_by_label("Quit while recording?").is_none()
    });
    assert!(clickable(&harness, "Stop recording"), "still recording");

    request_close(&mut harness);
    step_until(&mut harness, "the question", |h| {
        h.query_by_label("Quit while recording?").is_some()
    });
    harness.get_by_label("Stop and quit").click();
    harness.step();
    assert!(commands(&harness).contains(&egui::ViewportCommand::Close));
}

#[test]
fn a_quit_signal_closes_without_asking() {
    let dir = TempDir::new("signal");
    let signal = Arc::new(AtomicBool::new(false));
    let set = Arc::clone(&signal);
    let model = model(&dir);
    let mut harness = Harness::builder()
        .with_size([1400.0, 900.0])
        .build_eframe(move |cc| DesktopApp::new(&cc.egui_ctx, model).with_quit_signal(set));
    settle(&mut harness, 5);
    harness.get_by_label("Start recording").click();
    step_until(&mut harness, "the recording", |h| {
        clickable(h, "Stop recording")
    });
    // Unsaved changes and a running recording, yet no question.
    signal.store(true, Ordering::Relaxed);
    harness.step();
    assert!(commands(&harness).contains(&egui::ViewportCommand::Close));
    request_close(&mut harness);
    harness.step();
    assert!(!commands(&harness).contains(&egui::ViewportCommand::CancelClose));
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
    // A bus in a profile cannot be removed; the engine would refuse.
    assert!(!clickable(&harness, "Remove bus Headphones…"));
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
    assert!(!clickable(&harness, "Add a channel"));
    assert!(!clickable(&harness, "Remove channel Tone…"));
    assert!(!clickable(&harness, "Input of Tone"));
}

#[test]
fn the_stream_video_settings_reach_the_engine() {
    let dir = TempDir::new("video");
    let mut harness = harness(model(&dir));
    harness.get_by_label("Stream settings").click();
    step_until(&mut harness, "the stream settings", |h| {
        h.query_by_label("Video bitrate (kbps)").is_some()
    });
    assert!(is_on(&harness, "Auto"), "the default encoder");
    // A focused number field selects its text; typing replaces it.
    harness.get_by_label("Video bitrate (kbps)").focus();
    settle(&mut harness, 2);
    harness
        .input_mut()
        .events
        .push(egui::Event::Text("4500".to_owned()));
    harness.step();
    harness.key_press(egui::Key::Enter);
    harness.step();
    harness.get_by_label("Software").click();
    harness.step();
    harness.get_by_label("Done").click();
    // Unsaved changes mark a change the engine took.
    step_until(&mut harness, "the engine's answer", |h| {
        h.query_by_label("Server URL").is_none()
            && h.query_by_label("Project Untitled, unsaved changes")
                .is_some()
    });

    harness.get_by_label("Stream settings").click();
    step_until(&mut harness, "the stream settings again", |h| {
        h.query_by_label("Video bitrate (kbps)").is_some()
    });
    assert!(is_on(&harness, "Software"));
    assert_eq!(
        harness
            .get_by_label("Video bitrate (kbps)")
            .value()
            .as_deref()
            .map(str::trim),
        Some("4500 kbps")
    );
    harness.get_by_label("Cancel").click();
    step_until(&mut harness, "the dialog to close", |h| {
        h.query_by_label("Server URL").is_none()
    });

    // The recording has an encoder too, and no bitrate.
    harness.get_by_label("Recording settings").click();
    step_until(&mut harness, "the recording settings", |h| {
        h.query_by_label("Hardware").is_some()
    });
    assert!(harness.query_by_label("Video bitrate (kbps)").is_none());
    harness.get_by_label("Hardware").click();
    harness.step();
    harness.get_by_label("Done").click();
    step_until(&mut harness, "the dialog to close", |h| {
        h.query_by_label("Hardware").is_none()
    });
    // The engine's answer (or, on a refusal, a new reading) replaces what
    // the dialog showed at once.
    settle(&mut harness, 10);
    harness.get_by_label("Recording settings").click();
    step_until(&mut harness, "the recording settings again", |h| {
        is_on(h, "Hardware")
    });
}

#[test]
fn the_monitor_device_is_chosen_while_idle_and_an_ended_monitor_says_why() {
    let dir = TempDir::new("monitor");
    let mut harness = harness(model(&dir));
    harness.get_by_label("Monitor settings").click();
    step_until(&mut harness, "the monitor settings", |h| {
        h.query_by_label("Audio output").is_some()
    });
    assert!(!clickable(&harness, "Use device"), "nothing changed yet");
    harness.get_by_label("Audio output").click();
    step_until(&mut harness, "the output list", |h| {
        h.query_by_label("USB Headset").is_some()
    });
    harness.get_by_label("USB Headset").click();
    harness.step();
    harness.get_by_label("Use device").click();
    // The dock names the engine's device.
    step_until(&mut harness, "the device", |h| {
        h.query_by_label("Use device").is_none() && h.query_by_label("USB Headset").is_some()
    });

    // The test backend plays nothing: switched on in a run, the monitor
    // ends, and the dock says why.
    harness.get_by_label("Routing").click();
    step_until(&mut harness, "the matrix", |h| {
        h.query_by_label("Monitor on").is_some()
    });
    harness.get_by_label("Monitor on").click();
    step_until(&mut harness, "the monitor", |h| is_on(h, "Monitor on"));
    harness
        .get_by_role_and_label(Role::CheckBox, "Preview")
        .click();
    step_until(&mut harness, "the ended monitor", |h| {
        h.query_by_label_contains("Switch it off and on to open the device again")
            .is_some()
    });
    // The device changes only while the pipeline is idle.
    harness.get_by_label("Monitor settings").click();
    step_until(&mut harness, "the idle note", |h| {
        h.query_by_label("Stop the preview and the outputs to choose another device.")
            .is_some()
    });
    assert!(!clickable(&harness, "Audio output"));
    assert!(!clickable(&harness, "Use device"));
}

/// What the matrix says channel `channel` plays.
fn input_of(harness: &Harness<'static, DesktopApp>, channel: &str) -> Option<String> {
    harness
        .query_by_label(&format!("Input of {channel}"))
        .and_then(|node| node.accesskit_node().value())
}

#[test]
fn a_channel_is_added_on_desktop_audio_turned_into_a_tone_and_removed() {
    let dir = TempDir::new("channels");
    let mut harness = harness(model(&dir));
    harness.get_by_label("Routing").click();
    step_until(&mut harness, "the matrix", |h| {
        clickable(h, "Add a channel")
    });
    harness.get_by_label("Add a channel").click();
    step_until(&mut harness, "the channel dialog", |h| {
        h.query_by_label("New channel").is_some()
    });
    type_into_dialog(&mut harness, "Desktop");

    // The engine's inputs, desktop audio apart.
    harness.get_by_label("Audio input").click();
    step_until(&mut harness, "the input list", |h| {
        h.query_by_label("Monitor of Speakers").is_some()
    });
    assert!(harness.query_by_label("USB Mic").is_some());
    assert!(harness.query_by_label("DESKTOP AUDIO").is_some());
    harness.get_by_label("Monitor of Speakers").click();
    step_until(&mut harness, "the desktop audio note", |h| {
        h.query_by_label_contains("Desktop audio: what an output plays")
            .is_some()
    });
    harness.get_by_label("Add channel").click();
    step_until(&mut harness, "the new channel", |h| {
        input_of(h, "Desktop").is_some() && h.query_by_label("New channel").is_none()
    });
    assert_eq!(
        input_of(&harness, "Desktop").as_deref(),
        Some("Monitor of Speakers")
    );
    assert!(
        is_on(&harness, "Send Desktop to Program"),
        "a new channel sends to program"
    );
    assert!(
        harness
            .query_by_label("Project Untitled, unsaved changes")
            .is_some()
    );

    // Its input becomes a test tone; unchanged, there is nothing to apply.
    harness.get_by_label("Input of Desktop").click();
    step_until(&mut harness, "the input dialog", |h| {
        h.query_by_label("Input of “Desktop”").is_some()
    });
    assert!(!clickable(&harness, "Use input"));
    harness.get_by_label("Test tone").click();
    harness.step();
    harness.get_by_label("Use input").click();
    step_until(&mut harness, "the tone", |h| {
        input_of(h, "Desktop").as_deref() == Some("Tone 1000 Hz")
    });

    // Removing asks first.
    harness.get_by_label("Remove channel Desktop…").click();
    step_until(&mut harness, "the confirmation", |h| {
        h.query_by_label("Remove channel “Desktop”?").is_some()
    });
    harness.get_by_label("Remove channel").click();
    step_until(&mut harness, "the channel to go", |h| {
        input_of(h, "Desktop").is_none()
    });
}

/// The viewport commands of the last frame (title, close, cancel close).
fn commands(harness: &Harness<'static, DesktopApp>) -> Vec<egui::ViewportCommand> {
    harness
        .output()
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .map(|viewport| viewport.commands.clone())
        .unwrap_or_default()
}

/// The window's close button.
fn request_close(harness: &mut Harness<'static, DesktopApp>) {
    harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .events
        .push(egui::ViewportEvent::Close);
}

/// Type into the dialog's first text field.
fn type_into_dialog(harness: &mut Harness<'static, DesktopApp>, text: &str) {
    harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("the text field")
        .focus();
    harness.step();
    harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("the text field")
        .type_text(text);
    harness.step();
}

#[test]
fn a_project_is_saved_changed_and_opened_again() {
    let dir = TempDir::new("project");
    let mut harness = harness(model(&dir));
    // Adding the source was a change.
    harness
        .get_by_label("Project Untitled, unsaved changes")
        .click();
    step_until(&mut harness, "the project menu", |h| {
        h.query_by_label("Save as…").is_some()
    });
    harness.get_by_label("Save as…").click();
    step_until(&mut harness, "the save dialog", |h| {
        h.query_by_label("Save the project as").is_some()
    });
    type_into_dialog(&mut harness, "show");
    step_until(&mut harness, "the listing", |h| clickable(h, "Save"));
    harness.get_by_label("Save").click();
    step_until(&mut harness, "the save", |h| {
        h.query_by_label("Project show.json").is_some()
    });
    assert!(dir.0.join("show.json").is_file());

    // A change marks it again.
    harness.get_by_label("Mute Tone").click();
    step_until(&mut harness, "the mute", |h| {
        is_on(h, "Mute Tone")
            && h.query_by_label("Project show.json, unsaved changes")
                .is_some()
    });

    // Opening the file over the change asks first, then brings the saved
    // state back.
    harness
        .get_by_label("Project show.json, unsaved changes")
        .click();
    step_until(&mut harness, "the project menu", |h| {
        h.query_by_label("Open project…").is_some()
    });
    harness.get_by_label("Open project…").click();
    step_until(&mut harness, "the file list", |h| {
        h.query_by_label("show.json").is_some()
    });
    assert!(!clickable(&harness, "Open"), "nothing chosen yet");
    harness.get_by_label("show.json").click();
    step_until(&mut harness, "the check", |h| {
        h.query_by_label("A valid project.").is_some() && clickable(h, "Open")
    });
    harness.get_by_label("Open").click();
    step_until(&mut harness, "the confirmation", |h| {
        h.query_by_label("Discard and open").is_some()
    });
    harness.get_by_label("Discard and open").click();
    step_until(&mut harness, "the opened project", |h| {
        h.query_by_label("Project show.json").is_some() && !is_on(h, "Mute Tone")
    });
}

#[test]
fn a_name_that_exists_is_replaced_only_when_the_dialog_says_so() {
    let dir = TempDir::new("replace");
    std::fs::write(dir.0.join("show.json"), b"{}").expect("existing file");
    let mut harness = harness(model(&dir));
    harness
        .get_by_label("Project Untitled, unsaved changes")
        .click();
    step_until(&mut harness, "the project menu", |h| {
        h.query_by_label("Save as…").is_some()
    });
    harness.get_by_label("Save as…").click();
    step_until(&mut harness, "the save dialog", |h| {
        h.query_by_label("Save the project as").is_some()
    });
    type_into_dialog(&mut harness, "show.json");
    step_until(&mut harness, "the warning", |h| {
        h.query_by_label("“show.json” exists: saving replaces it.")
            .is_some()
            && clickable(h, "Replace")
    });
    harness.get_by_label("Replace").click();
    step_until(&mut harness, "the save", |h| {
        h.query_by_label("Project show.json").is_some()
    });
    let saved = std::fs::read_to_string(dir.0.join("show.json")).expect("saved");
    assert!(saved.contains("lower third"), "{saved}");

    // A name the engine would refuse cannot be saved.
    harness.get_by_label("Project show.json").click();
    step_until(&mut harness, "the project menu", |h| {
        h.query_by_label("Save as…").is_some()
    });
    harness.get_by_label("Save as…").click();
    step_until(&mut harness, "the save dialog", |h| {
        h.query_by_label("Save the project as").is_some()
    });
    harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("the text field")
        .focus();
    harness.step();
    // The field starts with the current name; replace it.
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    harness.step();
    harness
        .get_all_by_role(Role::TextInput)
        .next()
        .expect("the text field")
        .type_text("../away.json");
    step_until(&mut harness, "the refusal", |h| {
        h.query_by_label_contains("Use a plain file name").is_some()
    });
    assert!(harness.query_by_label("Save").is_some() && !clickable(&harness, "Save"));
}

#[test]
fn closing_with_unsaved_changes_asks_and_can_save_first() {
    let dir = TempDir::new("quit");
    let mut harness = harness(model(&dir));
    request_close(&mut harness);
    harness.step();
    assert!(
        commands(&harness).contains(&egui::ViewportCommand::CancelClose),
        "{:?}",
        commands(&harness)
    );
    step_until(&mut harness, "the question", |h| {
        h.query_by_label("Quit with unsaved changes?").is_some()
    });
    // Untitled: saving asks for a name, then closes once saved.
    harness.get_by_label("Save as and quit…").click();
    step_until(&mut harness, "the save dialog", |h| {
        h.query_by_label("Save the project as").is_some()
    });
    type_into_dialog(&mut harness, "last");
    step_until(&mut harness, "the listing", |h| {
        clickable(h, "Save and quit")
    });
    harness.get_by_label("Save and quit").click();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !commands(&harness).contains(&egui::ViewportCommand::Close) {
        assert!(Instant::now() < deadline, "the window never closed");
        harness.step();
        thread::sleep(Duration::from_millis(10));
    }
    assert!(dir.0.join("last.json").is_file());
}

#[test]
fn quitting_without_saving_closes_and_a_saved_project_closes_at_once() {
    let dir = TempDir::new("discard");
    let mut harness = harness(model(&dir));
    request_close(&mut harness);
    step_until(&mut harness, "the question", |h| {
        h.query_by_label("Quit with unsaved changes?").is_some()
    });
    harness.get_by_label("Quit without saving").click();
    harness.step();
    assert!(commands(&harness).contains(&egui::ViewportCommand::Close));
    assert!(!dir.0.join("last.json").exists());

    // Nothing unsaved: the window closes without a question.
    let dir = TempDir::new("saved");
    let mut harness = harness_without_changes(&dir);
    request_close(&mut harness);
    harness.step();
    assert!(!commands(&harness).contains(&egui::ViewportCommand::CancelClose));
    settle(&mut harness, 3);
    assert!(
        harness
            .query_by_label("Quit with unsaved changes?")
            .is_none()
    );
}

/// A fresh engine nobody changed: nothing to save.
fn harness_without_changes(dir: &TempDir) -> Harness<'static, DesktopApp> {
    let out = dir.0.clone();
    let worker = EngineWorker::spawn(
        move || Engine::new().with_output_dir(&out).with_project_dir(out),
        || {},
    )
    .expect("worker");
    let mut model = Model::new(worker);
    wait(&mut model, "the scene list", |m| m.scenes.is_some());
    harness(model)
}

#[test]
fn closing_over_another_dialog_asks_and_a_change_on_its_way_counts() {
    let dir = TempDir::new("close-dialog");
    let mut harness = harness(model(&dir));
    harness
        .get_by_label("Project Untitled, unsaved changes")
        .click();
    step_until(&mut harness, "the project menu", |h| {
        h.query_by_label("Save as…").is_some()
    });
    harness.get_by_label("Save as…").click();
    step_until(&mut harness, "the save dialog", |h| {
        h.query_by_label("Save the project as").is_some()
    });
    request_close(&mut harness);
    step_until(&mut harness, "the question", |h| {
        h.query_by_label("Quit with unsaved changes?").is_some()
            && h.query_by_label("Save the project as").is_none()
    });

    // A change the engine has not answered yet is a change too.
    let dir = TempDir::new("in-flight");
    let out = dir.0.clone();
    let worker = EngineWorker::spawn(
        move || Engine::new().with_output_dir(&out).with_project_dir(out),
        || {},
    )
    .expect("worker");
    let mut model = Model::new(worker);
    wait(&mut model, "the mixer", |m| m.mixer.is_some());
    assert!(!model.has_changes());
    let channel = model.mixer.as_ref().expect("mixer").channels[0]
        .spec
        .id
        .clone();
    model.update_channel(&channel, None, Some(true));
    assert!(model.has_changes() && !model.unsaved);
    wait(&mut model, "the answer", |m| m.unsaved);
}

#[test]
fn a_double_click_opens_a_file_once_it_is_checked() {
    let dir = TempDir::new("double-click");
    let mut engine = Engine::new().with_project_dir(&dir.0);
    for (name, params) in [
        ("create_scene", serde_json::json!({"name": "Saved show"})),
        (
            "save_project",
            serde_json::json!({"file_name": "show.json"}),
        ),
    ] {
        let command = command_api::Command::decode(name, Some(&params)).expect(name);
        command_api::CommandHandler::handle(&mut engine, command).expect(name);
    }
    let out = dir.0.clone();
    let worker = EngineWorker::spawn(
        move || Engine::new().with_output_dir(&out).with_project_dir(out),
        || {},
    )
    .expect("worker");
    let mut model = Model::new(worker);
    wait(&mut model, "the scene list", |m| m.scenes.is_some());
    // kittest steps once per input event, by `step_dt` each: two clicks
    // within egui's 0.3 s need short steps.
    let mut harness = Harness::builder()
        .with_size([1400.0, 900.0])
        .with_step_dt(0.01)
        .build_eframe(move |cc| DesktopApp::new(&cc.egui_ctx, model));
    settle(&mut harness, 5);
    harness.get_by_label("Project Untitled").click();
    step_until(&mut harness, "the project menu", |h| {
        h.query_by_label("Open project…").is_some()
    });
    harness.get_by_label("Open project…").click();
    step_until(&mut harness, "the file list", |h| {
        h.query_by_label("show.json").is_some()
    });
    // Over a second since the last click, so these two are a double
    // click, not part of a triple one.
    settle(&mut harness, 120);
    harness.get_by_label("show.json").click();
    harness.step();
    harness.get_by_label("show.json").click();
    harness.step();
    step_until(&mut harness, "the opened project", |h| {
        h.query_by_label("Project show.json").is_some() && h.query_by_label("Saved show").is_some()
    });
}

/// Press `key` with Ctrl (Cmd on macOS) and let a frame take it.
fn command(harness: &mut Harness<'static, DesktopApp>, key: egui::Key) {
    harness.key_press_modifiers(egui::Modifiers::COMMAND, key);
    harness.step();
}

fn press(harness: &mut Harness<'static, DesktopApp>, key: egui::Key) {
    harness.key_press(key);
    harness.step();
}

/// The label of the control with keyboard focus (`Some(None)`: one
/// without a label).
fn focused(harness: &Harness<'static, DesktopApp>) -> Option<Option<String>> {
    harness
        .query(by().predicate(|node| node.is_focused()))
        .map(|node| node.accesskit_node().label())
}

/// The list row labelled `label`, if shown (the inspector and the program
/// header repeat names as text). A row is selected or not, which egui
/// reports as toggled.
fn row<'h>(
    harness: &'h Harness<'static, DesktopApp>,
    label: &'h str,
) -> Option<egui_kittest::Node<'h>> {
    harness.query(by().label(label).predicate(|node| node.toggled().is_some()))
}

/// Whether the list row labelled `label` shows as selected.
fn is_selected(harness: &Harness<'static, DesktopApp>, label: &str) -> bool {
    row(harness, label).is_some_and(|node| node.accesskit_node().toggled() == Some(Toggled::True))
}

#[test]
fn space_switches_the_preview_unless_a_control_has_focus() {
    let dir = TempDir::new("space");
    let mut harness = harness(model(&dir));
    press(&mut harness, egui::Key::Space);
    step_until(&mut harness, "the preview", |h| is_on(h, "Preview"));

    // A control with focus takes Space as its click: the row selects its
    // source, and the preview stays on.
    row(&harness, "lower third").expect("the row").focus();
    harness.step();
    press(&mut harness, egui::Key::Space);
    step_until(&mut harness, "the inspector", |h| {
        h.query_by_label("Remove source…").is_some()
    });
    assert!(is_on(&harness, "Preview"));

    // Escape leaves the row; Space is the preview's again.
    press(&mut harness, egui::Key::Escape);
    assert_eq!(focused(&harness), None);
    press(&mut harness, egui::Key::Space);
    step_until(&mut harness, "the preview to stop", |h| {
        !is_on(h, "Preview")
    });
}

#[test]
fn ctrl_r_records_and_stopping_by_key_asks_first() {
    let dir = TempDir::new("ctrl-r");
    let mut harness = harness(model(&dir));
    command(&mut harness, egui::Key::R);
    step_until(&mut harness, "the recording", |h| {
        clickable(h, "Stop recording")
    });

    // The key asks too; Escape cancels and the recording runs on.
    command(&mut harness, egui::Key::R);
    step_until(&mut harness, "the confirmation", |h| {
        h.query_by_label("Stop recording?").is_some()
    });
    press(&mut harness, egui::Key::Escape);
    step_until(&mut harness, "the confirmation to close", |h| {
        h.query_by_label("Stop recording?").is_none()
    });
    settle(&mut harness, 10);
    assert!(clickable(&harness, "Stop recording"), "still recording");

    command(&mut harness, egui::Key::R);
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
    assert_eq!(recordings(&dir).len(), 1);
}

#[test]
fn ctrl_l_asks_for_a_target_and_keys_wait_while_a_dialog_is_open() {
    let dir = TempDir::new("ctrl-l");
    let mut harness = harness(model(&dir));
    command(&mut harness, egui::Key::L);
    step_until(&mut harness, "the stream settings", |h| {
        h.query_by_label("Server URL").is_some()
    });
    // Behind the dialog, Ctrl+R starts nothing.
    command(&mut harness, egui::Key::R);
    settle(&mut harness, 10);
    assert!(harness.query_by_label("Stop recording").is_none());
    press(&mut harness, egui::Key::Escape);
    step_until(&mut harness, "the dialog to close", |h| {
        h.query_by_label("Server URL").is_none()
    });
}

#[test]
fn keys_wait_while_a_text_field_has_focus() {
    let dir = TempDir::new("typing");
    let mut harness = harness(model(&dir));
    harness.get_by_label("New scene").click();
    step_until(&mut harness, "the name field", |h| {
        h.query_by_label("Enter creates the scene, Esc cancels.")
            .is_some()
            && focused(h).is_some()
    });
    command(&mut harness, egui::Key::R);
    press(&mut harness, egui::Key::Space);
    settle(&mut harness, 10);
    assert!(harness.query_by_label("Stop recording").is_none());
    assert!(!is_on(&harness, "Preview"));
}

#[test]
fn ctrl_and_a_digit_put_a_scene_on_program_and_arrows_move_in_the_lists() {
    let dir = TempDir::new("scenes");
    let mut model = model(&dir);
    model.create_scene("Second");
    wait(&mut model, "the second scene", |m| {
        m.scenes
            .as_ref()
            .is_some_and(|list| list.scenes.iter().any(|scene| scene.name == "Second"))
    });
    let mut harness = harness(model);
    // The selected scene is the program scene until another is chosen;
    // putting it on program again is not offered.
    assert!(!clickable(&harness, "Put on program"));
    command(&mut harness, egui::Key::Num2);
    step_until(&mut harness, "the second scene on program", |h| {
        clickable(h, "Put on program")
    });
    command(&mut harness, egui::Key::Num1);
    step_until(&mut harness, "the first scene on program", |h| {
        !clickable(h, "Put on program")
    });
    // No third scene: nothing changes.
    command(&mut harness, egui::Key::Num3);
    settle(&mut harness, 10);
    assert!(!clickable(&harness, "Put on program"));

    // Down in the focused scene list selects the next scene and keeps the
    // focus in the list, also at its end.
    row(&harness, "Test pattern").expect("the row").focus();
    harness.step();
    press(&mut harness, egui::Key::ArrowDown);
    step_until(&mut harness, "the second scene selected", |h| {
        is_selected(h, "Second") && h.query_by_label_contains("has no sources yet").is_some()
    });
    assert_eq!(focused(&harness), Some(Some("Second".to_owned())));
    press(&mut harness, egui::Key::ArrowDown);
    settle(&mut harness, 3);
    assert_eq!(focused(&harness), Some(Some("Second".to_owned())));
    press(&mut harness, egui::Key::ArrowUp);
    step_until(&mut harness, "the first scene selected", |h| {
        is_selected(h, "Test pattern") && row(h, "lower third").is_some()
    });

    // The same in the source list, top layer first; the inspector follows.
    row(&harness, "lower third").expect("the row").focus();
    harness.step();
    press(&mut harness, egui::Key::ArrowDown);
    step_until(&mut harness, "the next source selected", |h| {
        is_selected(h, "overlay") && h.query_by_label("Remove source…").is_some()
    });
    assert_eq!(focused(&harness), Some(Some("overlay".to_owned())));
    press(&mut harness, egui::Key::ArrowUp);
    step_until(&mut harness, "the top source selected", |h| {
        is_selected(h, "lower third")
    });
}

/// Whether source row `upper` is drawn above row `lower`: the list shows
/// the top layer first, so this is the drawing order.
fn above(harness: &Harness<'static, DesktopApp>, upper: &str, lower: &str) -> bool {
    let top = |label| row(harness, label).map(|node| node.rect().top());
    matches!((top(upper), top(lower)), (Some(a), Some(b)) if a < b)
}

#[test]
fn the_eye_hides_and_shows_a_source() {
    let dir = TempDir::new("eye");
    let mut harness = harness(model(&dir));
    harness.get_by_label("Hide lower third").click();
    step_until(&mut harness, "the hidden source", |h| {
        h.query_by_label("Show lower third").is_some()
    });
    // The click went to the eye, not the row.
    assert!(harness.query_by_label("Remove source…").is_none());
    harness.get_by_label("Show lower third").click();
    step_until(&mut harness, "the shown source", |h| {
        h.query_by_label("Hide lower third").is_some()
    });
}

#[test]
fn keys_move_the_selected_source_between_layers() {
    let dir = TempDir::new("layer-keys");
    let mut harness = harness(model(&dir));
    // Top first: lower third (added last), overlay, colour bars.
    assert!(above(&harness, "lower third", "overlay"));
    // Nothing selected: nothing moves.
    command(&mut harness, egui::Key::ArrowDown);
    settle(&mut harness, 3);
    assert!(above(&harness, "lower third", "overlay"));

    row(&harness, "lower third").expect("the row").click();
    step_until(&mut harness, "the inspector", |h| {
        h.query_by_label("Remove source…").is_some()
    });
    row(&harness, "lower third").expect("the row").focus();
    harness.step();
    command(&mut harness, egui::Key::ArrowDown);
    step_until(&mut harness, "one layer down", |h| {
        above(h, "overlay", "lower third") && above(h, "lower third", "color_bars")
    });
    // The focus moves with the source's row.
    assert_eq!(focused(&harness), Some(Some("lower third".to_owned())));
    command(&mut harness, egui::Key::ArrowDown);
    step_until(&mut harness, "the bottom", |h| {
        above(h, "color_bars", "lower third")
    });
    // At the bottom it stays.
    command(&mut harness, egui::Key::ArrowDown);
    settle(&mut harness, 5);
    assert!(above(&harness, "color_bars", "lower third"));
    command(&mut harness, egui::Key::ArrowUp);
    step_until(&mut harness, "one layer up", |h| {
        above(h, "lower third", "color_bars") && above(h, "overlay", "lower third")
    });
    // Plain Up still moves the selection from the moved row.
    assert_eq!(focused(&harness), Some(Some("lower third".to_owned())));
    press(&mut harness, egui::Key::ArrowUp);
    step_until(&mut harness, "the row above selected", |h| {
        is_selected(h, "overlay")
    });
}

#[test]
fn dragging_a_source_row_reorders_the_layers() {
    let dir = TempDir::new("layer-drag");
    let mut harness = harness(model(&dir));
    let from = row(&harness, "lower third")
        .expect("the row")
        .rect()
        .center();
    let to = row(&harness, "color_bars")
        .expect("the row")
        .rect()
        .bottom()
        - 2.0;
    harness.hover_at(from);
    harness.step();
    harness.drag_at(from);
    harness.step();
    for step in 1..=5 {
        harness.hover_at(egui::pos2(
            from.x,
            from.y + (to - from.y) * step as f32 / 5.0,
        ));
        harness.step();
    }
    harness.drop_at(egui::pos2(from.x, to));
    step_until(&mut harness, "the bottom", |h| {
        above(h, "color_bars", "lower third")
    });
    assert!(above(&harness, "overlay", "color_bars"));
    // A drag is not a click: nothing was selected.
    assert!(harness.query_by_label("Remove source…").is_none());
}

#[test]
fn delete_asks_before_it_removes_the_selected_source() {
    let dir = TempDir::new("delete");
    let mut harness = harness(model(&dir));
    // Nothing selected: nothing to remove.
    press(&mut harness, egui::Key::Delete);
    settle(&mut harness, 3);
    assert!(harness.query_by_label_contains("Remove source “").is_none());

    row(&harness, "lower third").expect("the row").click();
    step_until(&mut harness, "the inspector", |h| {
        h.query_by_label("Remove source…").is_some()
    });
    press(&mut harness, egui::Key::Delete);
    step_until(&mut harness, "the confirmation", |h| {
        h.query_by_label("Remove source “lower third”?").is_some()
    });
    press(&mut harness, egui::Key::Escape);
    step_until(&mut harness, "the confirmation to close", |h| {
        h.query_by_label("Remove source “lower third”?").is_none()
    });
    assert!(row(&harness, "lower third").is_some());

    // A Mac's delete key sends Backspace.
    press(&mut harness, egui::Key::Backspace);
    step_until(&mut harness, "the confirmation", |h| {
        h.query_by_label("Remove source “lower third”?").is_some()
    });
    harness.get_by_label("Remove source").click();
    step_until(&mut harness, "the source to go", |h| {
        row(h, "lower third").is_none()
    });
}

#[test]
fn project_keys_save_save_as_and_open() {
    let dir = TempDir::new("project-keys");
    let mut harness = harness(model(&dir));
    // Never saved: Ctrl+S asks for a name.
    command(&mut harness, egui::Key::S);
    step_until(&mut harness, "the save dialog", |h| {
        h.query_by_label("Save the project as").is_some()
    });
    type_into_dialog(&mut harness, "show");
    step_until(&mut harness, "the listing", |h| clickable(h, "Save"));
    harness.get_by_label("Save").click();
    step_until(&mut harness, "the save", |h| {
        h.query_by_label("Project show.json").is_some()
    });

    // Saved once, Ctrl+S saves to the file without asking.
    harness.get_by_label("Mute Tone").click();
    step_until(&mut harness, "the change", |h| {
        h.query_by_label("Project show.json, unsaved changes")
            .is_some()
    });
    command(&mut harness, egui::Key::S);
    step_until(&mut harness, "the save", |h| {
        h.query_by_label("Project show.json").is_some()
    });
    assert!(harness.query_by_label("Save the project as").is_none());

    // Ctrl+Shift+S asks for a name, Ctrl+O lists the folder.
    harness.key_press_modifiers(
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::S,
    );
    step_until(&mut harness, "the save dialog", |h| {
        h.query_by_label("Save the project as").is_some()
    });
    press(&mut harness, egui::Key::Escape);
    step_until(&mut harness, "the dialog to close", |h| {
        h.query_by_label("Save the project as").is_none()
    });
    command(&mut harness, egui::Key::O);
    step_until(&mut harness, "the file list", |h| {
        h.query_by_label("show.json").is_some()
    });
}

#[test]
fn tab_moves_through_the_docks_in_reading_order() {
    let dir = TempDir::new("tab");
    let mut harness = harness(model(&dir));
    row(&harness, "lower third").expect("the row").click();
    step_until(&mut harness, "the inspector", |h| {
        h.query_by_label("Remove source…").is_some()
    });
    let mut order = Vec::new();
    for _ in 0..80 {
        press(&mut harness, egui::Key::Tab);
        let label = focused(&harness)
            .expect("Tab focuses a control")
            .expect("every control that takes the focus is named");
        if order.first() == Some(&label) {
            break;
        }
        order.push(label);
    }
    // One control of each dock, in reading order: the top bar, scenes,
    // sources, program, inspector, mixer, outputs.
    let anchors = [
        "Project Untitled, unsaved changes",
        "Start recording",
        "New scene",
        "lower third",
        "Preview",
        "X",
        "Remove source…",
        "Console",
        "Mute Tone",
        "Monitor settings",
    ];
    let places: Vec<usize> = anchors
        .iter()
        .map(|anchor| {
            order
                .iter()
                .position(|label| label == anchor)
                .unwrap_or_else(|| panic!("{anchor} not reached: {order:?}"))
        })
        .collect();
    assert!(
        places.is_sorted(),
        "Tab order {order:?} does not follow {anchors:?}"
    );
    // Shift+Tab goes back.
    harness.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::Tab);
    harness.step();
    assert_eq!(focused(&harness), Some(order.last().cloned()));
}

#[test]
fn escape_closes_the_inspector_drawer() {
    let dir = TempDir::new("drawer");
    let mut harness = harness_sized(model(&dir), [1280.0, 720.0]);
    row(&harness, "lower third").expect("the row").click();
    step_until(&mut harness, "the drawer", |h| {
        h.query_by_label("Close the inspector (Esc)").is_some()
    });
    press(&mut harness, egui::Key::Escape);
    step_until(&mut harness, "the drawer to close", |h| {
        h.query_by_label("Close the inspector (Esc)").is_none()
    });
}

/// Press at `from`, move to `to` and release there, a frame per event.
fn drag(harness: &mut Harness<'static, DesktopApp>, from: egui::Pos2, to: egui::Pos2) {
    harness.hover_at(from);
    harness.step();
    harness.drag_at(from);
    harness.step();
    harness.hover_at(to);
    harness.step();
    harness.drop_at(to);
    settle(harness, 3);
}

#[test]
fn dragging_a_dock_edge_resizes_the_dock() {
    let dir = TempDir::new("resize");
    let mut harness = harness(model(&dir));
    // The scenes dock's add button sits at the browser's right edge.
    let before = harness.get_by_label("New scene").rect();
    let edge = egui::pos2(260.0, 400.0);
    drag(&mut harness, edge, edge + egui::vec2(40.0, 0.0));
    let after = harness.get_by_label("New scene").rect();
    assert_eq!(after.left() - before.left(), 40.0);
    // Past its widest, the dock stops there (420).
    drag(
        &mut harness,
        egui::pos2(300.0, 400.0),
        egui::pos2(600.0, 400.0),
    );
    let widest = harness.get_by_label("New scene").rect();
    assert_eq!(widest.left() - before.left(), 160.0);

    // The console grows upwards from its top edge.
    let before = harness.get_by_label("Console").rect();
    let edge = egui::pos2(400.0, 900.0 - 232.0);
    drag(&mut harness, edge, edge - egui::vec2(0.0, 50.0));
    let after = harness.get_by_label("Console").rect();
    assert_eq!(before.top() - after.top(), 50.0);
}

/// Write a `width` x `height` PNG of one colour into the project folder.
fn write_image(dir: &TempDir, name: &str, width: u32, height: u32, rgb: [u8; 3]) {
    image::RgbImage::from_pixel(width, height, image::Rgb(rgb))
        .save(dir.0.join(name))
        .expect("png");
}

/// The value of the field labelled `label`, if shown.
fn field_value(harness: &Harness<'static, DesktopApp>, label: &str) -> Option<String> {
    harness
        .query_by_label(label)
        .and_then(|field| field.value())
}

#[test]
fn an_image_from_the_project_folder_is_added_changed_and_read_again() {
    let dir = TempDir::new("image");
    write_image(&dir, "logo.png", 64, 32, [255, 0, 0]);
    write_image(&dir, "other.png", 16, 16, [0, 255, 0]);
    let mut harness = harness(model(&dir));
    harness.get_by_label("Add a source to this scene").click();
    step_until(&mut harness, "the dialog", |h| {
        h.query_by_label("Image").is_some()
    });
    harness.get_by_label("Image").click();
    step_until(&mut harness, "the image list", |h| {
        field_value(h, "Image file").as_deref() == Some("Choose an image")
    });
    // A file must be chosen first.
    assert!(!clickable(&harness, "Add source"));
    assert!(harness.query_all_by_label("Own size").next().is_some());
    harness.get_by_label("Image file").click();
    step_until(&mut harness, "the files", |h| {
        h.query_by_label("logo.png · 64×32").is_some()
    });
    harness.get_by_label("logo.png · 64×32").click();
    step_until(&mut harness, "the choice", |h| clickable(h, "Add source"));
    // Named after its file.
    harness.get_by_label("Add source").click();
    step_until(&mut harness, "the new layer", |h| row(h, "logo").is_some());

    row(&harness, "logo").expect("the row").click();
    step_until(&mut harness, "the inspector", |h| {
        h.query_by_label("Read again").is_some()
    });
    assert_eq!(
        field_value(&harness, "Image file").as_deref(),
        Some("logo.png")
    );
    harness.get_by_label("Image file").click();
    step_until(&mut harness, "the files", |h| {
        h.query_by_label("other.png").is_some()
    });
    harness.get_by_label("other.png").click();
    step_until(&mut harness, "the other file", |h| {
        field_value(h, "Image file").as_deref() == Some("other.png")
    });
    // Gone from the folder: the inspector says so, and reading it again
    // fails in the log.
    std::fs::remove_file(dir.0.join("other.png")).expect("remove");
    harness.get_by_label("Refresh list").click();
    step_until(&mut harness, "the warning", |h| {
        h.query_by_label_contains("Not in the project folder")
            .is_some()
    });
    harness.get_by_label("Read again").click();
    step_until(&mut harness, "the error", |h| {
        h.query_by_label_contains("no image file `other.png`")
            .is_some()
    });
}

#[test]
fn a_camera_is_added_with_a_mode_and_changed_in_the_inspector() {
    let dir = TempDir::new("camera");
    let mut harness = harness(model(&dir));
    harness.get_by_label("Add a source to this scene").click();
    step_until(&mut harness, "the dialog", |h| {
        h.query_by_label("Camera").is_some()
    });
    harness.get_by_label("Camera").click();
    step_until(&mut harness, "the camera list", |h| {
        field_value(h, "Camera device").as_deref() == Some("Choose a camera")
    });
    // A camera must be chosen first.
    assert!(!clickable(&harness, "Add source"));
    harness.get_by_label("Camera device").click();
    step_until(&mut harness, "the cameras", |h| {
        h.query_by_label("Front Camera").is_some()
    });
    harness.get_by_label("Front Camera").click();
    step_until(&mut harness, "the modes", |h| {
        field_value(h, "Camera mode").as_deref() == Some("Automatic")
    });
    harness.get_by_label("Camera mode").click();
    step_until(&mut harness, "the mode list", |h| {
        h.query_by_label("1280×720 · 30 fps").is_some()
    });
    harness.get_by_label("1280×720 · 30 fps").click();
    step_until(&mut harness, "the mode", |h| {
        field_value(h, "Camera mode").as_deref() == Some("1280×720 · 30 fps")
    });
    // Named after its camera.
    harness.get_by_label("Add source").click();
    step_until(&mut harness, "the new layer", |h| {
        row(h, "Front Camera").is_some()
    });

    row(&harness, "Front Camera").expect("the row").click();
    step_until(&mut harness, "the inspector", |h| {
        field_value(h, "Camera device").as_deref() == Some("Front Camera")
    });
    assert_eq!(
        field_value(&harness, "Camera mode").as_deref(),
        Some("1280×720 · 30 fps")
    );
    // Another camera keeps the mode, since it has it.
    harness.get_by_label("Camera device").click();
    step_until(&mut harness, "the cameras", |h| {
        h.query_by_label("Desk Camera").is_some()
    });
    harness.get_by_label("Desk Camera").click();
    step_until(&mut harness, "the other camera", |h| {
        field_value(h, "Camera device").as_deref() == Some("Desk Camera")
    });
    assert_eq!(
        field_value(&harness, "Camera mode").as_deref(),
        Some("1280×720 · 30 fps")
    );
    harness.get_by_label("Camera mode").click();
    step_until(&mut harness, "the mode list", |h| {
        h.query_by_label("1280×720 · 60 fps").is_some()
    });
    harness.get_by_label("1280×720 · 60 fps").click();
    step_until(&mut harness, "the other mode", |h| {
        field_value(h, "Camera mode").as_deref() == Some("1280×720 · 60 fps")
    });

    // Rendered, the camera delivers frames in that mode.
    harness
        .get_by_role_and_label(Role::CheckBox, "Preview")
        .click();
    step_until(&mut harness, "the running camera", |h| {
        h.query_by_label("Capturing").is_some()
            && h.query_by_label_contains("Runs in 1280×720 · 60 fps · YUY2")
                .is_some()
    });
}

#[test]
fn a_missing_camera_warns_and_restarts_once_plugged_in() {
    let dir = TempDir::new("camera-missing");
    let cameras = ListedCameras::default();
    let mut model = model_with(&dir, cameras.clone());
    model.set_preview(true);
    wait(&mut model, "the preview", |m| m.preview_enabled);
    cameras.plug(FRONT_CAMERA, false);
    let program = model
        .scenes
        .as_ref()
        .expect("scenes")
        .active_scene_id
        .clone();
    model.add_source(
        &program,
        &SourceForm {
            name: "host cam".to_owned(),
            kind: SourceType::Camera,
            camera_id: Some(FRONT_CAMERA.to_owned()),
            ..SourceForm::default()
        },
    );
    // Added all the same; the scene list's warning reaches the log.
    wait(&mut model, "the warning", |m| {
        m.log
            .iter()
            .any(|entry| entry.level == Level::Warning && entry.text.contains("not found"))
    });
    let mut harness = harness(model);
    row(&harness, "host cam").expect("the row").click();
    step_until(&mut harness, "the ended capture", |h| {
        h.query_by_label_contains("Ended: could not start")
            .is_some()
    });
    step_until(&mut harness, "the missing camera", |h| {
        field_value(h, "Camera device").as_deref() == Some("v4l2_input.usb-front (not found)")
    });

    // Back, it opens on a restart, in the mode the engine chooses, and
    // the list finds it again by itself.
    cameras.plug(FRONT_CAMERA, true);
    harness.get_by_label("Restart capture").click();
    step_until(&mut harness, "the running camera", |h| {
        h.query_by_label("Capturing").is_some()
            && h.query_by_label_contains("Runs in 1920×1080 · 30 fps · YUY2")
                .is_some()
    });
    step_until(&mut harness, "the camera listed", |h| {
        field_value(h, "Camera device").as_deref() == Some("Front Camera")
    });

    // Unplugged while it runs, it ends and is missing; once back, a restart
    // opens it again.
    cameras.plug(FRONT_CAMERA, false);
    step_until(&mut harness, "the ended camera", |h| {
        h.query_by_label_contains("Ended: camera `v4l2_input.usb-front` was unplugged")
            .is_some()
    });
    step_until(&mut harness, "the missing camera", |h| {
        field_value(h, "Camera device").as_deref() == Some("v4l2_input.usb-front (not found)")
    });
    cameras.plug(FRONT_CAMERA, true);
    harness.get_by_label("Restart capture").click();
    step_until(&mut harness, "the camera again", |h| {
        h.query_by_label("Capturing").is_some()
    });
}
