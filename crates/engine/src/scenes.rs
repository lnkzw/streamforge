//! The engine's scene collection and its translation into compositor input.
//!
//! Scenes are engine state changed only through commands. The pipeline
//! renders the active scene; [`build`] turns it into a compositor scene and
//! sources (static frames are cached so unchanged sources keep their state
//! across live updates).

use std::collections::HashMap;
use std::sync::Arc;

use command_api::{
    AddSourceRequest, CommandError, ErrorCode, MAX_SCENE_SOURCES, MAX_SCENES, Rgba, SceneList,
    SceneSpec, SourceKind, SourceRect, SourceSpec, UpdateSourceRequest,
};
use media_core::{FrameMailbox, VideoFrame};
use serde_json::json;
use video_compositor::{
    Animation, Layer, Rect, Scene, SceneUpdate, SourceConfig, SourceId, SourceInput, Transform,
    pattern,
};

use crate::composite::{CANVAS_HEIGHT, CANVAS_WIDTH, SCREEN_BOX, overlay_x};

/// The built-in scene (the M3 reference scene).
pub(crate) const DEFAULT_SCENE: &str = "Test pattern";
/// Ids of the built-in sources. The overlay moves (as in the reference
/// scene) while it keeps its built-in rectangle; ids are never reused, so
/// user sources are never animated.
const BUILTIN_BARS: u32 = 1;
const BUILTIN_OVERLAY: u32 = 2;
const BUILTIN_OVERLAY_RECT: SourceRect = SourceRect {
    x: 64.0,
    y: 120.0,
    width: 160.0,
    height: 160.0,
};
/// Compositor id of the legacy `screen: true` output layer.
const EXTRA_SCREEN: SourceId = SourceId(u32::MAX);
/// Solid colors are tiny frames stretched over their rectangle.
const SOLID_SIZE: u32 = 8;

pub(crate) struct SceneCollection {
    scenes: Vec<SceneSpec>,
    active: String,
    next_id: u32,
}

impl SceneCollection {
    pub fn new() -> Self {
        let builtin = |id, name: &str, kind, rect| SourceSpec {
            id,
            name: name.to_owned(),
            kind,
            rect,
            opacity: 1.0,
        };
        Self {
            scenes: vec![SceneSpec {
                name: DEFAULT_SCENE.to_owned(),
                sources: vec![
                    builtin(
                        BUILTIN_BARS,
                        "color_bars",
                        SourceKind::ColorBars,
                        full_canvas(),
                    ),
                    builtin(
                        BUILTIN_OVERLAY,
                        "overlay",
                        // 60 % gray at 60 % alpha, as in the reference scene.
                        SourceKind::Solid {
                            color: Rgba([153, 153, 153, 153]),
                        },
                        BUILTIN_OVERLAY_RECT,
                    ),
                ],
            }],
            active: DEFAULT_SCENE.to_owned(),
            next_id: BUILTIN_OVERLAY + 1,
        }
    }

    fn allocate_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    pub fn list(&self) -> SceneList {
        SceneList {
            canvas_width: CANVAS_WIDTH,
            canvas_height: CANVAS_HEIGHT,
            active: self.active.clone(),
            scenes: self.scenes.clone(),
        }
    }

    /// Ids of the screen sources in any scene (their captures may keep
    /// running while another scene is shown).
    pub fn screen_source_ids(&self) -> Vec<u32> {
        self.scenes
            .iter()
            .flat_map(|scene| &scene.sources)
            .filter(|source| matches!(source.kind, SourceKind::Screen { .. }))
            .map(|source| source.id)
            .collect()
    }

    pub fn active(&self) -> &SceneSpec {
        self.scenes
            .iter()
            .find(|scene| scene.name == self.active)
            .unwrap_or(&self.scenes[0])
    }

    pub fn get(&self, name: &str) -> Result<&SceneSpec, CommandError> {
        self.scenes
            .iter()
            .find(|scene| scene.name == name)
            .ok_or_else(|| not_found("scene", name))
    }

    fn get_mut(&mut self, name: &str) -> Result<&mut SceneSpec, CommandError> {
        self.scenes
            .iter_mut()
            .find(|scene| scene.name == name)
            .ok_or_else(|| not_found("scene", name))
    }

