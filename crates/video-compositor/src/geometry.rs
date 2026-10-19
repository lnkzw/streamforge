//! Layer placement math.
//!
//! Coordinates are canvas pixels with the origin at the top-left corner. A
//! canvas pixel `(px, py)` belongs to a layer when its center
//! `(px + 0.5, py + 0.5)` lies inside the layer rectangle; it samples the
//! source at the matching center-aligned source coordinate.

use serde::Serialize;

/// Axis-aligned rectangle in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Placement of a source on the canvas: top-left position and scale.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Transform {
    pub x: f32,
    pub y: f32,
    pub scale_x: f32,
    pub scale_y: f32,
}

impl Transform {
    pub const IDENTITY: Self = Self {
        x: 0.0,
        y: 0.0,
        scale_x: 1.0,
        scale_y: 1.0,
    };

    pub fn at(x: f32, y: f32) -> Self {
        Self {
            x,
            y,
            ..Self::IDENTITY
        }
    }

    /// Uniform scale that fits a `width` x `height` source inside `area`,
    /// centered, preserving the aspect ratio.
    pub fn fit(width: u32, height: u32, area: Rect) -> Self {
        let scale = (area.width / width as f32).min(area.height / height as f32);
        Self {
            x: area.x + (area.width - width as f32 * scale) / 2.0,
            y: area.y + (area.height - height as f32 * scale) / 2.0,
            scale_x: scale,
            scale_y: scale,
        }
    }

    /// Scale exactly 1 at an integer position: pixels map 1:1.
    pub fn is_pixel_aligned(&self) -> bool {
        self.scale_x == 1.0 && self.scale_y == 1.0 && self.x.fract() == 0.0 && self.y.fract() == 0.0
    }

    /// Canvas rectangle covered by a `width` x `height` source.
    pub fn rect(&self, width: u32, height: u32) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            width: width as f32 * self.scale_x,
            height: height as f32 * self.scale_y,
        }
    }
}

/// Half-open integer pixel range `[x0, x1) x [y0, y1)` on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelBounds {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl PixelBounds {
    pub fn area(&self) -> u64 {
        u64::from(self.x1 - self.x0) * u64::from(self.y1 - self.y0)
    }
}

/// Canvas pixels whose centers lie inside `rect`, clipped to the canvas.
pub fn pixel_bounds(rect: Rect, canvas_width: u32, canvas_height: u32) -> Option<PixelBounds> {
    let span = |start: f32, length: f32, limit: u32| -> Option<(u32, u32)> {
        let first = (start - 0.5).ceil().max(0.0);
        let end = (start + length - 0.5).ceil().min(limit as f32);
        (end > first).then_some((first as u32, end as u32))
    };
    let (x0, x1) = span(rect.x, rect.width, canvas_width)?;
    let (y0, y1) = span(rect.y, rect.height, canvas_height)?;
    Some(PixelBounds { x0, y0, x1, y1 })
}

/// Source coordinate (in source pixels, center-aligned) sampled by canvas
/// pixel `index` along one axis.
pub fn source_coordinate(index: u32, origin: f32, scale: f32) -> f32 {
    (index as f32 + 0.5 - origin) / scale - 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_covers_source_exactly() {
        let bounds = pixel_bounds(Transform::IDENTITY.rect(4, 3), 10, 10).expect("visible");
        assert_eq!(
            bounds,
            PixelBounds {
                x0: 0,
                y0: 0,
                x1: 4,
                y1: 3
            }
        );
        assert_eq!(bounds.area(), 12);
    }

    #[test]
    fn bounds_are_clipped_to_the_canvas() {
        let bounds = pixel_bounds(Transform::at(-2.0, 8.0).rect(4, 4), 10, 10).expect("visible");
        assert_eq!(
            bounds,
            PixelBounds {
                x0: 0,
                y0: 8,
                x1: 2,
                y1: 10
            }
        );
    }

    #[test]
    fn offscreen_layer_has_no_bounds() {
        assert_eq!(
            pixel_bounds(Transform::at(10.0, 0.0).rect(4, 4), 10, 10),
            None
        );
        assert_eq!(
            pixel_bounds(Transform::at(-4.0, 0.0).rect(4, 4), 10, 10),
            None
        );
    }

    #[test]
    fn half_pixel_offsets_follow_pixel_centers() {
        // Covers [0.25, 2.25): centers 0.5, 1.5 are inside, 2.5 is not.
        let bounds = pixel_bounds(Transform::at(0.25, 0.0).rect(2, 1), 10, 10).expect("visible");
        assert_eq!((bounds.x0, bounds.x1), (0, 2));
    }

    #[test]
    fn scaling_maps_centers_to_source_coordinates() {
        // 2x upscale: canvas pixels 0..4 sample source -0.25, 0.25, 0.75, 1.25.
        let coords: Vec<f32> = (0..4).map(|i| source_coordinate(i, 0.0, 2.0)).collect();
        assert_eq!(coords, vec![-0.25, 0.25, 0.75, 1.25]);
        // Identity maps pixel centers to themselves.
        assert_eq!(source_coordinate(7, 3.0, 1.0), 4.0);
    }

    #[test]
    fn fit_preserves_aspect_and_centers() {
        let t = Transform::fit(
            2560,
            1440,
            Rect {
                x: 100.0,
                y: 100.0,
                width: 1000.0,
                height: 1000.0,
            },
        );
        assert_eq!(t.scale_x, t.scale_y);
        let rect = t.rect(2560, 1440);
        assert!((rect.width - 1000.0).abs() < 1e-3);
        assert!((rect.height - 562.5).abs() < 1e-3);
        assert!((rect.y - (100.0 + (1000.0 - 562.5) / 2.0)).abs() < 1e-3);
    }

    #[test]
    fn pixel_alignment_detection() {
        assert!(Transform::at(3.0, 4.0).is_pixel_aligned());
        assert!(!Transform::at(3.5, 4.0).is_pixel_aligned());
        assert!(
            !Transform {
                scale_x: 2.0,
                ..Transform::IDENTITY
            }
            .is_pixel_aligned()
        );
    }
}
