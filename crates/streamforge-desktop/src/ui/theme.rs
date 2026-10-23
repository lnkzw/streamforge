//! Design tokens of the "Control Room" design system
//! (`docs/ui/design-system.md`) and the egui style built from them. Views
//! take colours, sizes and text roles from here, never their own.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Shadow, Stroke, TextStyle,
    Vec2,
};

use super::fonts;

/// Colours. Surfaces are neutral greys; the accent marks the operator's own
/// focus and selection; tally and state colours carry meaning only.
pub mod color {
    use eframe::egui::Color32;

    pub const BG_APP: Color32 = Color32::from_rgb(0x10, 0x11, 0x12);
    pub const BG_PANEL: Color32 = Color32::from_rgb(0x16, 0x17, 0x1A);
    pub const BG_RAISED: Color32 = Color32::from_rgb(0x1D, 0x1F, 0x22);
    pub const BG_PRESSED: Color32 = Color32::from_rgb(0x19, 0x1A, 0x1D);
    pub const BG_INSET: Color32 = Color32::from_rgb(0x0B, 0x0C, 0x0D);
    pub const LINE_SUBTLE: Color32 = Color32::from_rgb(0x25, 0x27, 0x2B);
    pub const LINE_STRONG: Color32 = Color32::from_rgb(0x33, 0x36, 0x3B);
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xE7, 0xE8, 0xEA);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xA2, 0xA6, 0xAC);
    pub const TEXT_TERTIARY: Color32 = Color32::from_rgb(0x6C, 0x70, 0x77);
    pub const TEXT_DISABLED: Color32 = Color32::from_rgb(0x46, 0x4A, 0x50);
    pub const ACCENT: Color32 = Color32::from_rgb(0x2C, 0xB5, 0xC2);
    /// `ACCENT` at 14 % over `BG_PANEL`.
    pub const ACCENT_WEAK: Color32 = Color32::from_rgb(0x19, 0x2E, 0x33);
    pub const TALLY_PROGRAM: Color32 = Color32::from_rgb(0xE2, 0x39, 0x3B);
    pub const STATE_OK: Color32 = Color32::from_rgb(0x3F, 0xBF, 0x72);
    pub const STATE_WARN: Color32 = Color32::from_rgb(0xE6, 0xA2, 0x3C);
    pub const STATE_ERROR: Color32 = Color32::from_rgb(0xF2, 0x68, 0x3D);
    pub const METER_LOW: Color32 = Color32::from_rgb(0x3F, 0xBF, 0x72);
    pub const METER_MID: Color32 = Color32::from_rgb(0xE6, 0xC4, 0x3C);
    pub const METER_HIGH: Color32 = Color32::from_rgb(0xE2, 0x39, 0x3B);
}

/// The spacing scale and fixed sizes, in logical pixels.
pub mod size {
    pub const SPACE_2: f32 = 2.0;
    pub const SPACE_4: f32 = 4.0;
    pub const SPACE_6: f32 = 6.0;
    pub const SPACE_8: f32 = 8.0;
    pub const SPACE_12: f32 = 12.0;
    pub const SPACE_16: f32 = 16.0;
    pub const SPACE_24: f32 = 24.0;
    /// Dense control height (buttons, fields, toggles).
    pub const CONTROL: f32 = 24.0;
    /// Transport buttons.
    pub const CONTROL_LARGE: f32 = 32.0;
    pub const ROW: f32 = 26.0;
    pub const ICON: f32 = 16.0;
    pub const RADIUS_CONTROL: u8 = 3;
    pub const RADIUS_POPUP: u8 = 4;
    pub const STROKE: f32 = 1.0;
    /// Focus rings, selection bars, tally frames.
    pub const STROKE_HEAVY: f32 = 2.0;
}

/// Durations of the motion conventions, in seconds.
pub mod motion {
    pub const HOVER: f32 = 0.12;
    pub const PANEL: f32 = 0.16;
    /// One period of the slow pulse of a state waiting on the network.
    pub const PULSE: f32 = 1.2;
}

/// The roles text plays; each has one face, size and colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRole {
    /// Dialog titles, the project name.
    Title,
    /// Dock and section labels (upper case, tracked).
    Section,
    /// Rows, buttons, inputs.
    Body,
    /// Control labels and badges.
    Label,
    /// Secondary facts and hints.
    Meta,
    /// Numbers that must line up: dB, counters, rates.
    Numeric,
    /// Running timers in the transport.
    Timecode,
}

impl TextRole {
    pub fn font(self) -> FontId {
        let named = |name: &str| FontFamily::Name(name.into());
        match self {
            Self::Title => FontId::new(14.0, named(fonts::SANS_SEMIBOLD_FAMILY)),
            Self::Section => FontId::new(11.0, named(fonts::SANS_MEDIUM_FAMILY)),
            Self::Body => FontId::new(13.0, FontFamily::Proportional),
            Self::Label => FontId::new(12.0, named(fonts::SANS_MEDIUM_FAMILY)),
            Self::Meta => FontId::new(11.0, FontFamily::Proportional),
            Self::Numeric => FontId::new(12.0, named(fonts::MONO_MEDIUM_FAMILY)),
            Self::Timecode => FontId::new(16.0, named(fonts::MONO_MEDIUM_FAMILY)),
        }
    }

