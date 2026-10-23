//! Scene, mixer and preview commands. They change engine state; changes to
//! what the running pipeline renders or mixes apply live.

use std::sync::Arc;

use command_api::{
    AddBusRequest, AddChannelRequest, AddSourceRequest, BusRef, ChannelRef, CommandError,
    ErrorCode, MixerState, PreviewState, SceneList, SceneRef, SetChannelInputRequest,
    SetSendRequest, SourceKind, SourceRef, UpdateBusRequest, UpdateChannelRequest,
    UpdateSourceRequest,
};
use media_core::FrameMailbox;

use crate::composite::FPS;
use crate::pipeline::{PREVIEW_EVERY, PREVIEW_HEIGHT, PREVIEW_WIDTH};
use crate::scenes::SceneCollection;
use crate::{Engine, audio};

impl Engine {
    /// The in-process preview: the newest quarter-resolution frame while
    /// `set_preview` is enabled. This is the media plane for embedding UIs;
    /// all control goes through commands.
    pub fn preview_frames(&self) -> Arc<FrameMailbox> {
        Arc::clone(&self.preview_frames)
    }

    pub(crate) fn list_scenes(&self) -> SceneList {
        self.scenes.list()
    }

    /// Apply `change`; if it alters what the running pipeline renders, swap
    /// the scene live, and undo the change if that fails.
    fn edit_scenes(
        &mut self,
        change: impl FnOnce(&mut SceneCollection) -> Result<(), CommandError>,
    ) -> Result<SceneList, CommandError> {
        let before = self.scenes.snapshot();
        let rendered = self.scenes.active().clone();
        change(&mut self.scenes)?;
        let (allow_new_captures, keep) = self.capture_policy();
        if let Some(pipeline) = &mut self.pipeline {
            if *self.scenes.active() != rendered {
                if let Err(error) = pipeline.apply_scene(
                    self.capture.as_mut(),
                    self.scenes.active(),
                    &keep,
                    allow_new_captures,
                ) {
                    self.scenes.restore(before);
                    return Err(error);
                }
            } else {
                // A source removed from a scene that is not rendered (or
                // its whole scene deleted) ends its kept capture too.
                pipeline.retire_unkept(&keep);
            }
        }
        Ok(self.scenes.list())
    }

