//! Components of the design system. Views compose these; they do not paint
//! controls of their own. Every interactive component reports a role and
//! label to AccessKit and draws the same focus ring.

use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2,
    Widget, WidgetInfo, WidgetType, pos2, text::LayoutJob, vec2,
};

use super::icons::{Icon, icon_rect};
use super::theme::{TextRole, color, motion, size};

/// How a control is drawn, normally from its [`Response`]; the specimen
/// render forces a state to show all of them side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VisualState {
    #[default]
    Normal,
    Hovered,
    Pressed,
    Focused,
    Disabled,
}

impl VisualState {
    fn of(response: &Response, enabled: bool) -> Self {
        if !enabled {
            Self::Disabled
        } else if response.is_pointer_button_down_on() {
            Self::Pressed
        } else if response.hovered() {
            Self::Hovered
        } else if response.has_focus() {
            Self::Focused
        } else {
            Self::Normal
        }
    }
}

/// Whether to draw the focus ring: keyboard focus shows even while the
/// pointer is over the control.
fn shows_focus(forced: Option<VisualState>, response: &Response) -> bool {
    match forced {
        Some(state) => state == VisualState::Focused,
        None => response.has_focus(),
    }
}

/// How often animations repaint: the app's own live cadence, not the
/// display rate.
const ANIMATION_FRAME: Duration = Duration::from_millis(33);

/// The position in a repeating animation of `period` seconds, from 0 to 1.
/// Taken in `f64` before narrowing, so it stays smooth after days of
/// uptime.
fn phase(ui: &Ui, period: f64) -> f32 {
    let time = ui.input(|input| input.time);
    ((time % period) / period) as f32
}

/// Add a control in a disabled scope: its response then reports disabled,
/// so `on_disabled_hover_text` shows, and egui gives it no interaction.
/// egui's fade of disabled content is undone, since components draw their
/// own disabled colours.
fn disabled(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> Response) -> Response {
    ui.scope(|ui| {
        let opacity = ui.opacity();
        ui.disable();
        ui.set_opacity(opacity);
        add(ui)
    })
    .inner
}

/// The 2 px accent ring around a focused control, 1 px outside it.
pub fn focus_ring(painter: &egui::Painter, rect: Rect, radius: u8) {
    painter.rect_stroke(
        rect.expand(1.0 + size::STROKE_HEAVY / 2.0),
        CornerRadius::same(radius + 2),
        Stroke::new(size::STROKE_HEAVY, color::ACCENT),
        StrokeKind::Middle,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// The one main action of a view or dialog.
    Primary,
    Secondary,
    /// Text only, for low-weight actions.
    Ghost,
    /// Destructive; the caller asks for confirmation.
    Danger,
}

pub struct Button<'a> {
    kind: ButtonKind,
    icon: Option<Icon>,
    label: &'a str,
    enabled: bool,
    busy: bool,
    min_width: f32,
    forced: Option<VisualState>,
}

impl<'a> Button<'a> {
    pub fn new(kind: ButtonKind, label: &'a str) -> Self {
        Self {
            kind,
            icon: None,
            label,
            enabled: true,
            busy: false,
            min_width: 0.0,
            forced: None,
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Waiting for the engine: a spinner replaces the icon, clicks are off.
    pub fn busy(mut self, busy: bool) -> Self {
        self.busy = busy;
        self
    }

    pub fn min_width(mut self, width: f32) -> Self {
        self.min_width = width;
        self
    }

    /// Draw in `state` regardless of the pointer (specimen only).
    pub fn forced(mut self, state: VisualState) -> Self {
        self.forced = Some(state);
        self
    }
}

impl Widget for Button<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        // Disabled through the `Ui`, so the response says so (and a
        // disabled tooltip shows).
        if !self.enabled && ui.is_enabled() {
            return disabled(ui, |ui| self.ui(ui));
        }
        let font = TextRole::Body.font();
        let galley = ui
            .painter()
            .layout_no_wrap(self.label.to_owned(), font, Color32::PLACEHOLDER);
        let has_icon = self.icon.is_some() || self.busy;
        let icon_space = if has_icon {
            size::ICON + size::SPACE_6
        } else {
            0.0
        };
        let width = (galley.size().x + icon_space + 2.0 * size::SPACE_12).max(self.min_width);
        let sense = if self.enabled && !self.busy {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(vec2(width, size::CONTROL), sense);
        response.widget_info(|| {
            WidgetInfo::labeled(WidgetType::Button, self.enabled && !self.busy, self.label)
        });
        if !ui.is_rect_visible(rect) {
            return response;
        }
        let state = self
            .forced
            .unwrap_or_else(|| VisualState::of(&response, self.enabled));
        let (fill, stroke, text) = button_colors(self.kind, state);
        let painter = ui.painter();
        let radius = CornerRadius::same(size::RADIUS_CONTROL);
        painter.rect(rect, radius, fill, stroke, StrokeKind::Inside);
        let content = icon_space + galley.size().x;
        let mut x = rect.center().x - content / 2.0;
        if self.busy {
            spinner(
                ui,
                icon_rect(pos2(x + size::ICON / 2.0, rect.center().y), 12.0),
                text,
            );
            x += icon_space;
        } else if let Some(icon) = self.icon {
            icon.paint(
                painter,
                icon_rect(pos2(x + size::ICON / 2.0, rect.center().y), size::ICON),
                text,
            );
            x += icon_space;
        }
        painter.galley(
            pos2(x, rect.center().y - galley.size().y / 2.0),
            galley,
            text,
        );
        if shows_focus(self.forced, &response) {
            focus_ring(painter, rect, size::RADIUS_CONTROL);
        }
        response
    }
}

/// A square ghost button showing only `icon`, at the `Ui`'s control height
/// (smaller in a compact table). `label` says what it does: it is the
/// tooltip and the name for assistive technology.
pub fn icon_button(ui: &mut Ui, icon: Icon, label: &str) -> Response {
    let enabled = ui.is_enabled();
    let side = ui.spacing().interact_size.y;
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        let state = VisualState::of(&response, enabled);
        let (fill, _, tint) = button_colors(ButtonKind::Ghost, state);
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(size::RADIUS_CONTROL), fill);
        icon.paint(painter, icon_rect(rect.center(), size::ICON), tint);
        if response.has_focus() {
            focus_ring(painter, rect, size::RADIUS_CONTROL);
        }
    }
    response.on_hover_text(label)
}

/// A ghost button that shows a state: the level's icon in its colour and
/// a line of text (cut to fit `max_width`). Used for the health summary,
/// which opens its details.
pub fn status_button(ui: &mut Ui, level: StatusLevel, text: &str, max_width: f32) -> Response {
    let (icon, tint) = status_icon(level);
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat::simple(TextRole::Body.font(), color::TEXT_PRIMARY),
    );
    let text_width = (max_width - size::ICON - size::SPACE_6 - 2.0 * size::SPACE_8).max(0.0);
    job.wrap = egui::text::TextWrapping::truncate_at_width(text_width);
    let galley = ui.painter().layout_job(job);
    let width = galley.size().x + size::ICON + size::SPACE_6 + 2.0 * size::SPACE_8;
    let (rect, response) = ui.allocate_exact_size(vec2(width, size::CONTROL), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, text));
    if ui.is_rect_visible(rect) {
        let state = VisualState::of(&response, true);
        let (fill, _, _) = button_colors(ButtonKind::Ghost, state);
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(size::RADIUS_CONTROL), fill);
        let icon_center = pos2(
            rect.left() + size::SPACE_8 + size::ICON / 2.0,
            rect.center().y,
        );
        icon.paint(painter, icon_rect(icon_center, 14.0), tint);
        let text_color = if level == StatusLevel::Info {
            color::TEXT_SECONDARY
        } else {
            color::TEXT_PRIMARY
        };
        painter.galley(
            pos2(
                rect.left() + size::SPACE_8 + size::ICON + size::SPACE_6,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            text_color,
        );
        if response.has_focus() {
            focus_ring(painter, rect, size::RADIUS_CONTROL);
        }
    }
    response
}

