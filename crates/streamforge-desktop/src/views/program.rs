//! The program dock: the monitor, as large as the space allows at the
//! canvas's aspect ratio, with a tally frame while an output runs, and one
//! line of technical facts under it.

use command_api::{SourceKind, SourceSpec};
use eframe::egui;
use eframe::egui::emath::GuiRounding as _;

use crate::app::DesktopApp;
use crate::model::SourceHealth;
use crate::ui::icons::Icon;
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets::{self, BadgeKind, Button, ButtonKind, Transport};
use crate::workspace::{self, Shortcut, fit};

use super::{right_to_left, scene, with_keys};

/// The line of facts under the monitor.
const FACTS_HEIGHT: f32 = 22.0;

impl DesktopApp {
    /// Returns where the header ends: a drawer opens below it, so the
    /// on-air marks and the preview switch stay in view. In a `narrow`
    /// window the header also opens and closes that drawer.
    pub(crate) fn program_dock(&mut self, ui: &mut egui::Ui, narrow: bool) -> f32 {
        let recording = self.on_air(Transport::Record);
        let live = self.on_air(Transport::Live);
        let program_name = self
            .model
            .scenes
            .as_ref()
            .map(|scenes| scenes.active_scene_name.clone())
            .unwrap_or_default();

        let actions = widgets::section_header(ui, "Program", None);
        right_to_left(ui, actions, |ui| {
            ui.spacing_mut().item_spacing.x = size::SPACE_6;
            if narrow {
                self.drawer_button(ui);
                ui.add_space(size::SPACE_12);
            }
            let mut preview = self.model.preview_enabled;
            if widgets::toggle(ui, &mut preview, "Preview")
                .on_hover_ui(|ui| {
                    ui.label(with_keys(
                        ui.ctx(),
                        "Start or stop the preview",
                        Shortcut::Preview,
                    ));
                })
                .changed()
            {
                self.model.set_preview(preview);
            }
            ui.label(TextRole::Label.text("Preview"));
            ui.add_space(size::SPACE_12);
            if live {
                widgets::badge(ui, BadgeKind::Tally, "live");
            }
            if recording {
                widgets::badge(ui, BadgeKind::Tally, "rec");
            }
            if !recording && !live && self.model.preview_enabled {
                widgets::badge(ui, BadgeKind::Neutral, "preview");
            }
            // The scene on air, by name, left of the marks.
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(
                    egui::Label::new(
                        TextRole::Body
                            .text(&program_name)
                            .color(color::TEXT_SECONDARY),
                    )
                    .truncate(),
                );
            });
        });

        let header_bottom = actions.bottom();
        let (canvas_width, canvas_height) = self.canvas();
        let space = ui.available_rect_before_wrap();
        let space = egui::Rect::from_min_max(
            space.min,
            egui::pos2(space.right(), space.bottom() - FACTS_HEIGHT),
        );
        // Centred across, at the top, so the facts sit right under it.
        let monitor = fit(
            space.shrink(size::STROKE_HEAVY),
            canvas_width,
            canvas_height,
        );
        let monitor = monitor.translate(egui::vec2(
            0.0,
            space.top() + size::STROKE_HEAVY - monitor.top(),
        ));
        // The preview asked for at the monitor's size is drawn 1:1, so it
        // stays sharp at any window size and display scaling.
        let (monitor, pixels) = on_pixels(monitor, ui.ctx().pixels_per_point());
        self.model.preview_size_wanted = pixels;
        // A click selects the source under the pointer. Not focusable: the
        // monitor has nothing to do by keyboard.
        let response = ui.allocate_rect(space, egui::Sense::CLICK);
        let marks = self.paint_monitor(ui, monitor, recording || live);
        let response = match response
            .hover_pos()
            .and_then(|pointer| marks.iter().rev().find(|(mark, _)| mark.contains(pointer)))
        {
            Some((_, why)) => response.on_hover_text_at_pointer(why),
            None => response,
        };
        if response.clicked()
            && let Some(pointer) = response.interact_pointer_pos()
            && monitor.contains(pointer)
        {
            self.select_at(monitor, pointer);
        }

        // Facts, aligned with the picture.
        let facts = egui::Rect::from_min_size(
            egui::pos2(monitor.left(), monitor.bottom() + size::STROKE_HEAVY),
            egui::vec2(monitor.width(), FACTS_HEIGHT),
        );
        let text = self.facts(canvas_width, canvas_height);
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(facts)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.add(
                    egui::Label::new(TextRole::Numeric.text(text).color(color::TEXT_TERTIARY))
                        .truncate(),
                );
            },
        );
        header_bottom
    }

    /// Returns the marks of sources whose capture ended, each with why.
    fn paint_monitor(
        &self,
        ui: &egui::Ui,
        monitor: egui::Rect,
        on_air: bool,
    ) -> Vec<(egui::Rect, String)> {
        let painter = ui.painter();
        painter.rect_filled(monitor, 0.0, color::BG_INSET);
        let mut marks = Vec::new();
        match (&self.preview, self.model.preview_enabled) {
            (Some(texture), true) => {
                painter.image(
                    texture.id(),
                    picture_rect(monitor, texture.size(), ui.ctx().pixels_per_point()),
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
                marks = self.paint_no_picture(ui, monitor);
                self.paint_selection(ui, monitor);
            }
            _ => {
                let center = monitor.center();
                Icon::Preview.paint(
                    painter,
                    egui::Rect::from_center_size(
                        center - egui::vec2(0.0, 18.0),
                        egui::Vec2::splat(32.0),
                    ),
                    color::TEXT_TERTIARY,
                );
                painter.text(
                    center + egui::vec2(0.0, 12.0),
                    egui::Align2::CENTER_CENTER,
                    "Preview off — switch it on to see the program",
                    TextRole::Meta.font(),
                    color::TEXT_TERTIARY,
                );
            }
        }
        if on_air {
            painter.rect_stroke(
                monitor,
                0.0,
                egui::Stroke::new(size::STROKE_HEAVY, color::TALLY_PROGRAM),
                egui::StrokeKind::Outside,
            );
        }
        marks
    }

    /// Mark where the program scene's sources whose capture ended are (see
    /// [`ended_captures`]): a dashed frame and, where it fits, a label.
    /// Only in the app; the program and every output stay as they are.
    /// Returns each mark with its tooltip.
    fn paint_no_picture(&self, ui: &egui::Ui, monitor: egui::Rect) -> Vec<(egui::Rect, String)> {
        let Some(scenes) = &self.model.scenes else {
            return Vec::new();
        };
        let Some(program) = scene(scenes, &scenes.active_scene_id) else {
            return Vec::new();
        };
        let scale = monitor.width() / scenes.canvas_width.max(1) as f32;
        let painter = ui.painter_at(monitor);
        let mut marks = Vec::new();
        for ended in ended_captures(&program.sources, |id| self.model.source_health(id)) {
            let rect = on_monitor(monitor, scale, ended.source);
            if !rect.is_positive() {
                continue;
            }
            let frame = rect.shrink(size::STROKE / 2.0);
            painter.extend(egui::Shape::dashed_line(
                &[
                    frame.left_top(),
                    frame.right_top(),
                    frame.right_bottom(),
                    frame.left_bottom(),
                    frame.left_top(),
                ],
                egui::Stroke::new(size::STROKE, color::STATE_WARN),
                6.0,
                4.0,
            ));
            paint_no_picture_label(&painter, rect, &ended);
            let what = if ended.empty {
                "has no picture"
            } else {
                "stopped, its last frame stays"
            };
            marks.push((
                rect,
                format!(
                    "`{}` {what}: {}\nSelect it and choose Restart capture in the inspector.",
                    ended.source.name,
                    workspace::short_reason(&ended.reason)
                ),
            ));
        }
        marks
    }

    /// Outline the selected source when its scene is the one on air, so
    /// the inspector's subject can be found in the picture.
    fn paint_selection(&self, ui: &egui::Ui, monitor: egui::Rect) {
        let Some(scenes) = &self.model.scenes else {
            return;
        };
        let (Some(scene_id), Some(source_id)) = (&self.selected_scene, &self.selected_source)
        else {
            return;
        };
        if *scene_id != scenes.active_scene_id {
            return;
        }
        let Some(source) =
            scene(scenes, scene_id).and_then(|scene| super::source(scene, source_id))
        else {
            return;
        };
        let scale = monitor.width() / scenes.canvas_width.max(1) as f32;
        let rect = on_monitor(monitor, scale, source);
        if rect.is_positive() {
            ui.painter().rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(size::STROKE, color::ACCENT),
                egui::StrokeKind::Inside,
            );
        }
    }

    /// In a narrow window, the inspector is a drawer: this opens it on the
    /// selected source after it was closed, and closes it.
    fn drawer_button(&mut self, ui: &mut egui::Ui) {
        // The same condition the drawer is shown on: it may show its
        // empty state for a source that is gone, and this still closes it.
        let selected = self.selected_source.is_some();
        let open = selected && self.drawer_open;
        let hint = if open {
            "Close the inspector (Esc)"
        } else {
            "Open the selected source in the inspector"
        };
        if ui
            .add(
                Button::new(ButtonKind::Ghost, "Inspector")
                    .icon(Icon::Layers)
                    .enabled(selected),
            )
            .on_hover_text(hint)
            .on_disabled_hover_text("Select a source to open it in the inspector")
            .clicked()
        {
            self.drawer_open = !open;
        }
    }

    /// A click on the picture selects the topmost visible source under it
    /// in the program scene.
    fn select_at(&mut self, monitor: egui::Rect, pointer: egui::Pos2) {
        let Some(scenes) = &self.model.scenes else {
            return;
        };
        let scale = scenes.canvas_width.max(1) as f32 / monitor.width();
        let at = (pointer - monitor.min) * scale;
        let Some(program) = scene(scenes, &scenes.active_scene_id) else {
            return;
        };
        let hit = workspace::source_at(&program.sources, at.x, at.y);
        let program_id = program.id.clone();
        let hit = hit.map(|source| source.id.clone());
        if hit.is_some() {
            self.selected_scene = Some(program_id);
        }
        self.select_source(hit);
    }

    /// Canvas, measured frame rate and render time.
    fn facts(&self, width: u32, height: u32) -> String {
        let mut facts = vec![format!("{width}×{height}")];
        if let Some(render) = self
            .model
            .health
            .as_ref()
            .and_then(|health| health.render.as_ref())
        {
            if render.elapsed_ms > 0 {
                let fps = render.frames_rendered as f64 * 1000.0 / render.elapsed_ms as f64;
                facts.push(format!("{fps:.1} fps"));
            }
            if let Some(time) = &render.render_time {
                facts.push(format!("render p95 {:.1} ms", time.p95_ms));
            }
            facts.push(format!("{} late", render.late_frames));
        }
        facts.join(" · ")
    }
}