    pub fn create(&mut self, name: &str) -> Result<(), CommandError> {
        if self.scenes.iter().any(|scene| scene.name == name) {
            return Err(CommandError::new(
                ErrorCode::InvalidParams,
                format!("a scene named `{name}` already exists"),
            ));
        }
        if self.scenes.len() >= MAX_SCENES {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                format!("at most {MAX_SCENES} scenes; delete one first"),
            ));
        }
        self.scenes.push(SceneSpec {
            name: name.to_owned(),
            sources: Vec::new(),
        });
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<(), CommandError> {
        self.get(name)?;
        if name == self.active {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                format!("`{name}` is the active scene; activate another one first"),
            ));
        }
        self.scenes.retain(|scene| scene.name != name);
        Ok(())
    }

    pub fn set_active(&mut self, name: &str) -> Result<(), CommandError> {
        self.get(name)?;
        name.clone_into(&mut self.active);
        Ok(())
    }

    pub fn add_source(&mut self, request: &AddSourceRequest) -> Result<(), CommandError> {
        let id = self.next_id;
        let scene = self.get_mut(&request.scene)?;
        if scene
            .sources
            .iter()
            .any(|source| source.name == request.name)
        {
            return Err(CommandError::new(
                ErrorCode::InvalidParams,
                format!(
                    "scene `{}` already has a source named `{}`",
                    request.scene, request.name
                ),
            ));
        }
        if scene.sources.len() >= MAX_SCENE_SOURCES {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                format!("at most {MAX_SCENE_SOURCES} sources per scene"),
            ));
        }
        scene.sources.push(SourceSpec {
            id,
            name: request.name.clone(),
            kind: request.kind.clone(),
            rect: request.rect.unwrap_or_else(full_canvas),
            opacity: request.opacity,
        });
        self.allocate_id();
        Ok(())
    }

    pub fn update_source(&mut self, request: &UpdateSourceRequest) -> Result<(), CommandError> {
        let scene = self.get_mut(&request.scene)?;
        let source = scene
            .sources
            .iter_mut()
            .find(|source| source.name == request.name)
            .ok_or_else(|| not_found("source", &request.name))?;
        let rect = &mut source.rect;
        rect.x = request.x.unwrap_or(rect.x);
        rect.y = request.y.unwrap_or(rect.y);
        rect.width = request.width.unwrap_or(rect.width);
        rect.height = request.height.unwrap_or(rect.height);
        source.opacity = request.opacity.unwrap_or(source.opacity);
        Ok(())
    }

    pub fn source(&self, scene: &str, name: &str) -> Result<&SourceSpec, CommandError> {
        self.get(scene)?
            .sources
            .iter()
            .find(|source| source.name == name)
            .ok_or_else(|| not_found("source", name))
    }

    pub fn remove_source(&mut self, scene: &str, name: &str) -> Result<(), CommandError> {
        let scene = self.get_mut(scene)?;
        let before = scene.sources.len();
        scene.sources.retain(|source| source.name != name);
        if scene.sources.len() == before {
            return Err(not_found("source", name));
        }
        Ok(())
    }

    /// Snapshot for undoing a change that the running pipeline rejected.
    pub fn snapshot(&self) -> (Vec<SceneSpec>, String) {
        (self.scenes.clone(), self.active.clone())
    }

    pub fn restore(&mut self, (scenes, active): (Vec<SceneSpec>, String)) {
        self.scenes = scenes;
        self.active = active;
    }
}

fn full_canvas() -> SourceRect {
    SourceRect {
        x: 0.0,
        y: 0.0,
        width: CANVAS_WIDTH as f32,
        height: CANVAS_HEIGHT as f32,
    }
}

fn not_found(what: &str, name: &str) -> CommandError {
    CommandError::new(ErrorCode::NotFound, format!("no {what} named `{name}`"))
        .with_details(json!({ what: name }))
}

/// Static frames by content, so rebuilding a scene reuses the same frames
/// and the compositor keeps those sources' state.
#[derive(Default)]
pub(crate) struct FrameCache {
    bars: Option<Arc<VideoFrame>>,
    solids: HashMap<[u8; 4], Arc<VideoFrame>>,
}

impl FrameCache {
    fn bars(&mut self) -> Result<Arc<VideoFrame>, CommandError> {
        if let Some(bars) = &self.bars {
            return Ok(Arc::clone(bars));
        }
        let bars = Arc::new(pattern::color_bars(CANVAS_WIDTH, CANVAS_HEIGHT).map_err(internal)?);
        self.bars = Some(Arc::clone(&bars));
        Ok(bars)
    }

    fn solid(&mut self, Rgba([r, g, b, a]): Rgba) -> Result<Arc<VideoFrame>, CommandError> {
        if let Some(frame) = self.solids.get(&[r, g, b, a]) {
            return Ok(Arc::clone(frame));
        }
        // Frames are premultiplied BGRA.
        let premultiply = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
        let frame = Arc::new(
            pattern::solid(
                SOLID_SIZE,
                SOLID_SIZE,
                [premultiply(b), premultiply(g), premultiply(r), a],
            )
            .map_err(internal)?,
        );
        self.solids.insert([r, g, b, a], Arc::clone(&frame));
        Ok(frame)
    }
}