/// A ghost button that opens a menu: an icon, a line of text (cut to fit
/// `max_width`), a dot when `marked` (the project's unsaved changes) and a
/// chevron. `label` names it for assistive technology and the tooltip.
pub fn menu_button(
    ui: &mut Ui,
    icon: Icon,
    text: &str,
    marked: bool,
    label: &str,
    max_width: f32,
) -> Response {
    const DOT: f32 = 6.0;
    // The project name is a title (design system, typography).
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat::simple(TextRole::Title.font(), Color32::PLACEHOLDER),
    );
    let mark_space = if marked { DOT + size::SPACE_6 } else { 0.0 };
    let fixed = 2.0 * (size::ICON + size::SPACE_6) + mark_space + 2.0 * size::SPACE_8;
    job.wrap = egui::text::TextWrapping::truncate_at_width((max_width - fixed).max(0.0));
    let galley = ui.painter().layout_job(job);
    let width = galley.size().x + fixed;
    let enabled = ui.is_enabled();
    let (rect, response) = ui.allocate_exact_size(vec2(width, size::CONTROL), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        let state = VisualState::of(&response, enabled);
        let (fill, _, _) = button_colors(ButtonKind::Ghost, state);
        let tint = if enabled {
            color::TEXT_PRIMARY
        } else {
            color::TEXT_DISABLED
        };
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(size::RADIUS_CONTROL), fill);
        let mut x = rect.left() + size::SPACE_8;
        icon.paint(
            painter,
            icon_rect(pos2(x + size::ICON / 2.0, rect.center().y), 14.0),
            color::TEXT_TERTIARY,
        );
        x += size::ICON + size::SPACE_6;
        let text_width = galley.size().x;
        painter.galley(
            pos2(x, rect.center().y - galley.size().y / 2.0),
            galley,
            tint,
        );
        x += text_width + size::SPACE_6;
        if marked {
            painter.circle_filled(
                pos2(x + DOT / 2.0, rect.center().y),
                DOT / 2.0,
                color::TEXT_SECONDARY,
            );
            x += DOT + size::SPACE_6;
        }
        Icon::ChevronDown.paint(
            painter,
            icon_rect(pos2(x + size::ICON / 2.0, rect.center().y), 12.0),
            color::TEXT_TERTIARY,
        );
        if response.has_focus() {
            focus_ring(painter, rect, size::RADIUS_CONTROL);
        }
    }
    response.on_hover_text(label)
}

/// A field that shows a choice and opens it elsewhere (a dialog): an icon
/// in `tint`, the current value cut to fit `width`, and a chevron, at the
/// `Ui`'s control height. `label` names it for assistive technology, which
/// reads `text` as its value; the caller adds the tooltip. Disabled with
/// the `Ui`.
pub fn select_field(
    ui: &mut Ui,
    icon: Icon,
    tint: Color32,
    text: &str,
    label: &str,
    width: f32,
) -> Response {
    field(
        ui,
        Some((icon, tint)),
        text,
        label,
        width,
        WidgetType::Button,
    )
}

/// A field that shows the current choice of a list and opens the list
/// under it, as a menu of [`choice`]s (and [`menu_heading`]s for groups).
/// The field looks like the [`select_field`] without an icon. `label`
/// names it for assistive technology, which reads `text` as its value;
/// the caller adds the tooltip. Disabled with the `Ui`.
pub fn dropdown(
    ui: &mut Ui,
    text: &str,
    label: &str,
    width: f32,
    menu: impl FnOnce(&mut Ui),
) -> Response {
    let response = field(ui, None, text, label, width, WidgetType::ComboBox);
    egui::Popup::menu(&response).width(width).show(|ui| {
        // As wide as the field (the frame's margin is outside the content).
        let margin = ui.style().spacing.menu_margin.sum().x;
        ui.set_min_width(width - margin);
        ui.spacing_mut().item_spacing.y = 0.0;
        menu(ui);
    });
    response
}

/// An option of a [`dropdown`], checked while it is `current`; a click
/// makes it current (the response is then changed) and closes the menu.
pub fn choice<T: PartialEq>(ui: &mut Ui, current: &mut T, value: T, text: &str) -> Response {
    let mut response = ui.add(MenuItem::new(text).selected(*current == value));
    if response.clicked() && *current != value {
        *current = value;
        response.mark_changed();
    }
    response
}

/// The field of a [`select_field`] or [`dropdown`]: an optional icon in
/// its tint, the value cut to fit, a chevron.
fn field(
    ui: &mut Ui,
    icon: Option<(Icon, Color32)>,
    text: &str,
    label: &str,
    width: f32,
    kind: WidgetType,
) -> Response {
    const ICON: f32 = 14.0;
    const CHEVRON: f32 = 12.0;
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat::simple(TextRole::Body.font(), Color32::PLACEHOLDER),
    );
    let icon_space = if icon.is_some() {
        ICON + size::SPACE_4
    } else {
        0.0
    };
    let fixed = icon_space + CHEVRON + size::SPACE_4 + 2.0 * size::SPACE_6;
    job.wrap = egui::text::TextWrapping::truncate_at_width((width - fixed).max(0.0));
    let galley = ui.painter().layout_job(job);
    let enabled = ui.is_enabled();
    let height = ui.spacing().interact_size.y;
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    response.widget_info(|| {
        let mut info = WidgetInfo::labeled(kind, enabled, label);
        info.current_text_value = Some(text.to_owned());
        info
    });
    if ui.is_rect_visible(rect) {
        let state = VisualState::of(&response, enabled);
        let (fill, stroke, text_color) = button_colors(ButtonKind::Secondary, state);
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(size::RADIUS_CONTROL),
            fill,
            stroke,
            StrokeKind::Inside,
        );
        let mut x = rect.left() + size::SPACE_6;
        if let Some((icon, tint)) = icon {
            let tint = if enabled { tint } else { color::TEXT_DISABLED };
            icon.paint(
                painter,
                icon_rect(pos2(x + ICON / 2.0, rect.center().y), ICON),
                tint,
            );
            x += icon_space;
        }
        painter.galley(
            pos2(x, rect.center().y - galley.size().y / 2.0),
            galley,
            text_color,
        );
        Icon::ChevronDown.paint(
            painter,
            icon_rect(
                pos2(
                    rect.right() - size::SPACE_6 - CHEVRON / 2.0,
                    rect.center().y,
                ),
                CHEVRON,
            ),
            color::TEXT_TERTIARY,
        );
        if response.has_focus() {
            focus_ring(painter, rect, size::RADIUS_CONTROL);
        }
    }
    response
}

/// A row of a menu (a popup, a context menu, a [`dropdown`]'s list): a
/// 16 px column for an icon, or for the check mark of the current choice,
/// then the text, and the keyboard shortcut that does the same,
/// right-aligned. The column is there in every row, so the texts of a
/// menu line up. A danger item is destructive and drawn in the error
/// colour; the caller asks before acting.
pub struct MenuItem<'a> {
    label: &'a str,
    icon: Option<Icon>,
    shortcut: Option<&'a str>,
    /// Set in a list of choices: whether this one is current.
    selected: Option<bool>,
    enabled: bool,
    danger: bool,
    forced: Option<VisualState>,
}

