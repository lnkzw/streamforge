//! The engine's scene collection and its translation into compositor input.
//!
//! Scenes are engine state changed only through commands. The pipeline
//! renders the active scene; [`build`] turns it into a compositor scene and
//! sources (static frames are cached so unchanged sources keep their state
//! across live updates).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use command_api::{
    AddSourceRequest, CommandError, ErrorCode, MAX_SCENE_SOURCES, MAX_SCENES, MoveSourceRequest,
    Rgba, SCENE_ID_PREFIX, SOURCE_ID_PREFIX, SceneList, SceneRef, SceneSpec, SourceKind,
    SourceRect, SourceRef, SourceSpec, UpdateSourceRequest,
};
use media_core::{FrameMailbox, VideoFrame};

use crate::ids::new_id;
use crate::images::ImageStore;
use crate::refs::{self, Kind, mismatch, not_found, not_found_id};
use video_compositor::{
    Animation, Layer, Rect, Scene, SceneUpdate, SourceConfig, SourceId, SourceInput, Transform,
    pattern,
};

use crate::composite::{CANVAS_HEIGHT, CANVAS_WIDTH, overlay_x};

/// The built-in scene (the M3 reference scene).
pub(crate) const DEFAULT_SCENE: &str = "Test pattern";
/// Fixed ids of the built-in scene and sources, so a saved and loaded
/// collection keeps them. The overlay moves (as in the reference scene)
/// while it keeps its built-in rectangle; generated ids never look like
/// these, so user sources are never animated.
const DEFAULT_SCENE_ID: &str = "scene_test_pattern";
const BUILTIN_BARS: &str = "src_color_bars";
const BUILTIN_OVERLAY: &str = "src_overlay";
const BUILTIN_OVERLAY_RECT: SourceRect = SourceRect {
    x: 64.0,
    y: 120.0,
    width: 160.0,
    height: 160.0,
};
/// Solid colors are tiny frames stretched over their rectangle.
const SOLID_SIZE: u32 = 8;

pub(crate) struct SceneCollection {
    scenes: Vec<SceneSpec>,
    /// Id of the active scene.
    active: String,
}

