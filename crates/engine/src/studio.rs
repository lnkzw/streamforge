//! Scene, mixer and preview commands. They change engine state; changes to
//! what the running pipeline renders or mixes apply live.

use std::sync::Arc;

use command_api::{
    AddSourceRequest, AudioSetup, CommandError, ErrorCode, MixerState, PreviewState, SceneList,
    StripChange, UpdateSourceRequest,
};
use media_core::FrameMailbox;

use crate::Engine;
use crate::composite::FPS;
use crate::pipeline::{
    PREVIEW_EVERY, PREVIEW_HEIGHT, PREVIEW_WIDTH, apply_strip, idle_mixer_state,
};
use crate::scenes::SceneCollection;

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
        // Open outputs must not wait for a picker; the preview alone means
        // someone is at the UI.
        let allow_new_captures = self.recording.is_none() && self.streaming.is_none();
        let keep = self.scenes.screen_source_ids();
        if let Some(pipeline) = &mut self.pipeline
            && *self.scenes.active() != rendered
            && let Err(error) = pipeline.apply_scene(
                self.capture.as_mut(),
                self.scenes.active(),
                &keep,
                allow_new_captures,
            )
        {
            self.scenes.restore(before);
            return Err(error);
        }
        Ok(self.scenes.list())
    }

    pub(crate) fn create_scene(&mut self, name: &str) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.create(name))
    }

    pub(crate) fn delete_scene(&mut self, name: &str) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.delete(name))
    }

    pub(crate) fn set_active_scene(&mut self, name: &str) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.set_active(name))
    }

    pub(crate) fn add_source(
        &mut self,
        request: &AddSourceRequest,
    ) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.add_source(request))
    }

    pub(crate) fn update_source(
        &mut self,
        request: &UpdateSourceRequest,
    ) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.update_source(request))
    }

    pub(crate) fn remove_source(
        &mut self,
        scene: &str,
        name: &str,
    ) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.remove_source(scene, name))
    }

    pub(crate) fn mixer_state(&self) -> MixerState {
        match &self.pipeline {
            Some(pipeline) => pipeline.mixer_state(),
            None => idle_mixer_state(&self.audio),
        }
    }

    pub(crate) fn set_mixer_strip(
        &mut self,
        change: StripChange,
    ) -> Result<MixerState, CommandError> {
        match &mut self.pipeline {
            Some(pipeline) => {
                pipeline.set_strip(change)?;
                if pipeline.audio_from_setup() {
                    apply_strip(&mut self.audio, change)?;
                }
            }
            None => {
                apply_strip(&mut self.audio, change)?;
            }
        }
        Ok(self.mixer_state())
    }

    pub(crate) fn set_master_gain(&mut self, gain_db: f32) -> MixerState {
        if let Some(pipeline) = &mut self.pipeline {
            pipeline.set_master_gain(gain_db);
            if !pipeline.audio_from_setup() {
                return self.mixer_state();
            }
        }
        self.audio.master_gain_db = gain_db;
        self.mixer_state()
    }

    pub(crate) fn set_audio_sources(
        &mut self,
        setup: AudioSetup,
    ) -> Result<MixerState, CommandError> {
        if self.pipeline.is_some() {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                "audio sources can only change while the pipeline is idle; stop the preview and \
                 outputs first (gains and mutes change live with set_mixer_strip)",
            ));
        }
        self.audio = setup;
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