impl<'a> MenuItem<'a> {
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            icon: None,
            shortcut: None,
            selected: None,
            enabled: true,
            danger: false,
            forced: None,
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// The shortcut as the platform writes it (e.g. "Ctrl+S", "⌘S").
    pub fn shortcut(mut self, shortcut: &'a str) -> Self {
        self.shortcut = Some(shortcut);
        self
    }

    /// An option of a list of choices; the current one has a check mark
    /// in the icon column.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = Some(selected);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    /// Draw in `state` regardless of the pointer (specimen only).
    pub fn forced(mut self, state: VisualState) -> Self {
        self.forced = Some(state);
        self
    }
}

impl Widget for MenuItem<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        // Disabled through the `Ui`, so the response says so (and a
        // disabled tooltip shows).
        if !self.enabled && ui.is_enabled() {
            return disabled(ui, |ui| self.ui(ui));
        }
        let lead = size::ICON + size::SPACE_6;
        let shortcut = self.shortcut.map(|text| {
            ui.painter().layout_no_wrap(
                text.to_owned(),
                TextRole::Meta.font(),
                Color32::PLACEHOLDER,
            )
        });
        let trail = shortcut
            .as_ref()
            .map_or(0.0, |galley| galley.size().x + size::SPACE_24);
        let fixed = 2.0 * size::SPACE_6 + lead + trail;
        let mut job = LayoutJob::single_section(
            self.label.to_owned(),
            egui::TextFormat::simple(TextRole::Body.font(), Color32::PLACEHOLDER),
        );
        job.wrap =
            egui::text::TextWrapping::truncate_at_width((ui.available_width() - fixed).max(0.0));
        let galley = ui.painter().layout_job(job);
        let sense = if self.enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        // At least the content; a menu's justified layout widens every row
        // to its widest.
        let (rect, response) =
            ui.allocate_at_least(vec2(galley.size().x + fixed, size::ROW), sense);
        // egui closes a menu on a pointer click only; Enter, Space and
        // assistive technology choose too.
        if response.clicked() {
            ui.close_kind(egui::UiKind::Menu);
        }
        response.widget_info(|| match self.selected {
            Some(selected) => WidgetInfo::selected(
                WidgetType::SelectableLabel,
                self.enabled,
                selected,
                self.label,
            ),
            None => WidgetInfo::labeled(WidgetType::Button, self.enabled, self.label),
        });
        if !ui.is_rect_visible(rect) {
            return response;
        }
        let state = self
            .forced
            .unwrap_or_else(|| VisualState::of(&response, self.enabled));
        let painter = ui.painter();
        if matches!(state, VisualState::Hovered | VisualState::Pressed) {
            let (fill, _, _) = button_colors(ButtonKind::Ghost, state);
            painter.rect_filled(rect, CornerRadius::same(size::RADIUS_CONTROL), fill);
        }
        let (text, icon_tint) = match (state, self.danger) {
            (VisualState::Disabled, _) => (color::TEXT_DISABLED, color::TEXT_DISABLED),
            (_, true) => (color::STATE_ERROR, color::STATE_ERROR),
            _ => (color::TEXT_PRIMARY, color::TEXT_SECONDARY),
        };
        let icon_center = pos2(
            rect.left() + size::SPACE_6 + size::ICON / 2.0,
            rect.center().y,
        );
        if self.selected == Some(true) {
            let tint = if self.enabled {
                color::ACCENT
            } else {
                color::TEXT_DISABLED
            };
            Icon::Check.paint(painter, icon_rect(icon_center, 14.0), tint);
        } else if let Some(icon) = self.icon {
            icon.paint(painter, icon_rect(icon_center, 14.0), icon_tint);
        }
        painter.galley(
            pos2(
                rect.left() + size::SPACE_6 + lead,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            text,
        );
        if let Some(galley) = shortcut {
            let tint = if state == VisualState::Disabled {
                color::TEXT_DISABLED
            } else {
                color::TEXT_TERTIARY
            };
            painter.galley(
                pos2(
                    rect.right() - size::SPACE_6 - galley.size().x,
                    rect.center().y - galley.size().y / 2.0,
                ),
                galley,
                tint,
            );
        }
        if shows_focus(self.forced, &response) {
            focus_ring(painter, rect, size::RADIUS_CONTROL);
        }
        response
    }
}

/// The heading of a group in a menu (e.g. desktop audio apart from the
/// other inputs): a section label in line with the rows' padding.
pub fn menu_heading(ui: &mut Ui, text: &str) {
    ui.add_space(size::SPACE_4);
    ui.horizontal(|ui| {
        ui.add_space(size::SPACE_6);
        ui.label(TextRole::Section.text(text));
    });
    ui.add_space(size::SPACE_2);
}

/// Waiting for something to arrive: a small spinner and one line saying
/// what.
pub fn loading(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = size::SPACE_6;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(size::ICON), Sense::hover());
        spinner(ui, icon_rect(rect.center(), 12.0), color::TEXT_TERTIARY);
        ui.label(TextRole::Meta.text(text));
    });
}

/// Where a dock has nothing to show: an icon and one line saying what is
/// missing, centred. The caller adds the action that fixes it below.
pub fn empty_state(ui: &mut Ui, icon: Icon, text: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(size::SPACE_12);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
        icon.paint(ui.painter(), rect, color::TEXT_TERTIARY);
        ui.add_space(size::SPACE_4);
        ui.label(TextRole::Meta.text(text));
    });
}

fn button_colors(kind: ButtonKind, state: VisualState) -> (Color32, Stroke, Color32) {
    use VisualState::*;
    let none = Stroke::NONE;
    let outline = |c: Color32| Stroke::new(size::STROKE, c);
    if state == Disabled {
        let fill = match kind {
            ButtonKind::Primary | ButtonKind::Secondary => color::BG_RAISED,
            _ => Color32::TRANSPARENT,
        };
        return (fill, none, color::TEXT_DISABLED);
    }
    match kind {
        ButtonKind::Primary => {
            let fill = match state {
                Hovered => Color32::from_rgb(0x3B, 0xC6, 0xD3),
                Pressed => Color32::from_rgb(0x25, 0x9C, 0xA8),
                _ => color::ACCENT,
            };
            (fill, none, color::BG_APP)
        }
        ButtonKind::Secondary => match state {
            Hovered => (
                Color32::from_rgb(0x24, 0x26, 0x2A),
                outline(Color32::from_rgb(0x44, 0x48, 0x4E)),
                color::TEXT_PRIMARY,
            ),
            Pressed => (
                color::BG_PRESSED,
                outline(color::LINE_STRONG),
                color::TEXT_PRIMARY,
            ),
            _ => (
                color::BG_RAISED,
                outline(color::LINE_STRONG),
                color::TEXT_PRIMARY,
            ),
        },
        ButtonKind::Ghost => match state {
            Hovered => (color::BG_RAISED, none, color::TEXT_PRIMARY),
            Pressed => (color::BG_PRESSED, none, color::TEXT_PRIMARY),
            _ => (Color32::TRANSPARENT, none, color::TEXT_SECONDARY),
        },
        ButtonKind::Danger => match state {
            Hovered => (
                color::STATE_ERROR.gamma_multiply(0.16),
                outline(color::STATE_ERROR),
                color::STATE_ERROR,
            ),
            Pressed => (
                color::STATE_ERROR.gamma_multiply(0.24),
                outline(color::STATE_ERROR),
                color::STATE_ERROR,
            ),
            _ => (
                Color32::TRANSPARENT,
                outline(color::STATE_ERROR.gamma_multiply(0.6)),
                color::STATE_ERROR,
            ),
        },
    }
}