    pub fn color(self) -> Color32 {
        match self {
            Self::Title | Self::Body | Self::Numeric | Self::Timecode => color::TEXT_PRIMARY,
            Self::Label => color::TEXT_SECONDARY,
            Self::Section | Self::Meta => color::TEXT_TERTIARY,
        }
    }

    /// `text` in this role: its font, colour and, for sections, upper case
    /// with tracking.
    pub fn text(self, text: impl Into<String>) -> RichText {
        let text: String = text.into();
        let rich = match self {
            Self::Section => RichText::new(text.to_uppercase()).extra_letter_spacing(0.6),
            _ => RichText::new(text),
        };
        rich.font(self.font()).color(self.color())
    }
}

/// Install the fonts and the style. Call once, when the app starts.
pub fn apply(ctx: &egui::Context) {
    fonts::install(ctx);
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(style);
}

fn style(style: &mut egui::Style) {
    use color::*;
    use size::*;

    style.text_styles = [
        (TextStyle::Small, TextRole::Meta.font()),
        (TextStyle::Body, TextRole::Body.font()),
        (TextStyle::Button, TextRole::Body.font()),
        (TextStyle::Heading, TextRole::Title.font()),
        (TextStyle::Monospace, TextRole::Numeric.font()),
    ]
    .into();

    let spacing = &mut style.spacing;
    spacing.item_spacing = Vec2::new(SPACE_6, SPACE_6);
    spacing.button_padding = Vec2::new(SPACE_8, SPACE_4);
    spacing.interact_size = Vec2::new(40.0, CONTROL);
    spacing.window_margin = Margin::same(SPACE_12 as i8);
    spacing.menu_margin = Margin::same(SPACE_4 as i8);
    spacing.indent = SPACE_16;
    spacing.slider_width = 120.0;
    spacing.slider_rail_height = 4.0;
    spacing.combo_height = 240.0;
    spacing.icon_width = 14.0;
    spacing.icon_width_inner = 8.0;
    spacing.icon_spacing = SPACE_6;
    spacing.tooltip_width = 320.0;

    let visuals = &mut style.visuals;
    visuals.dark_mode = true;
    visuals.override_text_color = None;
    visuals.panel_fill = BG_PANEL;
    visuals.window_fill = BG_PANEL;
    visuals.window_stroke = Stroke::new(STROKE, LINE_STRONG);
    visuals.window_corner_radius = CornerRadius::same(RADIUS_POPUP);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS_POPUP);
    visuals.window_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(110),
    };
    visuals.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(100),
    };
    visuals.faint_bg_color = BG_RAISED;
    visuals.extreme_bg_color = BG_INSET;
    visuals.text_edit_bg_color = Some(BG_INSET);
    visuals.code_bg_color = BG_INSET;
    visuals.hyperlink_color = ACCENT;
    visuals.warn_fg_color = STATE_WARN;
    visuals.error_fg_color = STATE_ERROR;
    visuals.selection.bg_fill = ACCENT_WEAK;
    visuals.selection.stroke = Stroke::new(STROKE, ACCENT);
    visuals.text_cursor.stroke = Stroke::new(STROKE_HEAVY, ACCENT);
    visuals.slider_trailing_fill = true;
    visuals.striped = false;
    visuals.indent_has_left_vline = false;
    visuals.collapsing_header_frame = false;
    visuals.disabled_alpha = 0.45;

    let radius = CornerRadius::same(RADIUS_CONTROL);
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = BG_PANEL;
    widgets.noninteractive.weak_bg_fill = BG_PANEL;
    widgets.noninteractive.bg_stroke = Stroke::new(STROKE, LINE_SUBTLE);
    widgets.noninteractive.fg_stroke = Stroke::new(STROKE, TEXT_SECONDARY);
    widgets.noninteractive.corner_radius = radius;
    widgets.inactive.bg_fill = BG_RAISED;
    widgets.inactive.weak_bg_fill = BG_RAISED;
    widgets.inactive.bg_stroke = Stroke::new(STROKE, LINE_STRONG);
    widgets.inactive.fg_stroke = Stroke::new(STROKE, TEXT_PRIMARY);
    widgets.inactive.corner_radius = radius;
    widgets.inactive.expansion = 0.0;
    widgets.hovered.bg_fill = Color32::from_rgb(0x24, 0x26, 0x2A);
    widgets.hovered.weak_bg_fill = Color32::from_rgb(0x24, 0x26, 0x2A);
    widgets.hovered.bg_stroke = Stroke::new(STROKE, Color32::from_rgb(0x44, 0x48, 0x4E));
    widgets.hovered.fg_stroke = Stroke::new(STROKE, TEXT_PRIMARY);
    widgets.hovered.corner_radius = radius;
    widgets.hovered.expansion = 0.0;
    widgets.active.bg_fill = BG_PRESSED;
    widgets.active.weak_bg_fill = BG_PRESSED;
    widgets.active.bg_stroke = Stroke::new(STROKE, ACCENT);
    widgets.active.fg_stroke = Stroke::new(STROKE, TEXT_PRIMARY);
    widgets.active.corner_radius = radius;
    widgets.active.expansion = 0.0;
    widgets.open.bg_fill = BG_RAISED;
    widgets.open.weak_bg_fill = BG_RAISED;
    widgets.open.bg_stroke = Stroke::new(STROKE, ACCENT);
    widgets.open.fg_stroke = Stroke::new(STROKE, TEXT_PRIMARY);
    widgets.open.corner_radius = radius;
}