    pub(crate) fn create_scene(&mut self, name: &str) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.create(name).map(drop))
    }

    pub(crate) fn delete_scene(&mut self, scene: &SceneRef) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.delete(scene))
    }

    pub(crate) fn set_active_scene(&mut self, scene: &SceneRef) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.set_active(scene))
    }

    pub(crate) fn add_source(
        &mut self,
        request: &AddSourceRequest,
    ) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.add_source(request).map(drop))
    }

    pub(crate) fn update_source(
        &mut self,
        request: &UpdateSourceRequest,
    ) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.update_source(request))
    }

    pub(crate) fn remove_source(&mut self, source: &SourceRef) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.remove_source(source))
    }

    /// Restart a screen source's capture (e.g. after it ended), optionally
    /// on another source (`source_id`, kept by the scene), while the
    /// pipeline and its outputs keep running. On error nothing changes.
    pub(crate) fn restart_source(
        &mut self,
        source: &SourceRef,
        source_id: Option<&str>,
    ) -> Result<SceneList, CommandError> {
        let spec = self.scenes.source(source)?;
        let name = &spec.name.clone();
        // Found once; from here on by its id.
        let source = &SourceRef::id(spec.id.clone());
        if !matches!(spec.kind, SourceKind::Screen { .. }) {
            return Err(CommandError::new(
                ErrorCode::InvalidParams,
                format!("`{name}` does not capture; only screen sources can be restarted"),
            ));
        }
        match &self.pipeline {
            None => return Err(no_pipeline(name)),
            Some(pipeline) if !pipeline.is_running() => {
                return Err(CommandError::new(
                    ErrorCode::InvalidState,
                    "the media pipeline stopped unexpectedly, so a new capture would not be \
                     rendered; turn the preview off and stop the open outputs to collect their \
                     reports",
                ));
            }
            Some(_) => {}
        }
        let before = self.scenes.snapshot();
        if let Some(id) = source_id {
            self.scenes.set_screen_source_id(source, id)?;
        }
        let source = self.scenes.source(source)?.clone();
        let (allow_new_captures, keep) = self.capture_policy();
        let restarted = match &mut self.pipeline {
            Some(pipeline) => pipeline.restart_screen(
                self.capture.as_mut(),
                self.scenes.active(),
                &source,
                &keep,
                allow_new_captures,
            ),
            None => Err(no_pipeline(name)),
        };
        if let Err(error) = restarted {
            self.scenes.restore(before);
            return Err(error);
        }
        Ok(self.scenes.list())
    }

    /// Choose what screen source `source` captures, while nothing captures
    /// it: a running (or ended) capture keeps its target until
    /// `restart_source`. If the source is rendered, its capture starts now.
    pub(crate) fn set_screen_source(
        &mut self,
        source: &SourceRef,
        screen: &str,
    ) -> Result<SceneList, CommandError> {
        let spec = self.scenes.source(source)?;
        let (id, name) = (spec.id.clone(), spec.name.clone());
        if self
            .pipeline
            .as_ref()
            .is_some_and(|pipeline| pipeline.has_scene_capture(&id))
        {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                format!(
                    "`{name}` is capturing; switch a running capture with restart_source and a \
                     `source_id`"
                ),
            ));
        }
        let source = SourceRef::id(id);
        self.edit_scenes(|scenes| scenes.set_screen_source_id(&source, screen))
    }

    /// Whether new captures may start (open outputs must not wait for a
    /// picker; the preview alone means someone is at the UI), and the
    /// screen sources whose captures keep running.
    fn capture_policy(&self) -> (bool, Vec<String>) {
        (
            self.recording.is_none() && self.streaming.is_none(),
            self.scenes.screen_source_ids(),
        )
    }

    pub(crate) fn mixer_state(&self) -> MixerState {
        match &self.pipeline {
            Some(pipeline) => pipeline.mixer_state(),
            None => audio::mixer_state(&self.audio, None),
        }
    }

    /// Structure changes need an idle pipeline: its mixer is built for a
    /// fixed set of channels.
    fn audio_structure_idle(&self) -> Result<(), CommandError> {
        if self.pipeline.is_some() {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                "channels and buses can only be added or removed, and a channel given another \
                 input, while the pipeline is idle; stop the preview and outputs first (levels, \
                 mutes, balance, sends and assignments change live)",
            ));
        }
        Ok(())
    }

    pub(crate) fn add_channel(
        &mut self,
        request: AddChannelRequest,
    ) -> Result<MixerState, CommandError> {
        self.audio_structure_idle()?;
        audio::add_channel(&mut self.audio, request)?;
        Ok(self.mixer_state())
    }

    pub(crate) fn remove_channel(
        &mut self,
        channel: &ChannelRef,
    ) -> Result<MixerState, CommandError> {
        self.audio_structure_idle()?;
        audio::remove_channel(&mut self.audio, channel)?;
        Ok(self.mixer_state())
    }

    pub(crate) fn set_channel_input(
        &mut self,
        request: SetChannelInputRequest,
    ) -> Result<MixerState, CommandError> {
        self.audio_structure_idle()?;
        audio::set_channel_input(&mut self.audio, request)?;
        Ok(self.mixer_state())
    }

    /// Live while the pipeline runs. A run with an output's own sources has
    /// its own channels; then the change applies to the run only.
    pub(crate) fn update_channel(
        &mut self,
        request: UpdateChannelRequest,
    ) -> Result<MixerState, CommandError> {
        match &mut self.pipeline {
            Some(pipeline) => {
                pipeline.update_channel(&request)?;
                if pipeline.audio_from_setup() {
                    audio::update_channel(&mut self.audio, &request)?;
                }
            }
            None => {
                audio::update_channel(&mut self.audio, &request)?;
            }
        }
        Ok(self.mixer_state())
    }

    pub(crate) fn add_bus(&mut self, request: AddBusRequest) -> Result<MixerState, CommandError> {
        self.audio_structure_idle()?;
        audio::add_bus(&mut self.audio, request)?;
        Ok(self.mixer_state())
    }

    pub(crate) fn remove_bus(&mut self, bus: &BusRef) -> Result<MixerState, CommandError> {
        self.audio_structure_idle()?;
        audio::remove_bus(&mut self.audio, bus)?;
        Ok(self.mixer_state())
    }

    /// Live, as [`Engine::update_channel`].
    pub(crate) fn set_send(&mut self, request: SetSendRequest) -> Result<MixerState, CommandError> {
        match &mut self.pipeline {
            Some(pipeline) => {
                pipeline.set_send(&request)?;
                if pipeline.audio_from_setup() {
                    audio::set_send(&mut self.audio, &request)?;
                }
            }
            None => {
                audio::set_send(&mut self.audio, &request)?;
            }
        }
        Ok(self.mixer_state())
    }

    pub(crate) fn update_bus(
        &mut self,
        request: UpdateBusRequest,
    ) -> Result<MixerState, CommandError> {
        match &mut self.pipeline {
            Some(pipeline) => {
                pipeline.update_bus(&request)?;
                if pipeline.audio_from_setup() {
                    audio::update_bus(&mut self.audio, &request)?;
                }
            }
            None => {
                audio::update_bus(&mut self.audio, &request)?;
            }
        }
        Ok(self.mixer_state())
    }

    pub(crate) fn set_preview(&mut self, enabled: bool) -> Result<PreviewState, CommandError> {
        let mut state = PreviewState {
            enabled: self.preview_enabled,
            width: PREVIEW_WIDTH,
            height: PREVIEW_HEIGHT,
            fps: (u64::from(FPS) / PREVIEW_EVERY) as u32,
            frames_published: None,
            frames_dropped: None,
            pipeline: None,
            warnings: Vec::new(),
        };
        if enabled && !self.preview_enabled {
            self.ensure_pipeline(None)?;
            self.preview_enabled = true;
            if let Some(pipeline) = &self.pipeline {
                pipeline.set_preview(Some(Arc::clone(&self.preview_frames)));
            }
        } else if !enabled && self.preview_enabled {
            if let Some((published, dropped)) = self
                .pipeline
                .as_ref()
                .and_then(|pipeline| pipeline.set_preview(None))
            {
                state.frames_published = Some(published);
                state.frames_dropped = Some(dropped);
            }
            self.preview_enabled = false;
            // A drained mailbox does not show a stale last frame later.
            let _ = self.preview_frames.take();
            let (pipeline, warnings) = self.release_pipeline();
            if pipeline.pipeline_stopped {
                state.pipeline = Some(pipeline);
            }
            state.warnings = warnings;
        }
        state.enabled = self.preview_enabled;
        Ok(state)
    }
}

fn no_pipeline(name: &str) -> CommandError {
    CommandError::new(
        ErrorCode::InvalidState,
        format!("nothing captures `{name}`: no pipeline runs (start the preview or an output)"),
    )
}