/// A small rotating arc; keeps repainting while shown.
pub fn spinner(ui: &Ui, rect: Rect, color: Color32) {
    // One turn per second.
    let start = phase(ui, 1.0) * std::f32::consts::TAU;
    let radius = rect.width() / 2.0 - 1.0;
    let points: Vec<_> = (0..=20)
        .map(|i| {
            let t = start + i as f32 / 20.0 * 4.4;
            rect.center() + vec2(t.cos(), t.sin()) * radius
        })
        .collect();
    ui.painter()
        .add(egui::Shape::line(points, Stroke::new(1.5, color)));
    ui.ctx().request_repaint_after(ANIMATION_FRAME);
}

/// What a transport button controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Record,
    Live,
}

/// The state of an output, as the transport shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    Idle,
    /// Asked to start, or reconnecting: the indicator pulses.
    Starting,
    /// Running for this many seconds.
    Running(u64),
    Failed,
}

/// A large start/stop control for an output: icon, label, state and
/// running time. The tally colour means "on air"; the label says it too.
pub struct TransportButton {
    pub transport: Transport,
    pub state: TransportState,
    pub enabled: bool,
    forced: Option<VisualState>,
}

impl TransportButton {
    pub fn new(transport: Transport, state: TransportState) -> Self {
        Self {
            transport,
            state,
            enabled: true,
            forced: None,
        }
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Draw in `state` regardless of the pointer (specimen only).
    pub fn forced(mut self, state: VisualState) -> Self {
        self.forced = Some(state);
        self
    }
}

impl Widget for TransportButton {
    fn ui(self, ui: &mut Ui) -> Response {
        // Disabled through the `Ui`, so the response says so (and a
        // disabled tooltip shows).
        if !self.enabled && ui.is_enabled() {
            return disabled(ui, |ui| self.ui(ui));
        }
        let (icon, label) = match self.transport {
            Transport::Record => (Icon::Record, "REC"),
            Transport::Live => (Icon::Live, "LIVE"),
        };
        let (rect, response) = ui.allocate_exact_size(
            vec2(TRANSPORT_WIDTH, size::CONTROL_LARGE),
            if self.enabled {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        // Starting includes reconnecting: the output is active, so the
        // action is to stop it.
        let active = matches!(
            self.state,
            TransportState::Running(_) | TransportState::Starting
        );
        // A failed output is finished first; the next click starts anew.
        let failed = self.state == TransportState::Failed;
        let action = match (self.transport, active, failed) {
            (Transport::Record, _, true) => "Finish the failed recording",
            (Transport::Live, _, true) => "End the failed stream",
            (Transport::Record, true, _) => "Stop recording",
            (Transport::Record, false, _) => "Start recording",
            (Transport::Live, true, _) => "End the stream",
            (Transport::Live, false, _) => "Go live",
        };
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, self.enabled, action));
        if !ui.is_rect_visible(rect) {
            return response;
        }
        let state = self
            .forced
            .unwrap_or_else(|| VisualState::of(&response, self.enabled));
        let painter = ui.painter();
        let radius = CornerRadius::same(size::RADIUS_CONTROL);
        let hovered = matches!(state, VisualState::Hovered | VisualState::Pressed);
        let (fill, stroke, fg, icon_color, status) = match self.state {
            TransportState::Running(_) => (
                if hovered {
                    Color32::from_rgb(0xC9, 0x2F, 0x31)
                } else {
                    color::TALLY_PROGRAM
                },
                Stroke::NONE,
                Color32::WHITE,
                Color32::WHITE,
                None,
            ),
            TransportState::Starting => {
                // One slow breath per period: bright, dim, bright.
                let wave =
                    0.5 + 0.5 * (phase(ui, f64::from(motion::PULSE)) * std::f32::consts::TAU).cos();
                let pulse = 0.55 + 0.45 * wave;
                ui.ctx().request_repaint_after(ANIMATION_FRAME);
                (
                    color::BG_RAISED,
                    Stroke::new(size::STROKE, color::STATE_WARN),
                    color::TEXT_PRIMARY,
                    color::STATE_WARN.gamma_multiply(pulse),
                    Some("STARTING"),
                )
            }
            TransportState::Failed => (
                color::BG_RAISED,
                Stroke::new(size::STROKE, color::STATE_ERROR),
                color::TEXT_PRIMARY,
                color::STATE_ERROR,
                Some("FAILED"),
            ),
            TransportState::Idle => (
                if hovered {
                    Color32::from_rgb(0x24, 0x26, 0x2A)
                } else {
                    color::BG_RAISED
                },
                Stroke::new(size::STROKE, color::LINE_STRONG),
                if self.enabled {
                    color::TEXT_PRIMARY
                } else {
                    color::TEXT_DISABLED
                },
                if self.enabled {
                    color::TALLY_PROGRAM
                } else {
                    color::TEXT_DISABLED
                },
                None,
            ),
        };
        // Disabled (e.g. while a stop is in flight) dims every state, so it
        // never looks clickable.
        let dim = |color: Color32| {
            if self.enabled {
                color
            } else {
                color.gamma_multiply(0.45)
            }
        };
        let (fill, stroke, fg, icon_color) = (
            dim(fill),
            Stroke::new(stroke.width, dim(stroke.color)),
            dim(fg),
            dim(icon_color),
        );
        painter.rect(rect, radius, fill, stroke, StrokeKind::Inside);
        let icon_box = icon_rect(pos2(rect.left() + 18.0, rect.center().y), size::ICON);
        if self.state == TransportState::Failed {
            Icon::Error.paint(painter, icon_box, icon_color);
        } else {
            icon.paint(painter, icon_box, icon_color);
        }
        painter.text(
            pos2(rect.left() + 34.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            TextRole::Label.font(),
            fg,
        );
        let trailing = match self.state {
            TransportState::Running(seconds) => Some((timecode(seconds), TextRole::Timecode)),
            _ => status.map(|text| (text.to_owned(), TextRole::Meta)),
        };
        if let Some((text, role)) = trailing {
            let color = dim(if matches!(self.state, TransportState::Running(_)) {
                Color32::WHITE
            } else {
                color::TEXT_SECONDARY
            });
            painter.text(
                pos2(rect.right() - 10.0, rect.center().y),
                Align2::RIGHT_CENTER,
                text,
                role.font(),
                color,
            );
        }
        if shows_focus(self.forced, &response) {
            focus_ring(painter, rect, size::RADIUS_CONTROL);
        }
        response
    }
}

/// Wide enough for the icon, the label and an `HH:MM:SS` timer.
pub const TRANSPORT_WIDTH: f32 = 176.0;

/// `seconds` as `HH:MM:SS`.
pub fn timecode(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeKind {
    /// Filled tally red: on air, recording, program.
    Tally,
    /// Outlined: a neutral fact such as "PREVIEW".
    Neutral,
    Ok,
    Warn,
    Error,
}

/// A short upper-case label: REC, LIVE, PGM, PREVIEW.
pub fn badge(ui: &mut Ui, kind: BadgeKind, label: &str) -> Response {
    let font = TextRole::Label.font();
    let galley = ui.painter().layout_no_wrap(
        label.to_uppercase(),
        egui::FontId::new(10.0, font.family),
        Color32::PLACEHOLDER,
    );
    let size = vec2(galley.size().x + 10.0, 16.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    let radius = CornerRadius::same(2);
    let (fill, stroke, text) = match kind {
        BadgeKind::Tally => (color::TALLY_PROGRAM, Stroke::NONE, Color32::WHITE),
        BadgeKind::Neutral => (
            Color32::TRANSPARENT,
            Stroke::new(1.0, color::LINE_STRONG),
            color::TEXT_SECONDARY,
        ),
        BadgeKind::Ok => (
            Color32::TRANSPARENT,
            Stroke::new(1.0, color::STATE_OK.gamma_multiply(0.7)),
            color::STATE_OK,
        ),
        BadgeKind::Warn => (
            Color32::TRANSPARENT,
            Stroke::new(1.0, color::STATE_WARN.gamma_multiply(0.7)),
            color::STATE_WARN,
        ),
        BadgeKind::Error => (
            Color32::TRANSPARENT,
            Stroke::new(1.0, color::STATE_ERROR.gamma_multiply(0.7)),
            color::STATE_ERROR,
        ),
    };
    painter.rect(rect, radius, fill, stroke, StrokeKind::Inside);
    painter.galley(rect.center() - galley.size() / 2.0, galley, text);
    response
}

/// The lowest level a meter or fader shows, in dBFS.
pub const METER_FLOOR_DB: f32 = -60.0;

/// A level meter: zones (green to −18 dBFS, yellow to −6, red above), the
/// unlit scale dimmed, a peak hold line and a clip latch at the top. Square
/// edges, as on hardware. `level_db` and `peak_db` are dBFS.
pub fn meter(ui: &mut Ui, size_px: Vec2, level_db: f32, peak_db: f32, clipped: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(size_px, Sense::hover());
    paint_meter(ui.painter(), rect, level_db, peak_db, clipped);
    response
}

fn paint_meter(painter: &egui::Painter, rect: Rect, level_db: f32, peak_db: f32, clipped: bool) {
    let vertical = rect.height() > rect.width();
    painter.rect_filled(rect, 0.0, color::BG_INSET);
    let clip_len = 4.0;
    let (bar, clip_rect) = if vertical {
        let clip = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + clip_len));
        (
            Rect::from_min_max(pos2(rect.left(), rect.top() + clip_len + 2.0), rect.max),
            clip,
        )
    } else {
        let clip = Rect::from_min_max(pos2(rect.right() - clip_len, rect.top()), rect.max);
        (
            Rect::from_min_max(rect.min, pos2(rect.right() - clip_len - 2.0, rect.bottom())),
            clip,
        )
    };
    let fraction = |db: f32| ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);
    // The part of the bar between two levels.
    let span = |from: f32, to: f32| {
        let (a, b) = (fraction(from), fraction(to));
        if vertical {
            Rect::from_min_max(
                pos2(bar.left(), bar.bottom() - b * bar.height()),
                pos2(bar.right(), bar.bottom() - a * bar.height()),
            )
        } else {
            Rect::from_min_max(
                pos2(bar.left() + a * bar.width(), bar.top()),
                pos2(bar.left() + b * bar.width(), bar.bottom()),
            )
        }
    };
    for (from, to, zone) in [
        (METER_FLOOR_DB, -18.0, color::METER_LOW),
        (-18.0, -6.0, color::METER_MID),
        (-6.0, 0.0, color::METER_HIGH),
    ] {
        painter.rect_filled(span(from, to), 0.0, zone.gamma_multiply(0.14));
        if level_db > from {
            painter.rect_filled(span(from, level_db.min(to)), 0.0, zone);
        }
    }
    if peak_db > METER_FLOOR_DB {
        let at = fraction(peak_db);
        let line = if vertical {
            let y = bar.bottom() - at * bar.height();
            [pos2(bar.left(), y), pos2(bar.right(), y)]
        } else {
            let x = bar.left() + at * bar.width();
            [pos2(x, bar.top()), pos2(x, bar.bottom())]
        };
        painter.line_segment(line, Stroke::new(2.0, color::TEXT_PRIMARY));
    }
    let clip_color = if clipped {
        color::METER_HIGH
    } else {
        color::METER_HIGH.gamma_multiply(0.18)
    };
    painter.rect_filled(clip_rect, 0.0, clip_color);
}

/// The dB scale beside a vertical meter of the same height.
pub fn meter_scale(ui: &mut Ui, height: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(22.0, height), Sense::hover());
    let bar_top = rect.top() + 6.0;
    let painter = ui.painter();
    for db in [0.0_f32, -6.0, -12.0, -18.0, -30.0, -45.0, -60.0] {
        let fraction = (db - METER_FLOOR_DB) / -METER_FLOOR_DB;
        let y = rect.bottom() - fraction * (rect.bottom() - bar_top);
        painter.text(
            pos2(rect.right() - 2.0, y),
            Align2::RIGHT_CENTER,
            format!("{}", db.abs() as i32),
            egui::FontId::new(9.0, TextRole::Numeric.font().family),
            color::TEXT_TERTIARY,
        );
    }
    response
}

