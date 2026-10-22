//! Types of the scene, source, mixer and preview commands.
//!
//! The engine keeps a collection of scenes; the media pipeline renders the
//! active one. Edits to the active scene apply live while the pipeline runs.
//!
//! Scenes and sources have stable ids (ADR 0014): opaque strings with a
//! type prefix, assigned by the engine and never reused. Commands name a
//! scene or source by id or by name.

use std::fmt;

use capture_api::{CursorMode, VideoSourceKind};
use serde::Serialize;

use crate::{LiveInputSpec, ToneSpec};

/// Most scenes the engine keeps.
pub const MAX_SCENES: usize = 16;
/// Most sources per scene.
pub const MAX_SCENE_SOURCES: usize = 16;
/// Longest scene or source name, in characters.
pub const MAX_NAME_CHARS: usize = 64;
/// Scene ids start with this; the rest is opaque.
pub const SCENE_ID_PREFIX: &str = "scene_";
/// Source ids start with this; the rest is opaque.
pub const SOURCE_ID_PREFIX: &str = "src_";
/// Longest scene or source id, in bytes (`a-z`, `0-9` and `_`).
pub const MAX_ID_LEN: usize = 64;

/// How a command names a scene: by id, by name, or by both, which must then
/// name the same scene. The id is looked up first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SceneRef {
    pub id: Option<String>,
    pub name: Option<String>,
}

impl SceneRef {
    pub fn id(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            name: None,
        }
    }

    pub fn name(name: impl Into<String>) -> Self {
        Self {
            id: None,
            name: Some(name.into()),
        }
    }
}

impl fmt::Display for SceneRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.id, &self.name) {
            (Some(id), _) => write!(f, "`{id}`"),
            (None, Some(name)) => write!(f, "`{name}`"),
            (None, None) => f.write_str("(no scene given)"),
        }
    }
}

/// How a command names a source: by id (unique across all scenes), or by
/// its scene and name. With an id, a scene or name given as well must
/// match it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceRef {
    pub id: Option<String>,
    pub scene: Option<SceneRef>,
    pub name: Option<String>,
}

impl SourceRef {
    pub fn id(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            ..Self::default()
        }
    }

    /// The source `name` in the scene named `scene`.
    pub fn named(scene: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: None,
            scene: Some(SceneRef::name(scene)),
            name: Some(name.into()),
        }
    }
}

/// What a source shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceKind {
    /// SMPTE-like color bars, stretched to the source rectangle.
    ColorBars,
    /// A flat color (`#rrggbbaa`, straight alpha).
    Solid { color: Rgba },
    /// Live screen capture, fitted into the rectangle with its aspect
    /// ratio kept: the named source, or the one the user picks.
    Screen {
        source_types: Vec<VideoSourceKind>,
        cursor: CursorMode,
        /// From `list_screen_sources`; `None` opens the system picker.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_id: Option<String>,
    },
}

/// A color as red, green, blue, alpha; serialized as `#rrggbbaa`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba(pub [u8; 4]);

impl Serialize for Rgba {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let [r, g, b, a] = self.0;
        serializer.serialize_str(&format!("#{r:02x}{g:02x}{b:02x}{a:02x}"))
    }
}

/// Placement on the canvas in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SourceRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceSpec {
    /// Stable id assigned by the engine (`src_…`), unique across scenes.
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub kind: SourceKind,
    #[serde(flatten)]
    pub rect: SourceRect,
    pub opacity: f32,
}

/// A scene; sources are drawn in order (later ones on top).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SceneSpec {
    /// Stable id assigned by the engine (`scene_…`).
    pub id: String,
    pub name: String,
    pub sources: Vec<SourceSpec>,
}

/// Result of every scene command: the whole collection after the change.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SceneList {
    pub canvas_width: u32,
    pub canvas_height: u32,
    /// Name of the active scene.
    pub active: String,
    pub active_id: String,
    pub scenes: Vec<SceneSpec>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AddSourceRequest {
    pub scene: SceneRef,
    pub name: String,
    pub kind: SourceKind,
    /// `None`: the whole canvas.
    pub rect: Option<SourceRect>,
    pub opacity: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateSourceRequest {
    pub source: SourceRef,
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub opacity: Option<f32>,
}

/// Audio of the pipeline: live inputs, test tones and the master gain.
/// Serialized as `set_audio_sources` parameters (a project file stores it
/// so).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioSetup {
    pub inputs: Vec<LiveInputSpec>,
    pub tones: Vec<ToneSpec>,
    pub master_gain_db: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StripChange {
    pub index: usize,
    pub gain_db: Option<f32>,
    pub muted: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StripState {
    pub index: usize,
    pub name: String,
    /// `live` or `tone`.
    pub kind: &'static str,
    pub gain_db: f32,
    pub muted: bool,
    /// Peak since the previous `get_mixer` (dBFS; `null`: silence or not
    /// running).
    pub peak_dbfs: Option<f64>,
}

/// Result of the mixer commands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MixerState {
    /// Whether the pipeline (and so the mixer) runs; levels need it.
    pub running: bool,
    pub strips: Vec<StripState>,
    pub master_gain_db: f32,
    pub master_peak_dbfs: Option<f64>,
}

/// Result of `set_preview`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreviewState {
    pub enabled: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// When turning the preview off: frames published, and frames dropped
    /// because no buffer was free (the viewer held them).
    pub frames_published: Option<u64>,
    pub frames_dropped: Option<u64>,
    /// When turning the preview off stopped the pipeline: its report.
    pub pipeline: Option<crate::PipelineReport>,
    pub warnings: Vec<String>,
}
