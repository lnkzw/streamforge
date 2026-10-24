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

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use engine::Engine;
use streamforge_desktop::app::DesktopApp;
use streamforge_desktop::model::{Model, SourceForm, SourceType};
use streamforge_desktop::worker::EngineWorker;

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
    /// A second scene with sources on program, the default tone in the
    /// mixer, the preview on, a recording running and a source selected.
    Live,
    /// Live, with the dialog for adding a source open.
    Dialog,
}

fn model(dir: &Path, situation: Situation) -> Model {
    let dir = dir.to_owned();
    let worker =
        EngineWorker::spawn(move || Engine::new().with_output_dir(dir), || {}).expect("worker");
    let mut model = Model::new(worker);
    wait(&mut model, "the scene list", |m| m.scenes.is_some());
    if let Situation::Live | Situation::Dialog = situation {
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
                },
            );
        }
        wait(&mut model, "the sources", |m| {
            m.scenes.as_ref().is_some_and(|list| {
                list.scenes
                    .iter()
                    .any(|scene| scene.name == "Interview" && scene.sources.len() == 3)
            })
        });
        model.activate_scene(&interview, "Interview");
        wait(&mut model, "the scene on program", |m| {
            m.scenes
                .as_ref()
                .is_some_and(|list| list.active == "Interview")
        });
        model.set_preview(true);
        wait(&mut model, "the preview", |m| m.preview_enabled);
        model.start_recording("render.mkv");
        wait(&mut model, "the recording", |m| m.recording.is_some());
    }
    model
}

fn render(out: &Path, name: &str, situation: Situation, size: [f32; 2]) {
    let dir = TempDir::new(name);
    let model = model(&dir.0, situation);
    let mut harness = Harness::builder()
        .with_size(size)
        .build_eframe(move |cc| DesktopApp::new(&cc.egui_ctx, model));
    // Let the polls answer and the preview and meters fill in.
    for _ in 0..60 {
        harness.step();
        thread::sleep(Duration::from_millis(16));
    }
    let action = match situation {
        Situation::Idle => None,
        // Open a source in the inspector (a drawer in narrow windows).
        Situation::Live => Some("lower third"),
        Situation::Dialog => Some("Add a source to this scene"),
    };
    if let Some(label) = action {
        harness.get_by_label(label).click();
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
    }
    render(&out, "dialog-1400x900", Situation::Dialog, [1400.0, 900.0]);
}

/// The design system's specimen: tokens and every component in its states,
/// drawn by the real code (`src/ui`).
fn specimen(ui: &mut egui::Ui) {
    use egui::{Color32, RichText, Vec2, vec2};
    use streamforge_desktop::ui::icons::Icon;
    use streamforge_desktop::ui::theme::{TextRole, color};
    use streamforge_desktop::ui::widgets::{
        self, BadgeKind, Button, ButtonKind, ListRow, StatusLevel, Transport, TransportButton,
        TransportState, VisualState,
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
                });
                let mut text = "recording.mkv".to_owned();
                ui.add(egui::TextEdit::singleline(&mut text).desired_width(220.0));

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
        .with_size([1400.0, 900.0])
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
