//! The docks of the production workspace. Each module adds the methods
//! that draw one dock to [`DesktopApp`](crate::app::DesktopApp); they use
//! the design system's components and tokens only.

mod dialogs;
mod inspector;
mod mixer;
mod outputs;
mod program;
mod routing;
mod scenes;
mod sources;
mod top_bar;

use command_api::{
    BusState, MAX_COORDINATE, PROGRAM_BUS_ID, SceneList, SceneSpec, SourceKind, SourceSpec,
};
use eframe::egui;

use crate::ui::icons::Icon;
use crate::ui::theme::size;
use crate::ui::widgets;
use crate::workspace::{self, Shortcut};

/// The value edit to send, if any: when a drag ends, or right away for
/// keyboard and typed input. Mid-drag values are not sent, so a drag is
/// one command, not one per frame.
///
/// Views rebuild `value` from the model every frame, and some widgets
/// (`DragValue`) apply a drag only while the pointer moves, so on the
/// release frame `value` is the model's again. The value seen during the
/// drag is therefore kept and sent when the drag ends.
fn commit(ui: &egui::Ui, response: &egui::Response, value: f32) -> Option<f32> {
    let dragged = response.id.with("dragged value");
    if response.dragged() {
        ui.data_mut(|data| data.insert_temp(dragged, value));
        None
    } else if response.drag_stopped() {
        let seen = ui.data_mut(|data| data.remove_temp::<f32>(dragged));
        if response.changed() {
            Some(value)
        } else {
            seen
        }
    } else {
        response.changed().then_some(value)
    }
}

/// Name a widget that has no label of its own (a number field in a
/// table) for assistive technology and the tooltip.
fn labelled(ui: &egui::Ui, response: egui::Response, label: &str) -> egui::Response {
    ui.ctx()
        .accesskit_node_builder(response.id, |builder| builder.set_label(label));
    response.on_hover_text(label)
}

/// A source's opacity in percent: a slider and, beside it, a number field
/// for the exact value. Returns both responses; a live edit commits them
/// with [`commit`], a form takes them as they change.
///
/// While one of the two is dragged, both show the dragged value: a live
/// edit rebuilds `percent` from the model each frame and sends it only
/// when the drag ends, and the slider is drawn before the field.
fn opacity_row(ui: &mut egui::Ui, percent: &mut f32) -> [egui::Response; 2] {
    const FIELD: f32 = 64.0;
    let draft = ui.id().with("opacity draft");
    if let Some(dragged) = ui.data(|data| data.get_temp::<f32>(draft)) {
        *percent = dragged;
    }
    let responses = ui
        .horizontal(|ui| {
            let width = (ui.available_width() - FIELD - ui.spacing().item_spacing.x).max(80.0);
            let slider = widgets::slider(ui, percent, 0.0..=100.0, 1.0, width, "Opacity");
            let field = ui.add_sized(
                [FIELD, size::CONTROL],
                egui::DragValue::new(percent)
                    .range(0.0..=100.0)
                    .suffix(" %")
                    .fixed_decimals(0),
            );
            [slider, labelled(ui, field, "Opacity (%)")]
        })
        .inner;
    if responses.iter().any(egui::Response::dragged) {
        ui.data_mut(|data| data.insert_temp(draft, *percent));
    } else {
        ui.data_mut(|data| data.remove_temp::<f32>(draft));
    }
    responses
}

/// A shortcut's keys as a menu or tooltip shows them (Cmd on macOS);
/// `None` for a scene past the ninth.
fn key_hint(ctx: &egui::Context, shortcut: Shortcut) -> Option<String> {
    shortcut.keys().map(|keys| ctx.format_shortcut(&keys))
}

/// Up and Down in a list whose row with focus is among `rows`: the row
/// to select, which takes the focus. egui would move the focus to
/// whatever lies above or below; the list keeps it, also at its ends.
fn list_keys(ui: &egui::Ui, rows: &[egui::Response]) -> Option<usize> {
    let focused = rows.iter().position(egui::Response::has_focus)?;
    let (pressed, steps) = ui.input(|input| {
        let down = input.num_presses(egui::Key::ArrowDown);
        let up = input.num_presses(egui::Key::ArrowUp);
        (
            down + up > 0 && !input.modifiers.any(),
            down as isize - up as isize,
        )
    });
    if !pressed {
        return None;
    }
    ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
    let target = workspace::list_step(focused, rows.len(), steps);
    if target == focused {
        return None;
    }
    rows[target].request_focus();
    rows[target].scroll_to_me(None);
    Some(target)
}

/// A bus's name where room is short: `PGM` for the program bus.
fn bus_short_name(bus: &BusState) -> &str {
    if bus.id == PROGRAM_BUS_ID {
        "PGM"
    } else {
        &bus.name
    }
}

/// What the engine accepts for a source position, or a size (`size`:
/// whole pixels from 1 here, the engine takes anything above 0).
fn coordinate_range(size: bool) -> std::ops::RangeInclusive<f32> {
    if size {
        1.0..=MAX_COORDINATE
    } else {
        -MAX_COORDINATE..=MAX_COORDINATE
    }
}

/// A source kind's icon and name.
fn kind_label(kind: &SourceKind) -> (Icon, &'static str) {
    match kind {
        SourceKind::ColorBars => (Icon::Bars, "colour bars"),
        SourceKind::Solid { .. } => (Icon::Solid, "solid colour"),
        SourceKind::Screen { .. } => (Icon::Screen, "screen capture"),
    }
}

pub(crate) fn scene<'a>(scenes: &'a SceneList, id: &str) -> Option<&'a SceneSpec> {
    scenes.scenes.iter().find(|scene| scene.id == id)
}

pub(crate) fn source<'a>(scene: &'a SceneSpec, id: &str) -> Option<&'a SourceSpec> {
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
