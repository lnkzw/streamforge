//! Icons drawn with the painter on a 16 px grid with a 1.5 px stroke and
//! round joins: one weight everywhere, no icon font, no glyph that a font
//! might lack.

use eframe::egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2, epaint::PathShape};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// A filled dot: recording.
    Record,
    /// Concentric arcs: on air.
    Live,
    /// A filled square.
    Stop,
    /// An eye: the preview, a visible layer.
    Preview,
    /// An eye, struck through: a hidden layer.
    Hidden,
    /// Stacked frames: scenes.
    Scene,
    /// Stacked sheets: sources (layers).
    Layers,
    /// A monitor: screen capture.
    Screen,
    /// A filled swatch: a solid colour.
    Solid,
    /// Vertical bars: the test pattern.
    Bars,
    /// A sine wave: a test tone.
    Tone,
    /// A microphone: a live input.
    Mic,
    /// A speaker.
    Speaker,
    /// A speaker, struck through.
    Muted,
    Plus,
    Trash,
    Gear,
    ChevronDown,
    ChevronRight,
    Warning,
    Error,
    Check,
    /// A circular arrow: restart.
    Restart,
    Folder,
    /// A sheet with a folded corner: a project file.
    File,
    Close,
}

impl Icon {
    /// Draw the icon centred in `rect` (scaled from the 16 px grid).
    pub fn paint(self, painter: &Painter, rect: Rect, color: Color32) {
        let scale = rect.width().min(rect.height()) / 16.0;
        let origin = rect.center() - Vec2::splat(8.0 * scale);
        let at = |x: f32, y: f32| origin + Vec2::new(x, y) * scale;
        let stroke = Stroke::new(1.5 * scale, color);
        let line = |points: &[(f32, f32)]| {
            painter.add(Shape::Path(PathShape::line(
                points.iter().map(|&(x, y)| at(x, y)).collect(),
                stroke,
            )));
        };
        let closed = |points: &[(f32, f32)]| {
            painter.add(Shape::Path(PathShape::closed_line(
                points.iter().map(|&(x, y)| at(x, y)).collect(),
                stroke,
            )));
        };
        let filled = |points: &[(f32, f32)]| {
            painter.add(Shape::convex_polygon(
                points.iter().map(|&(x, y)| at(x, y)).collect(),
                color,
                Stroke::NONE,
            ));
        };
        let rect_at =
            |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(at(x0, y0), at(x1, y1));
        let arc = |cx: f32, cy: f32, r: f32, from_deg: f32, to_deg: f32| {
            let steps = 16;
            let points: Vec<Pos2> = (0..=steps)
                .map(|i| {
                    let t = (from_deg + (to_deg - from_deg) * i as f32 / steps as f32).to_radians();
                    at(cx + r * t.cos(), cy + r * t.sin())
                })
                .collect();
            painter.add(Shape::Path(PathShape::line(points, stroke)));
        };
        match self {
            Self::Record => {
                painter.circle_filled(at(8.0, 8.0), 4.5 * scale, color);
            }
            Self::Live => {
                painter.circle_filled(at(8.0, 8.0), 2.0 * scale, color);
                arc(8.0, 8.0, 4.5, 140.0, 220.0);
                arc(8.0, 8.0, 4.5, -40.0, 40.0);
                arc(8.0, 8.0, 7.0, 145.0, 215.0);
                arc(8.0, 8.0, 7.0, -35.0, 35.0);
            }
            Self::Stop => {
                painter.rect_filled(rect_at(4.0, 4.0, 12.0, 12.0), 1.0 * scale, color);
            }
            Self::Preview | Self::Hidden => {
                // Two lids, arcs of radius 7 centred 5 below and above the
                // middle line; they meet at (8 ± 4.9, 8).
                arc(8.0, 13.0, 7.0, 225.6, 314.4);
                arc(8.0, 3.0, 7.0, 45.6, 134.4);
                painter.circle_stroke(at(8.0, 8.0), 2.2 * scale, stroke);
                if self == Self::Hidden {
                    line(&[(3.0, 13.0), (13.0, 3.0)]);
                }
            }
            Self::Scene => {
                closed(&[(2.0, 5.0), (12.0, 5.0), (12.0, 13.0), (2.0, 13.0)]);
                line(&[(4.5, 3.0), (14.0, 3.0), (14.0, 10.5)]);
            }
            Self::Layers => {
                closed(&[(8.0, 2.5), (14.0, 5.5), (8.0, 8.5), (2.0, 5.5)]);
                line(&[(2.0, 8.5), (8.0, 11.5), (14.0, 8.5)]);
                line(&[(2.0, 11.0), (8.0, 14.0), (14.0, 11.0)]);
            }
            Self::Screen => {
                closed(&[(2.0, 3.0), (14.0, 3.0), (14.0, 11.0), (2.0, 11.0)]);
                line(&[(5.5, 14.0), (10.5, 14.0)]);
                line(&[(8.0, 11.0), (8.0, 14.0)]);
            }
            Self::Solid => {
                // A swatch: an outlined square, half filled on the diagonal.
                closed(&[(3.0, 3.0), (13.0, 3.0), (13.0, 13.0), (3.0, 13.0)]);
                filled(&[(13.0, 3.0), (13.0, 13.0), (3.0, 13.0)]);
            }
            Self::Bars => {
                for x in [3.5, 6.5, 9.5, 12.5] {
                    line(&[(x, 3.0), (x, 13.0)]);
                }
            }
            Self::Tone => {
                let points: Vec<(f32, f32)> = (0..=24)
                    .map(|i| {
                        let x = 2.0 + 12.0 * i as f32 / 24.0;
                        let y = 8.0 - 4.0 * (std::f32::consts::TAU * (x - 2.0) / 12.0).sin();
                        (x, y)
                    })
                    .collect();
                line(&points);
            }
            Self::Mic => {
                closed(&[(6.0, 2.5), (10.0, 2.5), (10.0, 8.5), (6.0, 8.5)]);
                arc(8.0, 7.5, 4.0, 0.0, 180.0);
                line(&[(8.0, 11.5), (8.0, 14.0)]);
            }
            Self::Speaker | Self::Muted => {
                closed(&[
                    (2.5, 6.0),
                    (5.0, 6.0),
                    (8.5, 3.0),
                    (8.5, 13.0),
                    (5.0, 10.0),
                    (2.5, 10.0),
                ]);
                if self == Self::Speaker {
                    arc(8.5, 8.0, 3.0, -50.0, 50.0);
                    arc(8.5, 8.0, 5.5, -50.0, 50.0);
                } else {
                    line(&[(11.0, 6.0), (14.5, 10.0)]);
                    line(&[(14.5, 6.0), (11.0, 10.0)]);
                }
            }
            Self::Plus => {
                line(&[(8.0, 3.0), (8.0, 13.0)]);
                line(&[(3.0, 8.0), (13.0, 8.0)]);
            }
            Self::Trash => {
                line(&[(2.5, 4.5), (13.5, 4.5)]);
                line(&[(6.0, 4.5), (6.0, 2.5), (10.0, 2.5), (10.0, 4.5)]);
                line(&[(4.0, 4.5), (4.8, 13.5), (11.2, 13.5), (12.0, 4.5)]);
            }
            Self::Gear => {
                painter.circle_stroke(at(8.0, 8.0), 2.2 * scale, stroke);
                for i in 0..8 {
                    let t = (i as f32 * 45.0).to_radians();
                    let (s, c) = t.sin_cos();
                    line(&[
                        (8.0 + 4.3 * c, 8.0 + 4.3 * s),
                        (8.0 + 6.3 * c, 8.0 + 6.3 * s),
                    ]);
                }
                painter.circle_stroke(at(8.0, 8.0), 4.3 * scale, stroke);
            }
            Self::ChevronDown => line(&[(4.0, 6.0), (8.0, 10.0), (12.0, 6.0)]),
            Self::ChevronRight => line(&[(6.0, 4.0), (10.0, 8.0), (6.0, 12.0)]),
            Self::Warning => {
                closed(&[(8.0, 2.5), (14.0, 13.0), (2.0, 13.0)]);
                line(&[(8.0, 6.5), (8.0, 9.5)]);
                painter.circle_filled(at(8.0, 11.3), 0.9 * scale, color);
            }
            Self::Error => {
                painter.circle_stroke(at(8.0, 8.0), 6.0 * scale, stroke);
                line(&[(8.0, 4.8), (8.0, 8.8)]);
                painter.circle_filled(at(8.0, 11.0), 0.9 * scale, color);
            }
            Self::Check => line(&[(3.0, 8.5), (6.5, 12.0), (13.0, 4.5)]),
            Self::Restart => {
                arc(8.0, 8.0, 5.0, -60.0, 230.0);
                filled(&[(10.6, 1.8), (13.8, 4.8), (9.4, 5.9)]);
            }
            Self::Folder => {
                closed(&[
                    (2.0, 4.0),
                    (6.5, 4.0),
                    (8.0, 5.5),
                    (14.0, 5.5),
                    (14.0, 13.0),
                    (2.0, 13.0),
                ]);
            }
            Self::File => {
                closed(&[
                    (3.5, 2.0),
                    (9.5, 2.0),
                    (12.5, 5.0),
                    (12.5, 14.0),
                    (3.5, 14.0),
                ]);
                line(&[(9.5, 2.0), (9.5, 5.0), (12.5, 5.0)]);
            }
            Self::Close => {
                line(&[(4.0, 4.0), (12.0, 12.0)]);
                line(&[(12.0, 4.0), (4.0, 12.0)]);
            }
        }
    }

    /// Every icon, for the specimen render.
    pub const ALL: [Icon; 26] = [
        Self::Record,
        Self::Live,
        Self::Stop,
        Self::Preview,
        Self::Hidden,
        Self::Scene,
        Self::Layers,
        Self::Screen,
        Self::Solid,
        Self::Bars,
        Self::Tone,
        Self::Mic,
        Self::Speaker,
        Self::Muted,
        Self::Plus,
        Self::Trash,
        Self::Gear,
        Self::ChevronDown,
        Self::ChevronRight,
        Self::Warning,
        Self::Error,
        Self::Check,
        Self::Restart,
        Self::Folder,
        Self::File,
        Self::Close,
    ];
}

/// The square an icon occupies at `size`, centred at `center`.
pub fn icon_rect(center: Pos2, size: f32) -> Rect {
    Rect::from_center_size(center, Vec2::splat(size))
}