/// Where `source` is on `monitor`, which shows the canvas at `scale`
/// points per canvas pixel; cut to the monitor.
fn on_monitor(monitor: egui::Rect, scale: f32, source: &SourceSpec) -> egui::Rect {
    egui::Rect::from_min_size(
        monitor.min + egui::vec2(source.rect.x, source.rect.y) * scale,
        egui::vec2(source.rect.width, source.rect.height) * scale,
    )
    .intersect(monitor)
}

/// How the engine's health begins the end reason of a camera that did not
/// open (`docs/protocol.md`, Cameras): its source draws nothing.
const NOT_STARTED: &str = "could not start: ";

/// A visible screen or camera source whose capture ended.
#[derive(Debug, PartialEq)]
struct EndedCapture<'a> {
    source: &'a SourceSpec,
    reason: String,
    /// It did not start (or a restart failed), so it draws nothing; a
    /// capture that ended while it ran keeps its last frame.
    empty: bool,
}

/// The visible screen and camera sources of a scene (in drawing order)
/// whose capture ended, with why.
fn ended_captures(
    sources: &[SourceSpec],
    health: impl Fn(&str) -> SourceHealth,
) -> Vec<EndedCapture<'_>> {
    sources
        .iter()
        .filter(|source| {
            source.visible
                && matches!(
                    source.kind,
                    SourceKind::Screen { .. } | SourceKind::Camera { .. }
                )
        })
        .filter_map(|source| match health(&source.id) {
            SourceHealth::Ended { reason } => Some(EndedCapture {
                source,
                empty: reason.starts_with(NOT_STARTED),
                reason,
            }),
            SourceHealth::NotCapturing | SourceHealth::Capturing { .. } => None,
        })
        .collect()
}