/// Live inputs the build can use: screen captures by source id, and the
/// legacy output screen layer.
pub(crate) struct LiveFrames<'a> {
    pub screens: &'a [(u32, Arc<FrameMailbox>)],
    pub extra_screen: Option<&'a Arc<FrameMailbox>>,
}

/// Turn `scene` into compositor input. Screen sources without a running
/// capture are left out.
pub(crate) fn build(
    scene: &SceneSpec,
    live: &LiveFrames<'_>,
    cache: &mut FrameCache,
) -> Result<SceneUpdate, CommandError> {
    let mut canvas = Scene::new(CANVAS_WIDTH, CANVAS_HEIGHT);
    let mut sources = Vec::with_capacity(scene.sources.len() + 1);
    for spec in &scene.sources {
        let id = SourceId(spec.id);
        let rect = spec.rect;
        let (layer, input) = match &spec.kind {
            SourceKind::ColorBars => (
                stretched(spec, CANVAS_WIDTH, CANVAS_HEIGHT),
                SourceInput::Static(cache.bars()?),
            ),
            SourceKind::Solid { color } => (
                stretched(spec, SOLID_SIZE, SOLID_SIZE),
                SourceInput::Static(cache.solid(*color)?),
            ),
            SourceKind::Screen { .. } => {
                let Some((_, mailbox)) = live.screens.iter().find(|(source, _)| *source == spec.id)
                else {
                    continue;
                };
                let area = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                };
                (
                    Layer::fitted(spec.name.clone(), id, area),
                    SourceInput::Live(Arc::clone(mailbox)),
                )
            }
        };
        canvas.layers.push(layer.with_opacity(spec.opacity));
        sources.push(SourceConfig {
            id,
            name: spec.name.clone(),
            input,
        });
    }
    if let Some(mailbox) = live.extra_screen {
        // Above the bottom source, as in the reference scene (bars, screen,
        // overlay).
        let at = canvas.layers.len().min(1);
        canvas.layers.insert(
            at,
            Layer::fitted("screen", EXTRA_SCREEN, SCREEN_BOX).with_opacity(0.9),
        );
        sources.push(SourceConfig {
            id: EXTRA_SCREEN,
            name: "screen".to_owned(),
            input: SourceInput::Live(Arc::clone(mailbox)),
        });
    }
    let animated = scene
        .sources
        .iter()
        .any(|source| source.id == BUILTIN_OVERLAY && source.rect == BUILTIN_OVERLAY_RECT);
    let animation: Option<Animation> = animated.then_some(move_builtin_overlay);
    Ok(SceneUpdate {
        scene: canvas,
        sources,
        animation,
    })
}

/// Sweep the built-in overlay across the canvas (as the reference scene).
fn move_builtin_overlay(scene: &mut Scene, tick: u64) {
    if let Some(layer) = scene
        .layers
        .iter_mut()
        .find(|layer| layer.source == SourceId(BUILTIN_OVERLAY))
    {
        layer.transform.x = overlay_x(tick);
    }
}

fn stretched(spec: &SourceSpec, width: u32, height: u32) -> Layer {
    Layer::new(
        spec.name.clone(),
        SourceId(spec.id),
        Transform {
            x: spec.rect.x,
            y: spec.rect.y,
            scale_x: spec.rect.width / width as f32,
            scale_y: spec.rect.height / height as f32,
        },
    )
}