impl SceneCollection {
    pub fn new() -> Self {
        let builtin = |id: &str, name: &str, kind, rect| SourceSpec {
            id: id.to_owned(),
            name: name.to_owned(),
            kind,
            rect,
            opacity: 1.0,
            visible: true,
        };
        Self {
            scenes: vec![SceneSpec {
                id: DEFAULT_SCENE_ID.to_owned(),
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
            active: DEFAULT_SCENE_ID.to_owned(),
        }
    }

    pub fn list(&self) -> SceneList {
        let active = self.active();
        SceneList {
            canvas_width: CANVAS_WIDTH,
            canvas_height: CANVAS_HEIGHT,
            active_scene_name: active.name.clone(),
            active_scene_id: active.id.clone(),
            scenes: self.scenes.clone(),
            warnings: Vec::new(),
        }
    }

    /// Ids of the screen and camera sources in any scene (their captures
    /// may keep running while another scene is shown).
    pub fn capture_source_ids(&self) -> Vec<String> {
        self.sources()
            .filter(|source| {
                matches!(
                    source.kind,
                    SourceKind::Screen { .. } | SourceKind::Camera { .. }
                )
            })
            .map(|source| source.id.clone())
            .collect()
    }

    /// The files image sources in any scene show.
    pub fn image_files(&self) -> HashSet<&str> {
        self.sources().filter_map(image_file).collect()
    }

    /// Names of the image sources showing `file`, in any scene.
    pub fn sources_showing(&self, file: &str) -> Vec<&str> {
        self.sources()
            .filter(|source| image_file(source) == Some(file))
            .map(|source| source.name.as_str())
            .collect()
    }

    fn sources(&self) -> impl Iterator<Item = &SourceSpec> {
        self.scenes.iter().flat_map(|scene| &scene.sources)
    }

    pub fn active(&self) -> &SceneSpec {
        self.scenes
            .iter()
            .find(|scene| scene.id == self.active)
            .unwrap_or(&self.scenes[0])
    }

    /// The scene `scene` names: by id first; a name given as well must be
    /// that scene's.
    fn scene_index(&self, scene: &SceneRef) -> Result<usize, CommandError> {
        refs::position(
            &self.scenes,
            scene.id.as_deref(),
            scene.name.as_deref(),
            &Kind {
                what: "scene",
                id_field: "scene_id",
                name_field: "scene_name",
            },
            |scene| (&scene.id, &scene.name),
        )
    }

    /// Where the source `source` names is: by id first (ids are unique
    /// across scenes); a scene or name given as well must match.
    fn source_index(&self, source: &SourceRef) -> Result<(usize, usize), CommandError> {
        if let Some(id) = &source.id {
            let (scene, index) = self
                .scenes
                .iter()
                .enumerate()
                .find_map(|(scene, spec)| {
                    let index = spec.sources.iter().position(|source| source.id == *id)?;
                    Some((scene, index))
                })
                .ok_or_else(|| not_found_id("source", "source_id", id))?;
            // A scene that does not exist does not hold it either: the
            // reference disagrees (`invalid_params`), the source exists.
            if let Some(wanted) = &source.scene
                && self.scene_index(wanted).ok() != Some(scene)
            {
                return Err(mismatch(format!(
                    "source `{id}` is in scene `{}`, not {wanted}",
                    self.scenes[scene].name
                )));
            }
            let found = &self.scenes[scene].sources[index].name;
            if let Some(name) = &source.name
                && name != found
            {
                return Err(mismatch(format!(
                    "source `{id}` is named `{found}`, not `{name}`"
                )));
            }
            return Ok((scene, index));
        }
        let (Some(scene), Some(name)) = (&source.scene, &source.name) else {
            return Err(mismatch(
                "no source given: pass `source_id`, or `scene_id`/`scene_name` and `source_name`",
            ));
        };
        let scene = self.scene_index(scene)?;
        let index = self.scenes[scene]
            .sources
            .iter()
            .position(|source| source.name == *name)
            .ok_or_else(|| not_found("source", "source_name", name))?;
        Ok((scene, index))
    }

    #[cfg(test)]
    pub fn get(&self, scene: &SceneRef) -> Result<&SceneSpec, CommandError> {
        Ok(&self.scenes[self.scene_index(scene)?])
    }

    /// Create an empty scene; returns its id.
    pub fn create(&mut self, name: &str) -> Result<String, CommandError> {
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
        let id = new_id(SCENE_ID_PREFIX, |id| {
            self.scenes.iter().any(|scene| scene.id == id)
        })?;
        self.scenes.push(SceneSpec {
            id: id.clone(),
            name: name.to_owned(),
            sources: Vec::new(),
        });
        Ok(id)
    }

    pub fn delete(&mut self, scene: &SceneRef) -> Result<(), CommandError> {
        let index = self.scene_index(scene)?;
        let scene = &self.scenes[index];
        if scene.id == self.active {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                format!(
                    "`{}` is the active scene; activate another one first",
                    scene.name
                ),
            ));
        }
        self.scenes.remove(index);
        Ok(())
    }

    pub fn set_active(&mut self, scene: &SceneRef) -> Result<(), CommandError> {
        let index = self.scene_index(scene)?;
        self.active.clone_from(&self.scenes[index].id);
        Ok(())
    }

