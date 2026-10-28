//! Renders the real desktop UI headless to PNG files, to look at the result
//! (layout, clipping, contrast, states) at realistic window sizes without
//! opening a window:
//!
//! ```sh
//! STREAMFORGE_UI_RENDER_DIR=/some/dir \
//!   cargo test -p streamforge-desktop --test render -- --ignored --nocapture
//! ```
//!
//! It drives the same `Model` and engine as the app. Rendering needs a wgpu
//! adapter (a GPU, or a software one), so the test is ignored by default.

mod support;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use command_api::{Command, CommandHandler};
use eframe::egui;
use eframe::egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use engine::Engine;
use serde_json::json;
use streamforge_desktop::app::DesktopApp;
use streamforge_desktop::model::{Model, SourceForm, SourceType};
use streamforge_desktop::worker::EngineWorker;
use support::{FRONT_CAMERA, ListedCameras, ListedDevices};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "streamforge-desktop-render-{}-{name}",
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

#[derive(Clone, Copy)]
enum Situation {
    /// Fresh start: the built-in scene, nothing running.
    Idle,
    /// A second scene with sources on program, two channels, a submix and
    /// a monitor mix in the mixer, the preview on, a recording running and
    /// a source selected.
    Live,
    /// Live, with the dialog for adding a source open.
    Dialog,
    /// Live, with the routing matrix instead of the console.
    Routing,
    /// Idle, with the project menu open.
    ProjectMenu,
    /// Idle, with the open dialog on a file that has problems.
    OpenProject,
    /// Idle, with the save dialog on a name that exists.
    SaveAs,
    /// Idle, with the routing matrix on channels that capture a
    /// microphone, desktop audio and an unplugged device.
    Inputs,
    /// Inputs, with the desktop audio channel's input dialog.
    InputDialog,
    /// InputDialog, with its list of devices open.
    InputList,
    /// Idle, with the stream settings open.
    StreamSettings,
    /// Idle, with the monitor settings and their list of devices open.
    MonitorList,
    /// The preview on, and the monitor switched on to a device that is
    /// not there: it ended.
    MonitorEnded,
    /// Live, with a scene's context menu open.
    SceneMenu,
    /// Live, with the health details and the activity log open.
    Health,
    /// Idle, with the recording settings open.
    RecordSettings,
    /// Live, with the question that closing the window asks.
    Quit,
    /// Idle, with the dialog for adding a source on an image and its list
    /// of files open.
    ImageDialog,
    /// Live, with the image source selected.
    ImageInspector,
    /// Idle, with the dialog for adding a source on a camera and its list
    /// of modes open.
    CameraDialog,
    /// Live, with the camera source selected.
    CameraInspector,
}