fn internal(error: impl std::fmt::Display) -> CommandError {
    CommandError::new(ErrorCode::Internal, format!("scene build failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(scene: &str, name: &str, kind: SourceKind) -> AddSourceRequest {
        AddSourceRequest {
            scene: scene.to_owned(),
            name: name.to_owned(),
            kind,
            rect: None,
            opacity: 1.0,
        }
    }

    #[test]
    fn default_scene_matches_the_reference_scene() {
        let collection = SceneCollection::new();
        let list = collection.list();
        assert_eq!(list.active, DEFAULT_SCENE);
        let names: Vec<_> = list.scenes[0].sources.iter().map(|s| &s.name).collect();
        assert_eq!(names, ["color_bars", "overlay"]);
        let update = build(
            collection.active(),
            &LiveFrames {
                screens: &[],
                extra_screen: None,
            },
            &mut FrameCache::default(),
        )
        .expect("build");
        assert!(update.animation.is_some());
        assert_eq!(update.scene.layers.len(), 2);
        update.scene.validate().expect("valid");
    }

    #[test]
    fn only_the_builtin_overlay_in_place_is_animated() {
        let mut collection = SceneCollection::new();
        let none = LiveFrames {
            screens: &[],
            extra_screen: None,
        };
        let mut cache = FrameCache::default();
        collection
            .update_source(&UpdateSourceRequest {
                scene: DEFAULT_SCENE.to_owned(),
                name: "overlay".to_owned(),
                x: Some(500.0),
                y: None,
                width: None,
                height: None,
                opacity: None,
            })
            .expect("move");
        // Moved by the user: it stays where it was put.
        let moved = build(collection.active(), &none, &mut cache).expect("build");
        assert!(moved.animation.is_none());
        // A user source named `overlay` in a new scene is not animated.
        collection.create("Other").expect("create");
        collection
            .add_source(&AddSourceRequest {
                scene: "Other".to_owned(),
                name: "overlay".to_owned(),
                kind: SourceKind::ColorBars,
                rect: Some(BUILTIN_OVERLAY_RECT),
                opacity: 1.0,
            })
            .expect("add");
        let other =
            build(collection.get("Other").expect("scene"), &none, &mut cache).expect("build");
        assert!(other.animation.is_none());
    }

    #[test]
    fn the_legacy_screen_layer_sits_above_the_bottom_source() {
        let collection = SceneCollection::new();
        let mailbox = Arc::new(FrameMailbox::new());
        let update = build(
            collection.active(),
            &LiveFrames {
                screens: &[],
                extra_screen: Some(&mailbox),
            },
            &mut FrameCache::default(),
        )
        .expect("build");
        let names: Vec<_> = update
            .scene
            .layers
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(names, ["color_bars", "screen", "overlay"]);
    }

    #[test]
    fn edits_keep_names_unique_and_protect_the_active_scene() {
        let mut collection = SceneCollection::new();
        collection.create("Show").expect("create");
        assert_eq!(
            collection.create("Show").unwrap_err().code,
            ErrorCode::InvalidParams
        );
        collection
            .add_source(&source(
                "Show",
                "red",
                SourceKind::Solid {
                    color: Rgba([255, 0, 0, 255]),
                },
            ))
            .expect("add");
        let duplicate = collection
            .add_source(&source("Show", "red", SourceKind::ColorBars))
            .unwrap_err();
        assert_eq!(duplicate.code, ErrorCode::InvalidParams);
        assert_eq!(
            collection
                .add_source(&source("Nope", "x", SourceKind::ColorBars))
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        collection.set_active("Show").expect("activate");
        assert_eq!(
            collection.delete("Show").unwrap_err().code,
            ErrorCode::InvalidState
        );
        collection.set_active(DEFAULT_SCENE).expect("activate");
        collection.delete("Show").expect("delete");
        assert!(collection.get("Show").is_err());
    }

    #[test]
    fn built_scenes_reuse_static_frames_and_skip_missing_captures() {
        let mut collection = SceneCollection::new();
        collection.create("Show").expect("create");
        for (name, kind) in [
            ("bars", SourceKind::ColorBars),
            (
                "screen",
                SourceKind::Screen {
                    source_types: vec![capture_api::VideoSourceKind::Monitor],
                    cursor: capture_api::CursorMode::Embedded,
                    source_id: None,
                },
            ),
        ] {
            collection
                .add_source(&source("Show", name, kind))
                .expect("add");
        }
        let scene = collection.get("Show").expect("scene").clone();
        let mut cache = FrameCache::default();
        let none = LiveFrames {
            screens: &[],
            extra_screen: None,
        };
        let first = build(&scene, &none, &mut cache).expect("build");
        // Without a running capture the screen source is left out.
        assert_eq!(first.sources.len(), 1);
        let second = build(&scene, &none, &mut cache).expect("build");
        let (SourceInput::Static(a), SourceInput::Static(b)) =
            (&first.sources[0].input, &second.sources[0].input)
        else {
            panic!("static bars");
        };
        assert!(Arc::ptr_eq(a, b));
        let screen_id = scene.sources[1].id;
        let mailbox = Arc::new(FrameMailbox::new());
        let with_capture = build(
            &scene,
            &LiveFrames {
                screens: &[(screen_id, Arc::clone(&mailbox))],
                extra_screen: None,
            },
            &mut cache,
        )
        .expect("build");
        assert_eq!(with_capture.sources.len(), 2);
        assert!(with_capture.animation.is_none());
    }
}