/// Gain change per arrow key or assistive-technology step, in dB.
pub const FADER_STEP_DB: f32 = 0.5;

/// A vertical gain fader over `range` (dB) with a unity mark; `label`
/// names it for assistive technology (e.g. the channel's name).
///
/// - **Pointer:** dragging moves the fader from where it was grabbed; a
///   plain click changes nothing, so touching a live fader never jumps it.
///   Grabbing the track off the thumb moves the thumb there. Near 0 dB a
///   drag snaps to unity, like the detent of a physical fader.
/// - **Keyboard:** with focus, Up and Down change it by [`FADER_STEP_DB`];
///   AccessKit's increment, decrement and set-value do the same.
///
/// The response is changed whenever the value is; callers commit when a
/// drag ends, or at once for keyboard changes, as with egui's sliders. A
/// value outside `range` (set elsewhere) is drawn at the nearest end.
pub fn fader(
    ui: &mut Ui,
    height: f32,
    gain_db: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    label: &str,
) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(24.0, height), Sense::click_and_drag());
    let (min, max) = (*range.start(), *range.end());
    let track = Rect::from_center_size(rect.center(), vec2(4.0, height - 12.0));
    let to_y = |db: f32| track.bottom() - (db.clamp(min, max) - min) / (max - min) * track.height();
    let from_y =
        |y: f32| min + ((track.bottom() - y) / track.height()).clamp(0.0, 1.0) * (max - min);
    let mut value = None;

    // Where on the thumb the drag began, kept for the drag's duration.
    let grab_id = response.id.with("grab");
    if response.drag_started()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let offset = pointer.y - to_y(*gain_db);
        let on_thumb = offset.abs() <= 6.0;
        ui.data_mut(|data| data.insert_temp(grab_id, if on_thumb { offset } else { 0.0 }));
    }
    // Also on the release frame: a caller that rebuilds the value from its
    // model each frame commits there, and must get the dragged value.
    if (response.dragged() || response.drag_stopped())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let offset: f32 = ui.data(|data| data.get_temp(grab_id)).unwrap_or(0.0);
        let dragged = from_y(pointer.y - offset);
        value = Some(if dragged.abs() < 0.6 {
            0.0
        } else {
            (dragged * 10.0).round() / 10.0
        });
    }

    let mut steps = 0i32;
    if response.has_focus() {
        ui.input(|input| {
            steps += input.num_presses(egui::Key::ArrowUp) as i32;
            steps -= input.num_presses(egui::Key::ArrowDown) as i32;
        });
    }
    let (asked, set) = numeric_requests(ui, response.id);
    steps += asked;
    value = set.or(value);
    if steps != 0 {
        value = Some(*gain_db + steps as f32 * FADER_STEP_DB);
    }
    if let Some(value) = value.map(|value| value.clamp(min, max))
        && value != *gain_db
    {
        *gain_db = value;
        response.mark_changed();
    }

    describe_numeric(
        ui,
        &response,
        true,
        *gain_db,
        (min, max),
        FADER_STEP_DB,
        label,
    );

    let painter = ui.painter();
    painter.rect_filled(track, 0.0, color::BG_INSET);
    let unity = to_y(0.0);
    painter.line_segment(
        [
            pos2(rect.left() + 2.0, unity),
            pos2(rect.right() - 2.0, unity),
        ],
        Stroke::new(1.0, color::TEXT_TERTIARY),
    );
    let thumb = Rect::from_center_size(pos2(rect.center().x, to_y(*gain_db)), vec2(22.0, 10.0));
    let active = response.dragged() || response.hovered();
    painter.rect(
        thumb,
        CornerRadius::same(2),
        if active {
            Color32::from_rgb(0x3A, 0x3D, 0x42)
        } else {
            Color32::from_rgb(0x2E, 0x31, 0x35)
        },
        Stroke::new(
            1.0,
            if response.dragged() {
                color::ACCENT
            } else {
                color::LINE_STRONG
            },
        ),
        StrokeKind::Inside,
    );
    painter.line_segment(
        [
            pos2(thumb.left() + 4.0, thumb.center().y),
            pos2(thumb.right() - 4.0, thumb.center().y),
        ],
        Stroke::new(1.0, color::TEXT_PRIMARY),
    );
    if response.has_focus() {
        focus_ring(painter, rect, size::RADIUS_CONTROL);
    }
    response
}

