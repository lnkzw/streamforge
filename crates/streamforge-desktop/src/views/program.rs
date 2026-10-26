//! The program dock: the monitor, as large as the space allows at the
//! canvas's aspect ratio, with a tally frame while an output runs, and one
//! line of technical facts under it.

use eframe::egui;

use crate::app::DesktopApp;
use crate::ui::icons::Icon;
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets::{self, BadgeKind, Transport};
use crate::workspace::{Shortcut, fit};

use super::{right_to_left, scene, with_keys};

/// The line of facts under the monitor.
const FACTS_HEIGHT: f32 = 22.0;

impl DesktopApp {
    /// Returns where the header ends: a drawer opens below it, so the
    /// on-air marks and the preview switch stay in view.
    pub(crate) fn program_dock(&mut self, ui: &mut egui::Ui) -> f32 {
        let recording = self.on_air(Transport::Record);
        let live = self.on_air(Transport::Live);
        let program_name = self
            .model
            .scenes
            .as_ref()
            .map(|scenes| scenes.active.clone())
            .unwrap_or_default();

        let actions = widgets::section_header(ui, "Program", None);
        right_to_left(ui, actions, |ui| {
            ui.spacing_mut().item_spacing.x = size::SPACE_6;
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
        // A click selects the source under the pointer. Not focusable: the
        // monitor has nothing to do by keyboard.
        let response = ui.allocate_rect(space, egui::Sense::CLICK);
        self.paint_monitor(ui, monitor, recording || live);
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

    fn paint_monitor(&self, ui: &egui::Ui, monitor: egui::Rect, on_air: bool) {
        let painter = ui.painter();
        painter.rect_filled(monitor, 0.0, color::BG_INSET);
        match (&self.preview, self.model.preview_enabled) {
            (Some(texture), true) => {
                painter.image(
                    texture.id(),
                    monitor,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
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
        if *scene_id != scenes.active_id {
            return;
        }
        let Some(source) =
            scene(scenes, scene_id).and_then(|scene| super::source(scene, source_id))
        else {
            return;
        };
        let scale = monitor.width() / scenes.canvas_width.max(1) as f32;
        let rect = egui::Rect::from_min_size(
            monitor.min + egui::vec2(source.rect.x, source.rect.y) * scale,
            egui::vec2(source.rect.width, source.rect.height) * scale,
        )
        .intersect(monitor);
        if rect.is_positive() {
            ui.painter().rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(size::STROKE, color::ACCENT),
                egui::StrokeKind::Inside,
            );
        }
    }

    /// A click on the picture selects the topmost source under it in the
    /// program scene.
    fn select_at(&mut self, monitor: egui::Rect, pointer: egui::Pos2) {
        let Some(scenes) = &self.model.scenes else {
            return;
        };
        let scale = scenes.canvas_width.max(1) as f32 / monitor.width();
        let at = (pointer - monitor.min) * scale;
        let Some(program) = scene(scenes, &scenes.active_id) else {
            return;
        };
        let hit = program.sources.iter().rev().find(|source| {
            let rect = &source.rect;
            at.x >= rect.x
                && at.y >= rect.y
                && at.x < rect.x + rect.width
                && at.y < rect.y + rect.height
        });
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
