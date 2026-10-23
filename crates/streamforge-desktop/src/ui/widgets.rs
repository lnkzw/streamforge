//! Components of the design system. Views compose these; they do not paint
//! controls of their own. Every interactive component reports a role and
//! label to AccessKit and draws the same focus ring.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2,
    Widget, WidgetInfo, WidgetType, pos2, vec2,
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
        if state == VisualState::Focused {
            focus_ring(painter, rect, size::RADIUS_CONTROL);
        }
        response
    }
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
    let time = ui.input(|input| input.time) as f32;
    let start = time * 6.0;
    let radius = rect.width() / 2.0 - 1.0;
    let points: Vec<_> = (0..=20)
        .map(|i| {
            let t = start + i as f32 / 20.0 * 4.4;
            rect.center() + vec2(t.cos(), t.sin()) * radius
        })
        .collect();
    ui.painter()
        .add(egui::Shape::line(points, Stroke::new(1.5, color)));
    ui.ctx().request_repaint();
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
        let action = match (self.transport, self.state) {
            (Transport::Record, TransportState::Running(_)) => "Stop recording",
            (Transport::Record, _) => "Start recording",
            (Transport::Live, TransportState::Running(_)) => "End the stream",
            (Transport::Live, _) => "Go live",
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
                let time = ui.input(|input| input.time) as f32;
                let pulse =
                    0.55 + 0.45 * (time * std::f32::consts::TAU / motion::PULSE).cos().abs();
                ui.ctx().request_repaint();
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
            let color = if matches!(self.state, TransportState::Running(_)) {
                Color32::WHITE
            } else {
                color::TEXT_SECONDARY
            };
            painter.text(
                pos2(rect.right() - 10.0, rect.center().y),
                Align2::RIGHT_CENTER,
                text,
                role.font(),
                color,
            );
        }
        if state == VisualState::Focused {
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

/// A vertical gain fader from `range` (dB) with a unity mark. The returned
/// response is dragged or changed like a slider's; callers commit when a
/// drag ends.
pub fn fader(
    ui: &mut Ui,
    height: f32,
    gain_db: &mut f32,
    range: std::ops::RangeInclusive<f32>,
) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(24.0, height), Sense::click_and_drag());
    let (min, max) = (*range.start(), *range.end());
    let track = Rect::from_center_size(rect.center(), vec2(4.0, height - 12.0));
    let to_y = |db: f32| track.bottom() - (db - min) / (max - min) * track.height();
    if let Some(pointer) = response.interact_pointer_pos()
        && (response.dragged() || response.clicked())
    {
        let fraction = ((track.bottom() - pointer.y) / track.height()).clamp(0.0, 1.0);
        let value = min + fraction * (max - min);
        // Snap to unity near 0 dB, as a physical fader detents.
        let value = if value.abs() < 0.6 {
            0.0
        } else {
            (value * 10.0).round() / 10.0
        };
        if value != *gain_db {
            *gain_db = value;
            response.mark_changed();
        }
    }
    response.widget_info(|| WidgetInfo::slider(true, f64::from(*gain_db), "gain"));
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

/// A dock or section heading: the label, an optional count, and room for
/// actions on the right (returned rect).
pub fn section_header(ui: &mut Ui, label: &str, count: Option<usize>) -> Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
    let painter = ui.painter();
    let galley = painter.layout_no_wrap(
        label.to_uppercase(),
        TextRole::Section.font(),
        TextRole::Section.color(),
    );
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
        painter.text(
            pos2(rect.left() + 30.0, rect.center().y),
            Align2::LEFT_CENTER,
            self.label,
            TextRole::Body.font(),
            text,
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
            painter.text(
                pos2(right, rect.center().y),
                Align2::RIGHT_CENTER,
                meta,
                TextRole::Meta.font(),
                color::TEXT_TERTIARY,
            );
        }
        if state == VisualState::Focused {
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
    let (icon, tint) = match level {
        StatusLevel::Ok => (Icon::Check, color::STATE_OK),
        StatusLevel::Info => (Icon::ChevronRight, color::TEXT_TERTIARY),
        StatusLevel::Warn => (Icon::Warning, color::STATE_WARN),
        StatusLevel::Error => (Icon::Error, color::STATE_ERROR),
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = size::SPACE_6;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        icon.paint(ui.painter(), rect, tint);
        let color = if level == StatusLevel::Info {
            color::TEXT_SECONDARY
        } else {
            tint
        };
        ui.label(TextRole::Body.text(text).color(color));
    })
    .response
}

/// An on/off switch for binary settings (the preview, mute).
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(30.0, 16.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, *on, label));
    let t = ui
        .ctx()
        .animate_bool_with_time(response.id, *on, motion::HOVER);
    let painter = ui.painter();
    let track = if *on {
        color::ACCENT
    } else {
        color::LINE_STRONG
    };
    painter.rect_filled(rect, CornerRadius::same(8), track);
    let x = egui::lerp((rect.left() + 8.0)..=(rect.right() - 8.0), t);
    painter.circle_filled(
        pos2(x, rect.center().y),
        6.0,
        if *on {
            color::BG_APP
        } else {
            color::TEXT_SECONDARY
        },
    );
    if response.has_focus() {
        focus_ring(painter, rect, 8);
    }
    response
}

/// A 1 px divider across the available width.
pub fn divider(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, color::LINE_SUBTLE);
}
