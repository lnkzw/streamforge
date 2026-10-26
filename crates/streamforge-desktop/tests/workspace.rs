//! The production workspace under simulated input (egui_kittest), on the
//! real model and engine: what the operator clicks reaches the engine, and
//! consequential actions ask first. No rendering is involved, so these run
//! everywhere.

mod support;

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
use support::ListedInputs;

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
/// engine lists audio inputs but captures nothing.
fn model(dir: &TempDir) -> Model {
    let out = dir.0.clone();
    let worker = EngineWorker::spawn(
        move || {
            Engine::with_capture(Box::new(ListedInputs::new()))
                .with_output_dir(&out)
                .with_project_dir(out)
        },
        || {},
    )
    .expect("worker");
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