/// What the render does before it is taken, in order.
enum Step {
    Click(&'static str),
    /// A secondary click: a context menu.
    RightClick(&'static str),
    Type(&'static str),
    /// The window's close button.
    Close,
}

/// A project directory with a valid project, a broken one and two images.
fn project_files(dir: &Path) {
    // A badge: a horizontal gradient that fades out to the right.
    image::RgbaImage::from_fn(320, 120, |x, _| {
        let t = x as f32 / 319.0;
        image::Rgba([
            (40.0 + 200.0 * t) as u8,
            (160.0 - 60.0 * t) as u8,
            220,
            (255.0 * (1.0 - 0.6 * t)) as u8,
        ])
    })
    .save(dir.join("logo.png"))
    .expect("logo");
    image::RgbImage::from_pixel(1920, 1080, image::Rgb([30, 30, 30]))
        .save(dir.join("backdrop.png"))
        .expect("backdrop");
    let mut engine = Engine::new().with_project_dir(dir);
    let save = Command::decode("save_project", Some(&json!({"file_name": "show.json"})))
        .expect("save_project");
    engine.handle(save).expect("save");
    let broken = json!({"schema_version": 2, "canvas": {"width": 1920, "height": 1080},
        "active_scene": "scene_missing", "scenes": [{"id": "nope", "name": "", "sources": []}],
        "audio": {"channels": [], "program": {"gain_db": 99.0, "mute": false}}});
    std::fs::write(dir.join("broken.json"), broken.to_string()).expect("broken");
}

/// An engine whose mixer has a second channel, a submix assigned to
/// program and a separate monitor mix, so the console and the matrix show
/// every zone, both taps and a double-routing warning.
fn routed_engine(dir: PathBuf) -> Engine {
    let mut engine = engine(dir);
    let tone = |hz: f64| json!({"kind": "tone", "frequency_hz": hz, "level_dbfs": -18.0});
    for (name, params) in [
        (
            "add_channel",
            json!({"name": "Guest", "input": tone(330.0)}),
        ),
        ("add_bus", json!({"name": "Voice", "assign_db": -3.0})),
        ("add_bus", json!({"name": "Headphones"})),
        (
            "set_send",
            json!({"channel_name": "Guest", "bus_name": "Program", "enabled": false}),
        ),
        (
            "set_send",
            json!({"channel_name": "Guest", "bus_name": "Voice", "enabled": true}),
        ),
        (
            "set_send",
            json!({"channel_name": "Tone", "bus_name": "Voice", "enabled": true, "level_db": -6.0}),
        ),
        (
            "set_send",
            json!({"channel_name": "Guest", "bus_name": "Headphones", "enabled": true, "tap": "pre_fader"}),
        ),
        (
            "set_send",
            json!({"channel_name": "Tone", "bus_name": "Headphones", "enabled": true}),
        ),
        (
            "set_output_profile",
            json!({"output": "monitor", "bus_name": "Headphones"}),
        ),
        (
            "set_output_profile",
            json!({"output": "recording", "tracks": [{"bus_name": "Voice"}]}),
        ),
        (
            "set_monitor_device",
            json!({"device": "alsa_output.usb-headset"}),
        ),
    ] {
        let command = Command::decode(name, Some(&params)).expect(name);
        if let Err(error) = engine.handle(command) {
            panic!("{name}: {error:?}");
        }
    }
    engine
}

/// An engine that lists audio devices and cameras (`support`) and
/// captures only the cameras.
fn engine(dir: PathBuf) -> Engine {
    Engine::with_capture(capture_api::Capture {
        audio: Box::new(ListedDevices::new()),
        camera: Box::new(ListedCameras::default()),
        ..capture_api::Capture::unsupported("the tests capture nothing")
    })
    .with_output_dir(dir)
}

/// An idle engine whose monitor is on, on a device that is not listed.
fn monitor_engine(dir: PathBuf) -> Engine {
    let mut engine = engine(dir);
    for (name, params) in [
        (
            "set_monitor_device",
            json!({"device": "bluetooth_headphones"}),
        ),
        (
            "set_output_profile",
            json!({"output": "monitor", "enabled": true}),
        ),
    ] {
        let command = Command::decode(name, Some(&params)).expect(name);
        if let Err(error) = engine.handle(command) {
            panic!("{name}: {error:?}");
        }
    }
    engine
}

/// An idle engine with a channel on each kind of device input.
fn inputs_engine(dir: PathBuf) -> Engine {
    let mut engine = engine(dir);
    let device = |name: &str| json!({"kind": "device", "device": name});
    for (name, input) in [
        ("Host", device("alsa_input.usb-mic")),
        ("Desktop", device("speakers.monitor")),
        ("Guest", device("alsa_input.usb-headset")),
    ] {
        let params = json!({"name": name, "input": input});
        let command = Command::decode("add_channel", Some(&params)).expect("add_channel");
        if let Err(error) = engine.handle(command) {
            panic!("add_channel: {error:?}");
        }
    }
    engine
}

fn model(dir: &Path, situation: Situation) -> Model {
    let dir = dir.to_owned();
    let live = matches!(
        situation,
        Situation::Live
            | Situation::Dialog
            | Situation::Routing
            | Situation::SceneMenu
            | Situation::Health
            | Situation::Quit
            | Situation::ImageInspector
            | Situation::CameraInspector
    );
    project_files(&dir);
    let worker = EngineWorker::spawn(
        move || {
            if live {
                routed_engine(dir.clone()).with_project_dir(dir)
            } else if matches!(
                situation,
                Situation::Inputs | Situation::InputDialog | Situation::InputList
            ) {
                inputs_engine(dir.clone()).with_project_dir(dir)
            } else if matches!(situation, Situation::MonitorEnded) {
                monitor_engine(dir.clone()).with_project_dir(dir)
            } else {
                engine(dir.clone()).with_project_dir(dir)
            }
        },
        || {},
    )
    .expect("worker");
    let mut model = Model::new(worker);
    wait(&mut model, "the scene list", |m| m.scenes.is_some());
    if live {
        model.create_scene("Interview");
        wait(&mut model, "the new scene", |m| {
            m.scenes.as_ref().is_some_and(|list| list.scenes.len() == 2)
        });
        let interview = model
            .scenes
            .as_ref()
            .and_then(|list| list.scenes.iter().find(|scene| scene.name == "Interview"))
            .map(|scene| scene.id.clone())
            .expect("Interview");
        for (name, kind, color, rect) in [
            ("backdrop", SourceType::Solid, [24, 32, 48, 255], None),
            (
                "host cam",
                SourceType::Camera,
                [0, 0, 0, 255],
                Some([240.0, 140.0, 1280.0, 720.0]),
            ),
            (
                "bars",
                SourceType::ColorBars,
                [0, 0, 0, 255],
                Some([1200.0, 60.0, 640.0, 360.0]),
            ),
            (
                "lower third",
                SourceType::Solid,
                [220, 60, 40, 230],
                Some([80.0, 860.0, 900.0, 120.0]),
            ),
            (
                "logo",
                SourceType::Image,
                [0, 0, 0, 255],
                Some([1560.0, 40.0, 320.0, 120.0]),
            ),
        ] {
            model.add_source(
                &interview,
                &SourceForm {
                    name: name.to_owned(),
                    kind,
                    color,
                    rect,
                    opacity: 1.0,
                    screen_source: None,
                    image_file: (kind == SourceType::Image).then(|| "logo.png".to_owned()),
                    camera_id: (kind == SourceType::Camera).then(|| FRONT_CAMERA.to_owned()),
                    camera_mode: None,
                },
            );
        }
        wait(&mut model, "the sources", |m| {
            m.scenes.as_ref().is_some_and(|list| {
                list.scenes
                    .iter()
                    .any(|scene| scene.name == "Interview" && scene.sources.len() == 5)
            })
        });
        // One hidden layer, to show its row.
        let bars = |m: &Model| {
            m.scenes
                .as_ref()
                .and_then(|list| list.scenes.iter().find(|scene| scene.name == "Interview"))
                .and_then(|scene| scene.sources.iter().find(|source| source.name == "bars"))
                .cloned()
                .expect("bars")
        };
        let id = bars(&model).id;
        model.set_source_visible(&id, false);
        wait(&mut model, "the hidden bars", |m| !bars(m).visible);
        model.activate_scene(&interview, "Interview");
        wait(&mut model, "the scene on program", |m| {
            m.scenes
                .as_ref()
                .is_some_and(|list| list.active_scene_name == "Interview")
        });
        model.set_preview(true);
        wait(&mut model, "the preview", |m| m.preview_enabled);
        model.start_recording("render.mkv");
        wait(&mut model, "the recording", |m| m.recording.is_some());
    }
    if matches!(situation, Situation::MonitorEnded) {
        model.set_preview(true);
        wait(&mut model, "the preview", |m| m.preview_enabled);
    }
    model
}

fn render(out: &Path, name: &str, situation: Situation, size: [f32; 2]) {
    let dir = TempDir::new(name);
    let model = model(&dir.0, situation);
    let mut harness = Harness::builder().with_size(size).build_eframe(move |cc| {
        DesktopApp::new(&cc.egui_ctx, model).with_project_folder("/home/operator/shows")
    });
    // Let the polls answer and the preview and meters fill in.
    for _ in 0..60 {
        harness.step();
        thread::sleep(Duration::from_millis(16));
    }
    let steps = match situation {
        Situation::Idle => vec![],
        // Open a source in the inspector (a drawer in narrow windows).
        Situation::Live => vec![Step::Click("lower third")],
        Situation::Dialog => vec![Step::Click("Add a source to this scene")],
        Situation::Routing => vec![Step::Click("Routing")],
        Situation::ProjectMenu => vec![Step::Click("Project Untitled")],
        Situation::OpenProject => vec![
            Step::Click("Project Untitled"),
            Step::Click("Open project…"),
            Step::Click("broken.json"),
        ],
        Situation::SaveAs => vec![
            Step::Click("Project Untitled"),
            Step::Click("Save as…"),
            Step::Type("show"),
        ],
        Situation::Inputs => vec![Step::Click("Routing")],
        Situation::InputDialog => vec![Step::Click("Routing"), Step::Click("Input of Desktop")],
        Situation::InputList => vec![
            Step::Click("Routing"),
            Step::Click("Input of Desktop"),
            Step::Click("Audio input"),
        ],
        Situation::StreamSettings => vec![Step::Click("Stream settings")],
        Situation::MonitorList => {
            vec![Step::Click("Monitor settings"), Step::Click("Audio output")]
        }
        Situation::MonitorEnded => vec![],
        Situation::SceneMenu => vec![Step::RightClick("Test pattern")],
        Situation::Health => vec![Step::Click("All healthy")],
        Situation::RecordSettings => vec![Step::Click("Recording settings")],
        Situation::Quit => vec![Step::Click("lower third"), Step::Close],
        Situation::ImageDialog => vec![
            Step::Click("Add a source to this scene"),
            Step::Click("Image"),
            Step::Click("Image file"),
        ],
        Situation::ImageInspector => vec![Step::Click("logo")],
        Situation::CameraDialog => vec![
            Step::Click("Add a source to this scene"),
            Step::Click("Camera"),
            Step::Click("Camera device"),
            Step::Click("Front Camera"),
            Step::Click("Camera mode"),
        ],
        Situation::CameraInspector => vec![Step::Click("host cam")],
    };
    for step in steps {
        match step {
            Step::Click(label) => harness.get_by_label(label).click(),
            Step::RightClick(label) => harness.get_by_label(label).click_secondary(),
            Step::Type(text) => {
                let field = || harness.get_all_by_role(Role::TextInput).next();
                field().expect("a text field").focus();
                harness.step();
                harness
                    .get_all_by_role(Role::TextInput)
                    .next()
                    .expect("a text field")
                    .type_text(text);
            }
            Step::Close => harness
                .input_mut()
                .viewports
                .entry(egui::ViewportId::ROOT)
                .or_default()
                .events
                .push(egui::ViewportEvent::Close),
        }
        for _ in 0..30 {
            harness.step();
            thread::sleep(Duration::from_millis(16));
        }
    }
    let image = harness.render().expect("render");
    let path = out.join(format!("{name}.png"));
    image.save(&path).expect("save");
    println!("wrote {}", path.display());
}

#[test]
#[ignore = "renders with wgpu into STREAMFORGE_UI_RENDER_DIR; run by hand"]
fn render_the_workspace() {
    let Some(out) = std::env::var_os("STREAMFORGE_UI_RENDER_DIR").map(PathBuf::from) else {
        eprintln!("set STREAMFORGE_UI_RENDER_DIR to render");
        return;
    };
    std::fs::create_dir_all(&out).expect("output dir");
    for (size, label) in [
        ([1280.0, 720.0], "1280x720"),
        ([1400.0, 900.0], "1400x900"),
        ([1920.0, 1080.0], "1920x1080"),
    ] {
        render(&out, &format!("idle-{label}"), Situation::Idle, size);
        render(&out, &format!("live-{label}"), Situation::Live, size);
        render(&out, &format!("routing-{label}"), Situation::Routing, size);
        render(&out, &format!("inputs-{label}"), Situation::Inputs, size);
    }
    render(
        &out,
        "input-dialog-1400x900",
        Situation::InputDialog,
        [1400.0, 900.0],
    );
    render(
        &out,
        "input-list-1400x900",
        Situation::InputList,
        [1400.0, 900.0],
    );
    render(&out, "dialog-1400x900", Situation::Dialog, [1400.0, 900.0]);
    render(
        &out,
        "project-menu-1280x720",
        Situation::ProjectMenu,
        [1280.0, 720.0],
    );
    render(
        &out,
        "open-project-1400x900",
        Situation::OpenProject,
        [1400.0, 900.0],
    );
    render(&out, "save-as-1400x900", Situation::SaveAs, [1400.0, 900.0]);
    render(
        &out,
        "stream-settings-1400x900",
        Situation::StreamSettings,
        [1400.0, 900.0],
    );
    render(
        &out,
        "monitor-list-1400x900",
        Situation::MonitorList,
        [1400.0, 900.0],
    );
    render(
        &out,
        "scene-menu-1280x720",
        Situation::SceneMenu,
        [1280.0, 720.0],
    );
    render(&out, "health-1400x900", Situation::Health, [1400.0, 900.0]);
    render(
        &out,
        "record-settings-1400x900",
        Situation::RecordSettings,
        [1400.0, 900.0],
    );
    render(&out, "quit-1400x900", Situation::Quit, [1400.0, 900.0]);
    render(
        &out,
        "image-dialog-1400x900",
        Situation::ImageDialog,
        [1400.0, 900.0],
    );
    render(
        &out,
        "image-inspector-1400x900",
        Situation::ImageInspector,
        [1400.0, 900.0],
    );
    render(
        &out,
        "camera-dialog-1400x900",
        Situation::CameraDialog,
        [1400.0, 900.0],
    );
    render(
        &out,
        "camera-inspector-1400x900",
        Situation::CameraInspector,
        [1400.0, 900.0],
    );
    for (size, label) in [
        ([1280.0, 720.0], "1280x720"),
        ([1920.0, 1080.0], "1920x1080"),
    ] {
        render(
            &out,
            &format!("monitor-ended-{label}"),
            Situation::MonitorEnded,
            size,
        );
    }
}

/// The design system's specimen: tokens and every component in its states,
/// drawn by the real code (`src/ui`).
fn specimen(ui: &mut egui::Ui) {
    use egui::{Color32, RichText, Vec2, vec2};
    use streamforge_desktop::ui::icons::Icon;
    use streamforge_desktop::ui::theme::{TextRole, color};
    use streamforge_desktop::ui::widgets::{
        self, BadgeKind, Button, ButtonKind, ListRow, MenuItem, StatusLevel, Transport,
        TransportButton, TransportState, VisualState,
    };

    egui::Frame::new()
        .fill(color::BG_PANEL)
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.columns(3, |columns| {
                let ui = &mut columns[0];
                widgets::section_header(ui, "Colour", None);
                for (name, value) in [
                    ("bg.app", color::BG_APP),
                    ("bg.panel", color::BG_PANEL),
                    ("bg.raised", color::BG_RAISED),
                    ("bg.inset", color::BG_INSET),
                    ("line.subtle", color::LINE_SUBTLE),
                    ("line.strong", color::LINE_STRONG),
                    ("text.primary", color::TEXT_PRIMARY),
                    ("text.secondary", color::TEXT_SECONDARY),
                    ("text.tertiary", color::TEXT_TERTIARY),
                    ("accent", color::ACCENT),
                    ("tally.program", color::TALLY_PROGRAM),
                    ("state.ok", color::STATE_OK),
                    ("state.warn", color::STATE_WARN),
                    ("state.error", color::STATE_ERROR),
                ] {
                    ui.horizontal(|ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(vec2(36.0, 18.0), egui::Sense::hover());
                        ui.painter().rect(
                            rect,
                            2.0,
                            value,
                            egui::Stroke::new(1.0, color::LINE_STRONG),
                            egui::StrokeKind::Inside,
                        );
                        ui.label(TextRole::Body.text(name));
                        ui.label(
                            TextRole::Numeric
                                .text(format!(
                                    "#{:02X}{:02X}{:02X}",
                                    value.r(),
                                    value.g(),
                                    value.b()
                                ))
                                .color(color::TEXT_TERTIARY),
                        );
                    });
                }
                ui.add_space(12.0);
                widgets::section_header(ui, "Type", None);
                ui.label(TextRole::Title.text("Title — Interview, take 2"));
                ui.label(TextRole::Section.text("Section — sources"));
                ui.label(TextRole::Body.text("Body — lower third, solid colour"));
                ui.label(TextRole::Label.text("Label — opacity"));
                ui.label(TextRole::Meta.text("Meta — 1920×1080 · 60 fps · h264_vaapi"));
                ui.label(TextRole::Numeric.text("Numeric — −18.4 dB  2.14 ms  1 024"));
                ui.label(TextRole::Timecode.text("01:12:04"));
                ui.add_space(12.0);
                widgets::section_header(ui, "Icons", Some(Icon::ALL.len()));
                ui.horizontal_wrapped(|ui| {
                    for icon in Icon::ALL {
                        let (rect, _) =
                            ui.allocate_exact_size(Vec2::splat(28.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 3.0, color::BG_RAISED);
                        icon.paint(
                            ui.painter(),
                            egui::Rect::from_center_size(rect.center(), Vec2::splat(16.0)),
                            color::TEXT_SECONDARY,
                        );
                    }
                });

                let ui = &mut columns[1];
                widgets::section_header(ui, "Buttons", None);
                for kind in [
                    ButtonKind::Primary,
                    ButtonKind::Secondary,
                    ButtonKind::Ghost,
                    ButtonKind::Danger,
                ] {
                    ui.horizontal(|ui| {
                        for (state, label) in [
                            (VisualState::Normal, "Normal"),
                            (VisualState::Hovered, "Hover"),
                            (VisualState::Focused, "Focus"),
                            (VisualState::Disabled, "Disabled"),
                        ] {
                            ui.add(Button::new(kind, label).forced(state));
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.add(Button::new(ButtonKind::Secondary, "Add source").icon(Icon::Plus));
                    ui.add(
                        Button::new(ButtonKind::Secondary, "Restart")
                            .icon(Icon::Restart)
                            .busy(true),
                    );
                    ui.add(Button::new(ButtonKind::Danger, "Remove").icon(Icon::Trash));
                });
                ui.horizontal(|ui| {
                    widgets::menu_button(ui, Icon::File, "show.json", false, "Menu", 240.0);
                    widgets::menu_button(ui, Icon::File, "show.json", true, "Marked", 240.0);
                    widgets::menu_button(
                        ui,
                        Icon::File,
                        "a project with a very long file name.json",
                        true,
                        "Cut",
                        200.0,
                    );
                });
                ui.add_space(12.0);
                widgets::section_header(ui, "Transport", None);
                for (transport, state) in [
                    (Transport::Record, TransportState::Idle),
                    (Transport::Record, TransportState::Running(4324)),
                    (Transport::Live, TransportState::Starting),
                    (Transport::Live, TransportState::Running(351)),
                    (Transport::Live, TransportState::Failed),
                ] {
                    ui.add(TransportButton::new(transport, state));
                }
                ui.add(
                    TransportButton::new(Transport::Record, TransportState::Idle)
                        .forced(VisualState::Focused),
                );
                ui.add_space(12.0);
                widgets::section_header(ui, "Badges and status", None);
                ui.horizontal(|ui| {
                    widgets::badge(ui, BadgeKind::Tally, "rec");
                    widgets::badge(ui, BadgeKind::Tally, "live");
                    widgets::badge(ui, BadgeKind::Tally, "pgm");
                    widgets::badge(ui, BadgeKind::Neutral, "preview");
                    widgets::badge(ui, BadgeKind::Ok, "connected");
                    widgets::badge(ui, BadgeKind::Warn, "reconnecting");
                    widgets::badge(ui, BadgeKind::Error, "failed");
                });
                widgets::status_line(ui, StatusLevel::Ok, "Stream connected · 6 000 kb/s");
                widgets::status_line(
                    ui,
                    StatusLevel::Warn,
                    "Encoder fell behind: 12 frames dropped",
                );
                widgets::status_line(
                    ui,
                    StatusLevel::Error,
                    "Capture ended: the window was closed",
                );
                widgets::status_line(ui, StatusLevel::Info, "Health from 2 s ago");
                ui.add_space(12.0);
                widgets::section_header(ui, "Controls", None);
                ui.horizontal(|ui| {
                    let mut on = true;
                    widgets::toggle(ui, &mut on, "preview");
                    ui.label(TextRole::Body.text("Preview"));
                    let mut off = false;
                    widgets::toggle(ui, &mut off, "mute");
                    ui.label(TextRole::Body.text("Mute"));
                    ui.add_enabled_ui(false, |ui| {
                        widgets::toggle(ui, &mut on, "disabled on");
                        widgets::toggle(ui, &mut off, "disabled off");
                    });
                });
                // A send cell's parts, the mix choice and the view switch.
                ui.horizontal(|ui| {
                    widgets::radio(ui, true, "takes Program");
                    widgets::radio(ui, false, "takes Voice");
                    ui.add_enabled_ui(false, |ui| widgets::radio(ui, true, "disabled"));
                    widgets::chip(ui, "post", false, "after the fader");
                    widgets::chip(ui, "pre", true, "before the fader");
                    widgets::chip(ui, "T2", true, "track 2");
                    ui.add_enabled_ui(false, |ui| widgets::chip(ui, "track", false, "full"));
                });
                widgets::tabs(ui, 0, &["Console", "Routing"]);
                // A channel's input in the matrix.
                ui.horizontal(|ui| {
                    widgets::select_field(
                        ui,
                        Icon::Mic,
                        color::TEXT_SECONDARY,
                        "USB Mic",
                        "Input",
                        160.0,
                    );
                    widgets::select_field(
                        ui,
                        Icon::Warning,
                        color::STATE_WARN,
                        "alsa_input.usb-headset",
                        "Missing",
                        160.0,
                    );
                    ui.add_enabled_ui(false, |ui| {
                        widgets::select_field(
                            ui,
                            Icon::Speaker,
                            color::TEXT_SECONDARY,
                            "Monitor of Speakers",
                            "Disabled",
                            160.0,
                        )
                    });
                });
                // Choices made in place: a dropdown, segments, a slider.
                ui.horizontal(|ui| {
                    widgets::dropdown(ui, "System default output", "Output", 200.0, |_| {});
                    ui.add_enabled_ui(false, |ui| {
                        widgets::dropdown(ui, "USB Headset", "Disabled", 140.0, |_| {})
                    });
                });
                ui.horizontal(|ui| {
                    let mut kind = 0;
                    widgets::segmented(
                        ui,
                        &mut kind,
                        &[(0, "Auto"), (1, "Software"), (2, "Hardware")],
                    );
                    ui.add_enabled_ui(false, |ui| {
                        widgets::segmented(ui, &mut kind, &[(0, "Device"), (1, "Test tone")])
                    });
                });
                ui.horizontal(|ui| {
                    let mut opacity = 70.0;
                    widgets::slider(ui, &mut opacity, 0.0..=100.0, 1.0, 180.0, "opacity");
                    ui.add_enabled_ui(false, |ui| {
                        widgets::slider(ui, &mut opacity, 0.0..=100.0, 1.0, 120.0, "disabled")
                    });
                });
                let mut text = "recording.mkv".to_owned();
                ui.add(egui::TextEdit::singleline(&mut text).desired_width(220.0));
                // A menu: icons, the current choice, a shortcut, states.
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(240.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.add(
                        MenuItem::new("Open project…")
                            .icon(Icon::Folder)
                            .shortcut("Ctrl+O"),
                    );
                    ui.add(
                        MenuItem::new("Save")
                            .icon(Icon::File)
                            .shortcut("Ctrl+S")
                            .forced(VisualState::Hovered),
                    );
                    ui.add(
                        MenuItem::new("Save as…")
                            .shortcut("Ctrl+Shift+S")
                            .enabled(false),
                    );
                    widgets::menu_heading(ui, "Desktop audio");
                    ui.add(MenuItem::new("Monitor of Speakers").selected(true));
                    ui.add(MenuItem::new("Monitor of USB Headset").selected(false));
                    ui.add(MenuItem::new("Delete…").icon(Icon::Trash).danger(true));
                });

                let ui = &mut columns[2];
                widgets::section_header(ui, "Scenes", Some(3));
                ui.add(
                    ListRow::new(Icon::Scene, "Interview")
                        .program(true)
                        .selected(true),
                );
                ui.add(ListRow::new(Icon::Scene, "Be right back").forced(VisualState::Hovered));
                ui.add(ListRow::new(Icon::Scene, "Starting soon").meta("4 sources"));
                // In a 220 px dock, as at small window sizes: the name is cut.
                let (dock, _) = ui.allocate_exact_size(vec2(220.0, 26.0), egui::Sense::hover());
                ui.scope_builder(egui::UiBuilder::new().max_rect(dock), |ui| {
                    ui.add(
                        ListRow::new(Icon::Scene, "Interview with a guest whose name is long")
                            .meta("12 sources")
                            .program(true),
                    );
                });
                ui.add_space(8.0);
                widgets::section_header(ui, "Sources", Some(3));
                ui.add(
                    ListRow::new(Icon::Solid, "lower third")
                        .meta("solid")
                        .selected(true),
                );
                ui.add(ListRow::new(Icon::Screen, "desk").meta("screen"));
                ui.add(
                    ListRow::new(Icon::Bars, "bars")
                        .meta("color bars")
                        .forced(VisualState::Focused),
                );
                ui.add_space(12.0);
                widgets::section_header(ui, "Mixer", None);
                ui.horizontal(|ui| {
                    let height = 180.0;
                    widgets::meter_scale(ui, height);
                    for (level, peak, clipped, mut gain) in [
                        (-24.0_f32, -16.0_f32, false, 0.0_f32),
                        (-9.0, -4.0, false, -6.0),
                        (-2.0, 0.0, true, 3.0),
                        (-60.0, -60.0, false, -60.0),
                    ] {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                widgets::meter(ui, vec2(8.0, height), level, peak, clipped);
                                widgets::meter(
                                    ui,
                                    vec2(8.0, height),
                                    level - 2.0,
                                    peak - 1.0,
                                    clipped,
                                );
                                widgets::fader(ui, height, &mut gain, -60.0..=12.0, "channel");
                            });
                            ui.label(
                                TextRole::Numeric
                                    .text(format!("{level:.0}"))
                                    .color(color::TEXT_SECONDARY),
                            );
                        });
                    }
                });
                widgets::meter(ui, vec2(220.0, 8.0), -14.0, -9.0, false);
                ui.label(RichText::new(" ").color(Color32::TRANSPARENT));
            });
        });
}

#[test]
#[ignore = "renders with wgpu into STREAMFORGE_UI_RENDER_DIR; run by hand"]
fn render_the_design_system_specimen() {
    let Some(out) = std::env::var_os("STREAMFORGE_UI_RENDER_DIR").map(PathBuf::from) else {
        eprintln!("set STREAMFORGE_UI_RENDER_DIR to render");
        return;
    };
    std::fs::create_dir_all(&out).expect("output dir");
    let mut themed = false;
    let mut harness = Harness::builder()
        .with_size([1400.0, 1200.0])
        .build_ui(move |ui| {
            // Fonts take effect from the next pass, as at app start.
            if !themed {
                streamforge_desktop::ui::theme::apply(ui.ctx());
                themed = true;
                return;
            }
            specimen(ui);
        });
    for _ in 0..5 {
        harness.step();
    }
    let path = out.join("specimen.png");
    harness.render().expect("render").save(&path).expect("save");
    println!("wrote {}", path.display());
}
