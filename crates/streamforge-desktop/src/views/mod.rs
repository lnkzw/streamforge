//! The docks of the production workspace. Each module adds the methods
//! that draw one dock to [`DesktopApp`](crate::app::DesktopApp); they use
//! the design system's components and tokens only.

mod dialogs;
mod inspector;
mod mixer;
mod outputs;
mod program;
mod scenes;
mod sources;
mod top_bar;

use command_api::{SceneList, SceneSpec, SourceKind, SourceSpec};
use eframe::egui;

use crate::ui::icons::Icon;

/// A value edit is sent when a drag ends, or right away for keyboard and
/// typed input (which change the value without dragging). Mid-drag values
/// are not sent, so a drag is one command, not one per frame.
fn committed(response: &egui::Response) -> bool {
    response.drag_stopped() || (response.changed() && !response.dragged())
}

/// A source kind's icon and name.
fn kind_label(kind: &SourceKind) -> (Icon, &'static str) {
    match kind {
        SourceKind::ColorBars => (Icon::Bars, "colour bars"),
        SourceKind::Solid { .. } => (Icon::Solid, "solid colour"),
        SourceKind::Screen { .. } => (Icon::Screen, "screen capture"),
    }
}

fn scene<'a>(scenes: &'a SceneList, id: &str) -> Option<&'a SceneSpec> {
    scenes.scenes.iter().find(|scene| scene.id == id)
}

fn source<'a>(scene: &'a SceneSpec, id: &str) -> Option<&'a SourceSpec> {
    scene.sources.iter().find(|source| source.id == id)
}

/// Lay out `add` right to left inside `rect` (e.g. the actions of a
/// section header).
fn right_to_left<R>(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
        add,
    )
    .inner
}