/// A label in the middle of `rect`: the source's kind and "`name`: no
/// picture" (or "stopped" over its last frame), cut to fit; only the icon
/// where the text does not fit, and nothing where that does not either.
fn paint_no_picture_label(painter: &egui::Painter, rect: egui::Rect, ended: &EndedCapture<'_>) {
    let source = ended.source;
    let pad = size::SPACE_6;
    let height = size::CONTROL;
    if rect.width() < height || rect.height() < height {
        return;
    }
    let icon = if matches!(source.kind, SourceKind::Camera { .. }) {
        Icon::Camera
    } else {
        Icon::Screen
    };
    let room = rect.width() - 2.0 * size::SPACE_4 - 2.0 * pad - size::ICON - pad;
    let galley = (room >= 4.0 * size::ICON).then(|| {
        let mut job = egui::text::LayoutJob::simple_singleline(
            format!(
                "{}: {}",
                source.name,
                if ended.empty { "no picture" } else { "stopped" }
            ),
            TextRole::Meta.font(),
            color::TEXT_SECONDARY,
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
        painter.layout_job(job)
    });
    let width = match &galley {
        Some(galley) => pad + size::ICON + pad + galley.size().x + pad,
        None => height,
    };
    let chip = egui::Rect::from_center_size(rect.center(), egui::vec2(width, height));
    painter.rect(
        chip,
        size::RADIUS_CONTROL,
        color::BG_RAISED,
        egui::Stroke::new(size::STROKE, color::LINE_STRONG),
        egui::StrokeKind::Inside,
    );
    let icon_rect = match &galley {
        Some(_) => egui::Rect::from_min_size(
            egui::pos2(chip.left() + pad, chip.center().y - size::ICON / 2.0),
            egui::Vec2::splat(size::ICON),
        ),
        None => egui::Rect::from_center_size(chip.center(), egui::Vec2::splat(size::ICON)),
    };
    icon.paint(painter, icon_rect, color::STATE_WARN);
    if let Some(galley) = galley {
        let at = egui::pos2(
            icon_rect.right() + pad,
            chip.center().y - galley.size().y / 2.0,
        );
        painter.galley(at, galley, color::TEXT_SECONDARY);
    }
}

/// `monitor` moved onto whole pixels, and its size in pixels (`None` when
/// it has none).
fn on_pixels(monitor: egui::Rect, pixels_per_point: f32) -> (egui::Rect, Option<(u32, u32)>) {
    let monitor = monitor.round_to_pixels(pixels_per_point);
    let pixels = monitor.size() * pixels_per_point;
    let size = (pixels.x >= 1.0 && pixels.y >= 1.0)
        .then(|| (pixels.x.round() as u32, pixels.y.round() as u32));
    (monitor, size)
}

/// Where the preview picture goes: at its own size in pixels when that is
/// the monitor's, give or take the pixel by which the engine's fit to the
/// canvas's aspect ratio can differ, so it is drawn 1:1; stretched over
/// the monitor otherwise (while a new size is on its way).
fn picture_rect(monitor: egui::Rect, texture: [usize; 2], pixels_per_point: f32) -> egui::Rect {
    let picture = egui::vec2(texture[0] as f32, texture[1] as f32);
    let off = (picture - monitor.size() * pixels_per_point).abs();
    if off.x <= 1.5 && off.y <= 1.5 {
        egui::Rect::from_min_size(monitor.min, picture / pixels_per_point)
    } else {
        monitor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picture_is_drawn_at_its_own_size_once_it_has_the_monitors() {
        // 1000x562 pixels at 125 %; the engine fits that to 999x562.
        let monitor = egui::Rect::from_min_size(egui::pos2(12.0, 20.0), egui::vec2(800.0, 449.6));
        let picture = picture_rect(monitor, [999, 562], 1.25);
        assert_eq!(picture.min, monitor.min);
        assert!((picture.width() * 1.25 - 999.0).abs() < 1e-3, "{picture:?}");
        assert!(
            (picture.height() * 1.25 - 562.0).abs() < 1e-3,
            "{picture:?}"
        );
        // A frame of the size before (or the default) is stretched.
        assert_eq!(picture_rect(monitor, [480, 270], 1.25), monitor);
        assert_eq!(picture_rect(monitor, [996, 560], 1.25), monitor);
    }

    #[test]
    fn the_monitor_lands_on_whole_pixels_and_asks_for_its_size() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.3, 20.0), egui::vec2(800.0, 450.0));
        let (monitor, pixels) = on_pixels(rect, 1.0);
        assert_eq!(monitor.min, egui::pos2(10.0, 20.0));
        assert_eq!(pixels, Some((800, 450)));
        // At 150 % a point is 1.5 pixels: the edges move to whole pixels
        // (10.3 pt = 15.45 px -> 15 px = 10 pt).
        let (monitor, pixels) = on_pixels(rect, 1.5);
        assert_eq!(monitor.min, egui::pos2(10.0, 20.0));
        assert_eq!(pixels, Some((1200, 675)));
        let (_, pixels) = on_pixels(rect, 1.25);
        assert_eq!(pixels, Some((1000, 563)));
        // Nothing to show, nothing to ask for.
        let empty = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::Vec2::ZERO);
        assert_eq!(on_pixels(empty, 2.0).1, None);
    }

    #[test]
    fn captures_that_ended_are_marked_where_their_source_is() {
        let source = |id: &str, kind: SourceKind, visible: bool| SourceSpec {
            id: id.to_owned(),
            name: id.to_owned(),
            kind,
            rect: command_api::SourceRect {
                x: 960.0,
                y: 540.0,
                width: 1920.0,
                height: 1080.0,
            },
            opacity: 1.0,
            visible,
        };
        let camera = || SourceKind::Camera {
            camera_id: "cam:1".to_owned(),
            mode: None,
        };
        let sources = [
            source("bars", SourceKind::ColorBars, true),
            source("cam", camera(), true),
            source("hidden", camera(), false),
            source("running", camera(), true),
            source(
                "screen",
                SourceKind::Screen {
                    target_kinds: Vec::new(),
                    cursor_mode: capture_api::CursorMode::Embedded,
                    capture_target_id: None,
                },
                true,
            ),
            source("idle", camera(), true),
        ];
        let health = |id: &str| match id {
            "running" => SourceHealth::Capturing {
                last_frame_age_ms: None,
                frames_consumed: 0,
            },
            "idle" => SourceHealth::NotCapturing,
            "screen" => SourceHealth::Ended {
                reason: "the captured window was closed".to_owned(),
            },
            _ => SourceHealth::Ended {
                reason: format!("could not start: {id} is busy"),
            },
        };
        // Visible screens and cameras whose capture ended, in drawing
        // order; not a hidden one, one that runs (also without a frame
        // yet) or one that does not capture. One that did not start draws
        // nothing; one that ended while it ran keeps its last frame.
        let marked: Vec<_> = ended_captures(&sources, health)
            .into_iter()
            .map(|ended| (ended.source.id.as_str(), ended.reason, ended.empty))
            .collect();
        assert_eq!(
            marked,
            [
                ("cam", "could not start: cam is busy".to_owned(), true),
                ("screen", "the captured window was closed".to_owned(), false),
            ]
        );
        // At the source's place on the monitor, cut to it: the lower
        // right quarter of a 960-point monitor of a 1920-pixel canvas.
        let monitor = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(960.0, 540.0));
        assert_eq!(
            on_monitor(monitor, 0.5, &sources[1]),
            egui::Rect::from_min_max(egui::pos2(490.0, 290.0), egui::pos2(970.0, 560.0))
        );
    }
}