/// What assistive technology asked of a numeric control this frame: the
/// steps up (increment) less those down (decrement), and a value it set.
fn numeric_requests(ui: &Ui, id: egui::Id) -> (i32, Option<f32>) {
    use egui::accesskit::{Action, ActionData};
    ui.input(|input| {
        let steps = input.num_accesskit_action_requests(id, Action::Increment) as i32
            - input.num_accesskit_action_requests(id, Action::Decrement) as i32;
        let set = input
            .accesskit_action_requests(id, Action::SetValue)
            .filter_map(|request| match request.data {
                Some(ActionData::NumericValue(target)) => Some(target as f32),
                _ => None,
            })
            .last();
        (steps, set)
    })
}

/// Describe a numeric control (a fader, a slider) to assistive
/// technology: its value, range and step, and the actions it takes.
fn describe_numeric(
    ui: &Ui,
    response: &Response,
    enabled: bool,
    value: f32,
    (min, max): (f32, f32),
    step: f32,
    label: &str,
) {
    use egui::accesskit::Action;
    response.widget_info(|| WidgetInfo::slider(enabled, f64::from(value), label));
    ui.ctx().accesskit_node_builder(response.id, |builder| {
        builder.set_min_numeric_value(f64::from(min));
        builder.set_max_numeric_value(f64::from(max));
        builder.set_numeric_value_step(f64::from(step));
        if !enabled {
            return;
        }
        builder.add_action(Action::SetValue);
        if value < max {
            builder.add_action(Action::Increment);
        }
        if value > min {
            builder.add_action(Action::Decrement);
        }
    });
}

/// A horizontal slider over `range`, for a setting such as a source's
/// opacity (live gain has the [`fader`]); the caller puts a number field
/// for the exact value beside it.
///
/// - **Pointer:** a click or drag on the track sets the value under the
///   pointer, rounded to `step`.
/// - **Keyboard:** with focus, Left and Right change it by `step` (and do
///   not move the focus); AccessKit's increment, decrement and set-value do
///   the same.
///
/// The response is changed whenever the value is; callers commit as for
/// the fader. `label` names it for assistive technology. Disabled with the
/// `Ui`.
pub fn slider(
    ui: &mut Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
    width: f32,
    label: &str,
) -> Response {
    const THUMB: f32 = 12.0;
    debug_assert!(
        range.start() < range.end() && step > 0.0,
        "a slider needs a range and a step"
    );
    let enabled = ui.is_enabled();
    let sense = if enabled {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let (rect, mut response) = ui.allocate_exact_size(vec2(width, size::CONTROL), sense);
    let (min, max) = (*range.start(), *range.end());
    let track = Rect::from_center_size(rect.center(), vec2(width - THUMB, 4.0));
    let to_x = |v: f32| track.left() + (v.clamp(min, max) - min) / (max - min) * track.width();
    let snap = |v: f32| ((v - min) / step).round() * step + min;
    let mut new = None;
    // Also on the release frame, as for the fader.
    if (response.clicked() || response.dragged() || response.drag_stopped())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let t = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0);
        new = Some(snap(min + t * (max - min)));
    }
    let mut steps = 0i32;
    if response.has_focus() {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    ..Default::default()
                },
            );
        });
        ui.input(|input| {
            steps += input.num_presses(egui::Key::ArrowRight) as i32;
            steps -= input.num_presses(egui::Key::ArrowLeft) as i32;
        });
    }
    let (asked, set) = numeric_requests(ui, response.id);
    steps += asked;
    new = set.or(new);
    if steps != 0 {
        new = Some(snap(*value) + steps as f32 * step);
    }
    if enabled
        && let Some(new) = new.map(|new| new.clamp(min, max))
        && new != *value
    {
        *value = new;
        response.mark_changed();
    }
    describe_numeric(ui, &response, enabled, *value, (min, max), step, label);
    if !ui.is_rect_visible(rect) {
        return response;
    }

    let painter = ui.painter();
    painter.rect_filled(track, CornerRadius::same(2), color::BG_INSET);
    let x = to_x(*value);
    let filled = Rect::from_min_max(track.min, pos2(x, track.bottom()));
    painter.rect_filled(
        filled,
        CornerRadius::same(2),
        if enabled {
            color::TEXT_TERTIARY
        } else {
            color::LINE_SUBTLE
        },
    );
    let thumb = Rect::from_center_size(pos2(x, rect.center().y), Vec2::splat(THUMB));
    let (fill, outline) = if !enabled {
        (color::BG_RAISED, color::LINE_SUBTLE)
    } else if response.dragged() {
        (Color32::from_rgb(0x3A, 0x3D, 0x42), color::ACCENT)
    } else if response.hovered() {
        (Color32::from_rgb(0x3A, 0x3D, 0x42), color::TEXT_TERTIARY)
    } else {
        (Color32::from_rgb(0x2E, 0x31, 0x35), color::LINE_STRONG)
    };
    painter.rect(
        thumb,
        CornerRadius::same(2),
        fill,
        Stroke::new(size::STROKE, outline),
        StrokeKind::Inside,
    );
    if response.has_focus() {
        focus_ring(painter, thumb, 2);
    }
    response
}

/// The tracking of section labels (`TextRole::Section`).
const SECTION_TRACKING: f32 = 0.6;

/// A dock or section heading: the label, an optional count, and room for
/// actions on the right (returned rect).
pub fn section_header(ui: &mut Ui, label: &str, count: Option<usize>) -> Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
    let painter = ui.painter();
    let mut job = LayoutJob::default();
    job.append(
        &label.to_uppercase(),
        0.0,
        egui::TextFormat {
            font_id: TextRole::Section.font(),
            color: TextRole::Section.color(),
            extra_letter_spacing: SECTION_TRACKING,
            ..Default::default()
        },
    );
    let galley = painter.layout_job(job);
    let text_pos = pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
    let label_width = galley.size().x;
    painter.galley(text_pos, galley, TextRole::Section.color());
    if let Some(count) = count {
        painter.text(
            pos2(rect.left() + label_width + 6.0, rect.center().y),
            Align2::LEFT_CENTER,
            count.to_string(),
            TextRole::Meta.font(),
            color::TEXT_TERTIARY,
        );
    }
    Rect::from_min_max(pos2(rect.left() + label_width + 24.0, rect.top()), rect.max)
}

