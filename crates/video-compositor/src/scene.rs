//! Scene graph: an ordered list of layers on a fixed-size canvas.

use serde::Serialize;
use thiserror::Error;

use crate::geometry::Transform;

/// Identifies the frame source a layer draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct SourceId(pub u32);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Layer {
    pub name: String,
    pub source: SourceId,
    pub transform: Transform,
    /// Multiplier applied to the source alpha, `0.0..=1.0`.
    pub opacity: f32,
    pub visible: bool,
}

impl Layer {
    pub fn new(name: impl Into<String>, source: SourceId, transform: Transform) -> Self {
        Self {
            name: name.into(),
            source,
            transform,
            opacity: 1.0,
            visible: true,
        }
    }

    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }
}

/// Layers are drawn in order: later layers cover earlier ones.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Scene {
    pub width: u32,
    pub height: u32,
    /// Opaque background in BGRA byte order.
    pub background: [u8; 4],
    pub layers: Vec<Layer>,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum SceneError {
    #[error("canvas must be non-empty, got {width}x{height}")]
    EmptyCanvas { width: u32, height: u32 },
    #[error("layer `{layer}` has a non-positive or non-finite scale")]
    InvalidScale { layer: String },
    #[error("layer `{layer}` has a non-finite position")]
    InvalidPosition { layer: String },
    #[error("layer `{layer}` has opacity {opacity}, expected 0.0..=1.0")]
    InvalidOpacity { layer: String, opacity: f32 },
}

impl Scene {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            background: [0, 0, 0, 255],
            layers: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), SceneError> {
        if self.width == 0 || self.height == 0 {
            return Err(SceneError::EmptyCanvas {
                width: self.width,
                height: self.height,
            });
        }
        for layer in &self.layers {
            let t = layer.transform;
            let positive = |s: f32| s.is_finite() && s > 0.0;
            if !positive(t.scale_x) || !positive(t.scale_y) {
                return Err(SceneError::InvalidScale {
                    layer: layer.name.clone(),
                });
            }
            if !t.x.is_finite() || !t.y.is_finite() {
                return Err(SceneError::InvalidPosition {
                    layer: layer.name.clone(),
                });
            }
            if !(0.0..=1.0).contains(&layer.opacity) {
                return Err(SceneError::InvalidOpacity {
                    layer: layer.name.clone(),
                    opacity: layer.opacity,
                });
            }
        }
        Ok(())
    }

    pub fn layer_mut(&mut self, name: &str) -> Option<&mut Layer> {
        self.layers.iter_mut().find(|layer| layer.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_scene_passes() {
        let mut scene = Scene::new(1920, 1080);
        scene
            .layers
            .push(Layer::new("a", SourceId(0), Transform::IDENTITY).with_opacity(0.5));
        assert_eq!(scene.validate(), Ok(()));
    }

    #[test]
    fn invalid_layers_are_rejected_by_name() {
        let base = Layer::new("bad", SourceId(0), Transform::IDENTITY);
        let cases = [
            Layer {
                transform: Transform {
                    scale_x: 0.0,
                    ..Transform::IDENTITY
                },
                ..base.clone()
            },
            Layer {
                transform: Transform {
                    scale_y: f32::NAN,
                    ..Transform::IDENTITY
                },
                ..base.clone()
            },
            Layer {
                transform: Transform::at(f32::INFINITY, 0.0),
                ..base.clone()
            },
            base.clone().with_opacity(1.5),
        ];
        for layer in cases {
            let mut scene = Scene::new(10, 10);
            scene.layers.push(layer);
            let err = scene.validate().unwrap_err();
            assert!(err.to_string().contains("bad"), "{err}");
        }
        assert!(matches!(
            Scene::new(0, 10).validate(),
            Err(SceneError::EmptyCanvas { .. })
        ));
    }
}