    /// Add a source; returns its id.
    pub fn add_source(&mut self, request: &AddSourceRequest) -> Result<String, CommandError> {
        let index = self.scene_index(&request.scene)?;
        let scene = &self.scenes[index];
        if scene
            .sources
            .iter()
            .any(|source| source.name == request.name)
        {
            return Err(CommandError::new(
                ErrorCode::InvalidParams,
                format!(
                    "scene `{}` already has a source named `{}`",
                    scene.name, request.name
                ),
            ));
        }
        if scene.sources.len() >= MAX_SCENE_SOURCES {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                format!("at most {MAX_SCENE_SOURCES} sources per scene"),
            ));
        }
        let id = new_id(SOURCE_ID_PREFIX, |id| {
            self.sources().any(|source| source.id == id)
        })?;
        self.scenes[index].sources.push(SourceSpec {
            id: id.clone(),
            name: request.name.clone(),
            kind: request.kind.clone(),
            rect: request.rect.unwrap_or_else(full_canvas),
            opacity: request.opacity,
            visible: request.visible,
        });
        Ok(id)
    }

    pub fn update_source(&mut self, request: &UpdateSourceRequest) -> Result<(), CommandError> {
        let (scene, index) = self.source_index(&request.source)?;
        let source = &mut self.scenes[scene].sources[index];
        // Checked before anything changes.
        let image = matches!(source.kind, SourceKind::Image { .. });
        let camera = matches!(source.kind, SourceKind::Camera { .. });
        let refused = if request.file.is_some() && !image {
            Some("not an image source; only image sources have a `file`")
        } else if (request.camera_id.is_some() || request.mode.is_some()) && !camera {
            Some("not a camera source; only camera sources have a `camera_id` and a `mode`")
        } else {
            None
        };
        if let Some(reason) = refused {
            return Err(CommandError::new(
                ErrorCode::InvalidParams,
                format!("`{}` is {reason}", source.name),
            ));
        }
        match &mut source.kind {
            SourceKind::Image { file } => {
                if let Some(wanted) = &request.file {
                    file.clone_from(wanted);
                }
            }
            SourceKind::Camera { camera_id, mode } => {
                if let Some(id) = &request.camera_id {
                    camera_id.clone_from(id);
                }
                if let Some(wanted) = request.mode {
                    *mode = (!wanted.is_auto()).then_some(wanted);
                }
            }
            _ => {}
        }
        let rect = &mut source.rect;
        rect.x = request.x.unwrap_or(rect.x);
        rect.y = request.y.unwrap_or(rect.y);
        rect.width = request.width.unwrap_or(rect.width);
        rect.height = request.height.unwrap_or(rect.height);
        source.opacity = request.opacity.unwrap_or(source.opacity);
        source.visible = request.visible.unwrap_or(source.visible);
        Ok(())
    }

    /// Move a source to `index` in its scene's drawing order (0 is the
    /// bottom); the ones between shift. Its own place is no change.
    pub fn move_source(&mut self, request: &MoveSourceRequest) -> Result<(), CommandError> {
        let (scene, index) = self.source_index(&request.source)?;
        let scene = &mut self.scenes[scene];
        let top = scene.sources.len() - 1;
        if request.index > top {
            return Err(CommandError::new(
                ErrorCode::InvalidParams,
                format!(
                    "`index` {} is past the top of scene `{}`: its sources are 0 to {top}",
                    request.index, scene.name
                ),
            )
            .with_details(serde_json::json!({"index": request.index, "max_index": top})));
        }
        let source = scene.sources.remove(index);
        scene.sources.insert(request.index, source);
        Ok(())
    }

    pub fn source(&self, source: &SourceRef) -> Result<&SourceSpec, CommandError> {
        let (scene, index) = self.source_index(source)?;
        Ok(&self.scenes[scene].sources[index])
    }

    /// Capture screen source `source` from `screen` from now on.
    pub fn set_capture_target_id(
        &mut self,
        source: &SourceRef,
        screen: &str,
    ) -> Result<(), CommandError> {
        let (scene, index) = self.source_index(source)?;
        let spec = &mut self.scenes[scene].sources[index];
        match &mut spec.kind {
            SourceKind::Screen {
                capture_target_id, ..
            } => {
                *capture_target_id = Some(screen.to_owned());
                Ok(())
            }
            _ => Err(CommandError::new(
                ErrorCode::InvalidParams,
                format!("`{}` is not a screen source", spec.name),
            )),
        }
    }

    pub fn remove_source(&mut self, source: &SourceRef) -> Result<(), CommandError> {
        let (scene, index) = self.source_index(source)?;
        self.scenes[scene].sources.remove(index);
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

/// The file `source` shows, if it is an image source.
fn image_file(source: &SourceSpec) -> Option<&str> {
    match &source.kind {
        SourceKind::Image { file } => Some(file),
        _ => None,
    }
}

/// Whether `scene` has an image source showing `file`.
pub(crate) fn shows_image(scene: &SceneSpec, file: &str) -> bool {
    scene
        .sources
        .iter()
        .any(|source| image_file(source) == Some(file))
}

fn full_canvas() -> SourceRect {
    SourceRect {
        x: 0.0,
        y: 0.0,
        width: CANVAS_WIDTH as f32,
        height: CANVAS_HEIGHT as f32,
    }
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

/// Inputs the build can use besides its own static frames: screen and
/// camera captures by source id, and the image sources' frames.
pub(crate) struct LiveFrames<'a> {
    pub captures: &'a [(String, Arc<FrameMailbox>)],
    pub images: &'a ImageStore,
}

/// Turn `scene` into compositor input. Screen and camera sources without a
/// capture (or whose camera was not found) and image sources without a
/// frame (their file was missing when a project loaded) are left out.
pub(crate) fn build(
    scene: &SceneSpec,
    live: &LiveFrames<'_>,
    cache: &mut FrameCache,
) -> Result<SceneUpdate, CommandError> {
    let mut canvas = Scene::new(CANVAS_WIDTH, CANVAS_HEIGHT);
    let mut sources = Vec::with_capacity(scene.sources.len());
    for spec in &scene.sources {
        let id = SourceId::from(spec.id.as_str());
        let area = Rect {
            x: spec.rect.x,
            y: spec.rect.y,
            width: spec.rect.width,
            height: spec.rect.height,
        };
        let (layer, input) = match &spec.kind {
            SourceKind::ColorBars => (
                stretched(spec, id.clone(), CANVAS_WIDTH, CANVAS_HEIGHT),
                SourceInput::Static(cache.bars()?),
            ),
            SourceKind::Solid { color } => (
                stretched(spec, id.clone(), SOLID_SIZE, SOLID_SIZE),
                SourceInput::Static(cache.solid(*color)?),
            ),
            SourceKind::Screen { .. } | SourceKind::Camera { .. } => {
                let Some((_, mailbox)) =
                    live.captures.iter().find(|(source, _)| *source == spec.id)
                else {
                    continue;
                };
                (
                    Layer::fitted(spec.name.clone(), id.clone(), area),
                    SourceInput::Live(Arc::clone(mailbox)),
                )
            }
            SourceKind::Image { file } => {
                let Some(frame) = live.images.frame(file) else {
                    continue;
                };
                (
                    Layer::fitted(spec.name.clone(), id.clone(), area),
                    SourceInput::Static(Arc::clone(frame)),
                )
            }
        };
        canvas.layers.push(Layer {
            visible: spec.visible,
            ..layer.with_opacity(spec.opacity)
        });
        sources.push(SourceConfig {
            id,
            name: spec.name.clone(),
            input,
        });
    }
    // A project file can give any source the built-in id; only a solid one
    // in the built-in place moves.
    let animated = scene.sources.iter().any(|source| {
        source.id == BUILTIN_OVERLAY
            && source.rect == BUILTIN_OVERLAY_RECT
            && matches!(source.kind, SourceKind::Solid { .. })
    });
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
        .find(|layer| layer.source.as_str() == BUILTIN_OVERLAY)
    {
        layer.transform.x = overlay_x(tick);
    }
}

fn stretched(spec: &SourceSpec, id: SourceId, width: u32, height: u32) -> Layer {
    Layer::new(
        spec.name.clone(),
        id,
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
    use serde_json::json;

    use super::*;

    fn source(scene: &str, name: &str, kind: SourceKind) -> AddSourceRequest {
        AddSourceRequest {
            scene: SceneRef::name(scene),
            name: name.to_owned(),
            kind,
            rect: None,
            opacity: 1.0,
            visible: true,
        }
    }

    #[test]
    fn default_scene_matches_the_reference_scene() {
        let collection = SceneCollection::new();
        let list = collection.list();
        assert_eq!(list.active_scene_name, DEFAULT_SCENE);
        let names: Vec<_> = list.scenes[0].sources.iter().map(|s| &s.name).collect();
        assert_eq!(names, ["color_bars", "overlay"]);
        let update = build(
            collection.active(),
            &LiveFrames {
                captures: &[],
                images: &ImageStore::default(),
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
            captures: &[],
            images: &ImageStore::default(),
        };
        let mut cache = FrameCache::default();
        collection
            .update_source(&UpdateSourceRequest {
                source: SourceRef::named(DEFAULT_SCENE, "overlay"),
                x: Some(500.0),
                y: None,
                width: None,
                height: None,
                opacity: None,
                visible: None,
                file: None,
                camera_id: None,
                mode: None,
            })
            .expect("move");
        // Moved by the user: it stays where it was put.
        let moved = build(collection.active(), &none, &mut cache).expect("build");
        assert!(moved.animation.is_none());
        // A user source named `overlay` in a new scene is not animated.
        collection.create("Other").expect("create");
        collection
            .add_source(&AddSourceRequest {
                scene: SceneRef::name("Other"),
                name: "overlay".to_owned(),
                kind: SourceKind::ColorBars,
                rect: Some(BUILTIN_OVERLAY_RECT),
                opacity: 1.0,
                visible: true,
            })
            .expect("add");
        let other = build(
            collection.get(&SceneRef::name("Other")).expect("scene"),
            &none,
            &mut cache,
        )
        .expect("build");
        assert!(other.animation.is_none());
        // A loaded file may give another kind of source the built-in id: it
        // does not move.
        let (mut scenes, active) = collection.snapshot();
        scenes[0].sources[1].kind = SourceKind::ColorBars;
        scenes[0].sources[1].rect = BUILTIN_OVERLAY_RECT;
        collection.restore((scenes, active));
        let loaded = build(collection.active(), &none, &mut cache).expect("build");
        assert!(loaded.animation.is_none());
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
        collection
            .set_active(&SceneRef::name("Show"))
            .expect("activate");
        assert_eq!(
            collection.delete(&SceneRef::name("Show")).unwrap_err().code,
            ErrorCode::InvalidState
        );
        collection
            .set_active(&SceneRef::name(DEFAULT_SCENE))
            .expect("activate");
        collection.delete(&SceneRef::name("Show")).expect("delete");
        assert!(collection.get(&SceneRef::name("Show")).is_err());
    }

    #[test]
    fn scenes_and_sources_have_stable_ids_and_are_found_by_id_or_name() {
        let mut collection = SceneCollection::new();
        let list = collection.list();
        assert_eq!(list.active_scene_id, DEFAULT_SCENE_ID);
        let builtin: Vec<_> = list.scenes[0].sources.iter().map(|s| &s.id).collect();
        assert_eq!(builtin, [BUILTIN_BARS, BUILTIN_OVERLAY]);

        let show = collection.create("Show").expect("create");
        assert!(show.starts_with(SCENE_ID_PREFIX), "{show}");
        let red = collection
            .add_source(&AddSourceRequest {
                scene: SceneRef::id(&show),
                ..source("ignored", "red", SourceKind::ColorBars)
            })
            .expect("add");
        assert!(red.starts_with(SOURCE_ID_PREFIX), "{red}");
        // Source names repeat across scenes; ids do not.
        let other = collection
            .add_source(&source(DEFAULT_SCENE, "red", SourceKind::ColorBars))
            .expect("add");
        assert_ne!(red, other);

        // By id alone, by scene and name, or by both when they agree.
        let by_id = collection.source(&SourceRef::id(&red)).expect("by id");
        assert_eq!(by_id.name, "red");
        assert_eq!(
            collection
                .source(&SourceRef::named("Show", "red"))
                .expect("by name")
                .id,
            red
        );
        let both = SourceRef {
            id: Some(red.clone()),
            scene: Some(SceneRef::name("Show")),
            name: Some("red".to_owned()),
        };
        assert_eq!(collection.source(&both).expect("both").id, red);
        collection
            .set_active(&SceneRef {
                id: Some(show.clone()),
                name: Some("Show".to_owned()),
            })
            .expect("activate by both");
        assert_eq!(collection.list().active_scene_name, "Show");

        // An id and a name that disagree are refused, not resolved by id.
        for wrong in [
            SourceRef {
                name: Some("blue".to_owned()),
                ..SourceRef::id(&red)
            },
            SourceRef {
                scene: Some(SceneRef::name(DEFAULT_SCENE)),
                ..SourceRef::id(&red)
            },
            // A scene that does not exist does not hold it either.
            SourceRef {
                scene: Some(SceneRef::name("Typo")),
                ..SourceRef::id(&red)
            },
        ] {
            let error = collection.source(&wrong).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidParams, "{wrong:?}");
        }
        let wrong_scene = SceneRef {
            id: Some(show.clone()),
            name: Some(DEFAULT_SCENE.to_owned()),
        };
        assert_eq!(
            collection.set_active(&wrong_scene).unwrap_err().code,
            ErrorCode::InvalidParams
        );
        let unknown = collection.source(&SourceRef::id("src_gone")).unwrap_err();
        assert_eq!(unknown.code, ErrorCode::NotFound);
        assert_eq!(unknown.details, Some(json!({"source_id": "src_gone"})));

        // Ids are random, not derived from names: adding a removed source's
        // name again gives a new id.
        collection
            .remove_source(&SourceRef::id(&red))
            .expect("remove");
        let again = collection
            .add_source(&source("Show", "red", SourceKind::ColorBars))
            .expect("add again");
        assert_ne!(again, red);
        assert!(collection.source(&SourceRef::id(&red)).is_err());
    }

    #[test]
    fn sources_move_in_the_drawing_order_and_hide() {
        let mut collection = SceneCollection::new();
        collection.create("Show").expect("create");
        for name in ["a", "b", "c"] {
            collection
                .add_source(&source("Show", name, SourceKind::ColorBars))
                .expect("add");
        }
        let order = |collection: &SceneCollection| -> Vec<String> {
            let scene = collection.get(&SceneRef::name("Show")).expect("scene");
            scene.sources.iter().map(|s| s.name.clone()).collect()
        };
        let to = |name: &str, index| MoveSourceRequest {
            source: SourceRef::named("Show", name),
            index,
        };
        // To the bottom, the others shift up.
        collection.move_source(&to("c", 0)).expect("move");
        assert_eq!(order(&collection), ["c", "a", "b"]);
        // To the top, by id.
        let c = collection
            .source(&SourceRef::named("Show", "c"))
            .expect("c");
        collection
            .move_source(&MoveSourceRequest {
                source: SourceRef::id(c.id.clone()),
                index: 2,
            })
            .expect("move");
        assert_eq!(order(&collection), ["a", "b", "c"]);
        // Its own place changes nothing.
        collection.move_source(&to("b", 1)).expect("move");
        assert_eq!(order(&collection), ["a", "b", "c"]);
        let past = collection.move_source(&to("a", 3)).unwrap_err();
        assert_eq!(past.code, ErrorCode::InvalidParams);
        assert_eq!(past.details, Some(json!({"index": 3, "max_index": 2})));
        assert_eq!(order(&collection), ["a", "b", "c"]);
        assert_eq!(
            collection.move_source(&to("gone", 0)).unwrap_err().code,
            ErrorCode::NotFound
        );

        collection
            .update_source(&UpdateSourceRequest {
                source: SourceRef::named("Show", "b"),
                x: None,
                y: None,
                width: None,
                height: None,
                opacity: None,
                visible: Some(false),
                file: None,
                camera_id: None,
                mode: None,
            })
            .expect("hide");
        let scene = collection.get(&SceneRef::name("Show")).expect("scene");
        let visible: Vec<_> = scene.sources.iter().map(|s| s.visible).collect();
        assert_eq!(visible, [true, false, true]);
        // The compositor gets every layer in order, the hidden one marked.
        let update = build(
            scene,
            &LiveFrames {
                captures: &[],
                images: &ImageStore::default(),
            },
            &mut FrameCache::default(),
        )
        .expect("build");
        let layers: Vec<_> = update
            .scene
            .layers
            .iter()
            .map(|layer| (layer.name.as_str(), layer.visible))
            .collect();
        assert_eq!(layers, [("a", true), ("b", false), ("c", true)]);
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
                    target_kinds: vec![capture_api::VideoSourceKind::Monitor],
                    cursor_mode: capture_api::CursorMode::Embedded,
                    capture_target_id: None,
                },
            ),
        ] {
            collection
                .add_source(&source("Show", name, kind))
                .expect("add");
        }
        let scene = collection
            .get(&SceneRef::name("Show"))
            .expect("scene")
            .clone();
        let mut cache = FrameCache::default();
        let none = LiveFrames {
            captures: &[],
            images: &ImageStore::default(),
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
        let screen_id = scene.sources[1].id.clone();
        let mailbox = Arc::new(FrameMailbox::new());
        let with_capture = build(
            &scene,
            &LiveFrames {
                captures: &[(screen_id, Arc::clone(&mailbox))],
                images: &ImageStore::default(),
            },
            &mut cache,
        )
        .expect("build");
        assert_eq!(with_capture.sources.len(), 2);
        assert!(with_capture.animation.is_none());
    }
}