/// A selectable row with an icon, a name and trailing metadata. The
/// selection (the thing being edited) is an accent bar and tint; the
/// program (what is on air) is a tally dot and "PGM". The two marks are
/// separate, so a selected scene that is not on air reads as such.
pub struct ListRow<'a> {
    pub icon: Icon,
    pub label: &'a str,
    pub meta: Option<&'a str>,
    pub selected: bool,
    pub program: bool,
    forced: Option<VisualState>,
}

impl<'a> ListRow<'a> {
    pub fn new(icon: Icon, label: &'a str) -> Self {
        Self {
            icon,
            label,
            meta: None,
            selected: false,
            program: false,
            forced: None,
        }
    }

    pub fn meta(mut self, meta: &'a str) -> Self {
        self.meta = Some(meta);
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn program(mut self, program: bool) -> Self {
        self.program = program;
        self
    }

    /// Draw in `state` regardless of the pointer (specimen only).
    pub fn forced(mut self, state: VisualState) -> Self {
        self.forced = Some(state);
        self
    }
}

impl Widget for ListRow<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), size::ROW), Sense::click());
        response.widget_info(|| {
            WidgetInfo::selected(WidgetType::SelectableLabel, true, self.selected, self.label)
        });
        if !ui.is_rect_visible(rect) {
            return response;
        }
        let state = self
            .forced
            .unwrap_or_else(|| VisualState::of(&response, true));
        let painter = ui.painter();
        let hover = ui.ctx().animate_bool_with_time(
            response.id.with("hover"),
            matches!(state, VisualState::Hovered | VisualState::Pressed),
            motion::HOVER,
        );
        if self.selected {
            painter.rect_filled(rect, 0.0, color::ACCENT_WEAK);
            painter.rect_filled(
                Rect::from_min_max(
                    rect.min,
                    pos2(rect.left() + size::STROKE_HEAVY, rect.bottom()),
                ),
                0.0,
                color::ACCENT,
            );
        } else if hover > 0.0 {
            painter.rect_filled(rect, 0.0, color::BG_RAISED.gamma_multiply(hover));
        }
        let text = if self.selected {
            color::TEXT_PRIMARY
        } else {
            color::TEXT_SECONDARY
        };
        self.icon.paint(
            painter,
            icon_rect(pos2(rect.left() + 16.0, rect.center().y), 14.0),
            if self.selected {
                color::TEXT_PRIMARY
            } else {
                color::TEXT_TERTIARY
            },
        );
        let mut right = rect.right() - size::SPACE_8;
        if self.program {
            let galley = painter.layout_no_wrap(
                "PGM".into(),
                egui::FontId::new(10.0, TextRole::Label.font().family),
                color::TALLY_PROGRAM,
            );
            right -= galley.size().x;
            painter.galley(
                pos2(right, rect.center().y - galley.size().y / 2.0),
                galley,
                color::TALLY_PROGRAM,
            );
            right -= 8.0;
            painter.circle_filled(pos2(right, rect.center().y), 3.5, color::TALLY_PROGRAM);
            right -= 10.0;
        }
        if let Some(meta) = self.meta {
            let meta = painter.text(
                pos2(right, rect.center().y),
                Align2::RIGHT_CENTER,
                meta,
                TextRole::Meta.font(),
                color::TEXT_TERTIARY,
            );
            right = meta.left() - size::SPACE_8;
        }
        // The name gets what is left, cut with an ellipsis.
        let left = rect.left() + 30.0;
        let mut job = LayoutJob::single_section(
            self.label.to_owned(),
            egui::TextFormat::simple(TextRole::Body.font(), text),
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width((right - left).max(0.0));
        let galley = painter.layout_job(job);
        painter.galley(
            pos2(left, rect.center().y - galley.size().y / 2.0),
            galley,
            text,
        );
        if shows_focus(self.forced, &response) {
            painter.rect_stroke(
                rect.shrink(1.0),
                0.0,
                Stroke::new(size::STROKE_HEAVY, color::ACCENT),
                StrokeKind::Inside,
            );
        }
        response
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLevel {
    Ok,
    Info,
    Warn,
    Error,
}

/// An icon and a line of text; the icon says the level, not only the colour.
pub fn status_line(ui: &mut Ui, level: StatusLevel, text: &str) -> Response {
    let (icon, tint) = status_icon(level);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = size::SPACE_6;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        icon.paint(ui.painter(), rect, tint);
        let color = if level == StatusLevel::Info {
            color::TEXT_SECONDARY
        } else {
            tint
        };
        ui.add(egui::Label::new(TextRole::Body.text(text).color(color)).wrap());
    })
    .response
}

/// The icon and colour that say a status level.
fn status_icon(level: StatusLevel) -> (Icon, Color32) {
    match level {
        StatusLevel::Ok => (Icon::Check, color::STATE_OK),
        StatusLevel::Info => (Icon::ChevronRight, color::TEXT_TERTIARY),
        StatusLevel::Warn => (Icon::Warning, color::STATE_WARN),
        StatusLevel::Error => (Icon::Error, color::STATE_ERROR),
    }
}

/// An on/off switch for binary settings (the preview, mute). Disabled
/// with the `Ui`.
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let enabled = ui.is_enabled();
    let (rect, mut response) = ui.allocate_exact_size(vec2(30.0, 16.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, enabled, *on, label));
    let t = ui
        .ctx()
        .animate_bool_with_time(response.id, *on, motion::HOVER);
    let painter = ui.painter();
    let (track, knob) = match (enabled, *on) {
        (true, true) => (color::ACCENT, color::BG_APP),
        (true, false) => (color::LINE_STRONG, color::TEXT_SECONDARY),
        (false, true) => (color::ACCENT.gamma_multiply(0.35), color::BG_APP),
        (false, false) => (color::LINE_SUBTLE, color::TEXT_DISABLED),
    };
    painter.rect_filled(rect, CornerRadius::same(8), track);
    let x = egui::lerp((rect.left() + 8.0)..=(rect.right() - 8.0), t);
    painter.circle_filled(pos2(x, rect.center().y), 6.0, knob);
    if response.has_focus() {
        focus_ring(painter, rect, 8);
    }
    response
}

/// One choice of several, e.g. the bus an output takes as its mix: a ring,
/// filled in the accent when selected. `label` names the choice for
/// assistive technology and the tooltip. Disabled with the `Ui`.
pub fn radio(ui: &mut Ui, selected: bool, label: &str) -> Response {
    let enabled = ui.is_enabled();
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
    response
        .widget_info(|| WidgetInfo::selected(WidgetType::RadioButton, enabled, selected, label));
    if ui.is_rect_visible(rect) {
        let state = VisualState::of(&response, enabled);
        let painter = ui.painter();
        let center = rect.center();
        let ring = match (state, selected) {
            (VisualState::Disabled, _) => color::LINE_SUBTLE,
            (_, true) => color::ACCENT,
            (VisualState::Hovered | VisualState::Pressed, false) => color::TEXT_SECONDARY,
            (_, false) => color::LINE_STRONG,
        };
        painter.circle(
            center,
            6.5,
            color::BG_INSET,
            Stroke::new(size::STROKE_HEAVY, ring),
        );
        if selected {
            let dot = if enabled {
                color::ACCENT
            } else {
                color::TEXT_DISABLED
            };
            painter.circle_filled(center, 3.0, dot);
        }
        if response.has_focus() {
            focus_ring(painter, rect, 9);
        }
    }
    response.on_hover_text(label)
}

/// A segmented switch between the views of one dock (Console, Routing):
/// the selected segment has primary text and an accent bar under it.
/// Returns the segment clicked this frame, if any.
pub fn tabs(ui: &mut Ui, selected: usize, labels: &[&str]) -> Option<usize> {
    let galleys: Vec<_> = labels
        .iter()
        .map(|label| {
            ui.painter().layout_no_wrap(
                (*label).to_owned(),
                TextRole::Label.font(),
                Color32::PLACEHOLDER,
            )
        })
        .collect();
    let widths: Vec<f32> = galleys
        .iter()
        .map(|galley| galley.size().x + 2.0 * size::SPACE_8)
        .collect();
    let gap = size::SPACE_2;
    let total = widths.iter().sum::<f32>() + gap * labels.len().saturating_sub(1) as f32;
    let mut clicked = None;
    // Left to right also inside a right-to-left header.
    ui.allocate_ui_with_layout(
        vec2(total, size::CONTROL),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (index, (label, galley)) in labels.iter().zip(galleys).enumerate() {
                let on = index == selected;
                let (rect, response) =
                    ui.allocate_exact_size(vec2(widths[index], size::CONTROL), Sense::click());
                response.widget_info(|| {
                    WidgetInfo::selected(WidgetType::SelectableLabel, true, on, *label)
                });
                if response.clicked() && !on {
                    clicked = Some(index);
                }
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let state = VisualState::of(&response, true);
                let painter = ui.painter();
                if matches!(state, VisualState::Hovered | VisualState::Pressed) {
                    let (fill, _, _) = button_colors(ButtonKind::Ghost, state);
                    painter.rect_filled(rect, CornerRadius::same(size::RADIUS_CONTROL), fill);
                }
                let text = if on {
                    color::TEXT_PRIMARY
                } else {
                    color::TEXT_SECONDARY
                };
                painter.galley(rect.center() - galley.size() / 2.0, galley, text);
                if on {
                    painter.rect_filled(
                        Rect::from_min_max(
                            pos2(
                                rect.left() + size::SPACE_4,
                                rect.bottom() - size::STROKE_HEAVY,
                            ),
                            pos2(rect.right() - size::SPACE_4, rect.bottom()),
                        ),
                        0.0,
                        color::ACCENT,
                    );
                }
                if response.has_focus() {
                    focus_ring(painter, rect, size::RADIUS_CONTROL);
                }
            }
        },
    );
    clicked
}

/// One choice of a few, side by side in one outlined control (a source's
/// kind, a channel's input, an encoder): the current one is filled in the
/// weak accent with its text in the accent. Each segment is a radio
/// button for assistive technology. Disabled with the `Ui`. Returns
/// whether a click changed `current`.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, choices: &[(T, &str)]) -> bool {
    let enabled = ui.is_enabled();
    let segments: Vec<_> = choices
        .iter()
        .map(|(value, label)| {
            let galley = ui.painter().layout_no_wrap(
                (*label).to_owned(),
                TextRole::Body.font(),
                Color32::PLACEHOLDER,
            );
            let width = galley.size().x + 2.0 * size::SPACE_12;
            (*value, *label, galley, width)
        })
        .collect();
    let total = segments.iter().map(|segment| segment.3).sum::<f32>();
    let mut changed = false;
    // Left to right also inside a right-to-left row.
    ui.allocate_ui_with_layout(
        vec2(total, size::CONTROL),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let outline = Rect::from_min_size(ui.cursor().min, vec2(total, size::CONTROL));
            let radius = CornerRadius::same(size::RADIUS_CONTROL);
            ui.painter().rect_filled(outline, radius, color::BG_INSET);
            let mut focused = None;
            let last = segments.len().saturating_sub(1);
            for (index, (value, label, galley, width)) in segments.into_iter().enumerate() {
                let on = value == *current;
                let (rect, response) =
                    ui.allocate_exact_size(vec2(width, size::CONTROL), Sense::click());
                response.widget_info(|| {
                    WidgetInfo::selected(WidgetType::RadioButton, enabled, on, label)
                });
                if response.clicked() && !on {
                    *current = value;
                    changed = true;
                }
                if response.has_focus() {
                    focused = Some(rect);
                }
                let state = VisualState::of(&response, enabled);
                let painter = ui.painter();
                // The outer corners follow the outline's.
                let first = if index == 0 { size::RADIUS_CONTROL } else { 0 };
                let end = if index == last {
                    size::RADIUS_CONTROL
                } else {
                    0
                };
                let corners = CornerRadius {
                    nw: first,
                    sw: first,
                    ne: end,
                    se: end,
                };
                let (fill, text) = match (state, on) {
                    (VisualState::Disabled, true) => (color::BG_RAISED, color::TEXT_DISABLED),
                    (VisualState::Disabled, false) => (Color32::TRANSPARENT, color::TEXT_DISABLED),
                    (_, true) => (color::ACCENT_WEAK, color::ACCENT),
                    (VisualState::Hovered | VisualState::Pressed, false) => {
                        let (fill, _, _) = button_colors(ButtonKind::Ghost, state);
                        (fill, color::TEXT_PRIMARY)
                    }
                    (_, false) => (Color32::TRANSPARENT, color::TEXT_SECONDARY),
                };
                painter.rect_filled(rect, corners, fill);
                if index > 0 {
                    painter.vline(
                        rect.left(),
                        rect.y_range().shrink(size::SPACE_4),
                        Stroke::new(size::STROKE, color::LINE_SUBTLE),
                    );
                }
                painter.galley(rect.center() - galley.size() / 2.0, galley, text);
            }
            let painter = ui.painter();
            painter.rect_stroke(
                outline,
                radius,
                Stroke::new(
                    size::STROKE,
                    if enabled {
                        color::LINE_STRONG
                    } else {
                        color::LINE_SUBTLE
                    },
                ),
                StrokeKind::Inside,
            );
            // Last, so the next segment does not cover it.
            if let Some(rect) = focused {
                focus_ring(painter, rect, size::RADIUS_CONTROL);
            }
        },
    );
    changed
}

/// A small button that shows a mode by its text, e.g. a send's tap (PRE or
/// POST). `marked` draws it in the accent, for the less usual mode, so it
/// stands out in a table. `label` says what it is and what a click does:
/// it is the tooltip and the name for assistive technology. Disabled with
/// the `Ui`.
pub fn chip(ui: &mut Ui, text: &str, marked: bool, label: &str) -> Response {
    let enabled = ui.is_enabled();
    let galley = ui.painter().layout_no_wrap(
        text.to_uppercase(),
        egui::FontId::new(10.0, TextRole::Label.font().family),
        Color32::PLACEHOLDER,
    );
    let size_px = vec2((galley.size().x + 10.0).max(36.0), 18.0);
    let (rect, response) = ui.allocate_exact_size(size_px, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        let state = VisualState::of(&response, enabled);
        let (fill, _, _) = button_colors(ButtonKind::Ghost, state);
        let (stroke, text_color) = match (state, marked) {
            (VisualState::Disabled, _) => (color::LINE_SUBTLE, color::TEXT_DISABLED),
            (_, true) => (color::ACCENT.gamma_multiply(0.7), color::ACCENT),
            (_, false) => (color::LINE_STRONG, color::TEXT_SECONDARY),
        };
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(2),
            fill,
            Stroke::new(size::STROKE, stroke),
            StrokeKind::Inside,
        );
        painter.galley(rect.center() - galley.size() / 2.0, galley, text_color);
        if response.has_focus() {
            focus_ring(painter, rect, 2);
        }
    }
    response.on_hover_text(label)
}

/// A 1 px divider across the available width.
pub fn divider(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, color::LINE_SUBTLE);
}
