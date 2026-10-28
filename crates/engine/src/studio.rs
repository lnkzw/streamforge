//! Scene, mixer and preview commands. They change engine state; changes to
//! what the running pipeline renders or mixes apply live.

use std::sync::Arc;

use audio_engine::BusRef as GraphBus;
use command_api::{
    AddBusRequest, AddChannelRequest, AddSourceRequest, BusRef, ChannelRef, CommandError,
    ErrorCode, MixerState, MoveSourceRequest, OutputKind, OutputMix, OutputProfileSet,
    OutputProfiles, PROGRAM_BUS_ID, PreviewState, SceneList, SceneRef, SetChannelInputRequest,
    SetOutputProfileRequest, SetSendRequest, SourceKind, SourceRef, SourceSpec, UpdateBusRequest,
    UpdateChannelRequest, UpdateSourceRequest,
};
use media_core::{FrameMailbox, Rational};
use serde_json::Value;

use crate::composite::FPS;
use crate::images::Replaced;
use crate::outputs::{mix_column, output_id};
use crate::pipeline::{PREVIEW_EVERY, PREVIEW_HEIGHT, PREVIEW_WIDTH, Pipeline};
use crate::scenes::{self, SceneCollection};
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

    /// The scene list with the warnings of the captures a command started.
    fn scene_list(&self, warnings: Vec<String>) -> SceneList {
        SceneList {
            warnings,
            ..self.scenes.list()
        }
    }

    /// Apply `change`; if it alters what the running pipeline renders, swap
    /// the scene live, and undo the change if that fails. Image frames no
    /// source shows any more are dropped.
    fn edit_scenes(
        &mut self,
        change: impl FnOnce(&mut SceneCollection) -> Result<(), CommandError>,
    ) -> Result<SceneList, CommandError> {
        let edited = self.apply_edit(None, change);
        self.images.retain(&self.scenes.image_files());
        edited
    }

    /// [`edit_scenes`](Self::edit_scenes) after reading image `file`: if
    /// its frame changed (`replaced`), a rendered scene showing it is
    /// swapped too, and the read is undone if the edit fails.
    fn edit_scenes_reading(
        &mut self,
        file: &str,
        replaced: Option<Replaced>,
        change: impl FnOnce(&mut SceneCollection) -> Result<(), CommandError>,
    ) -> Result<SceneList, CommandError> {
        let edited = self.apply_edit(replaced.is_some().then_some(file), change);
        if edited.is_err()
            && let Some(replaced) = replaced
        {
            self.images.put_back(replaced);
        }
        self.images.retain(&self.scenes.image_files());
        edited
    }

    fn apply_edit(
        &mut self,
        changed_image: Option<&str>,
        change: impl FnOnce(&mut SceneCollection) -> Result<(), CommandError>,
    ) -> Result<SceneList, CommandError> {
        let before = self.scenes.snapshot();
        let rendered = self.scenes.active().clone();
        change(&mut self.scenes)?;
        let (allow_new_captures, keep) = self.capture_policy();
        let mut warnings = Vec::new();
        if let Some(pipeline) = &mut self.pipeline {
            let active = self.scenes.active();
            if *active != rendered
                || changed_image.is_some_and(|file| scenes::shows_image(active, file))
            {
                match pipeline.apply_scene(
                    &mut self.capture,
                    active,
                    &self.images,
                    &keep,
                    allow_new_captures,
                ) {
                    Ok(started) => warnings = started,
                    Err(error) => {
                        self.scenes.restore(before);
                        return Err(error);
                    }
                }
            } else {
                // A source removed from a scene that is not rendered (or
                // its whole scene deleted) ends its kept capture too.
                pipeline.retire_unkept(&keep);
            }
        }
        Ok(self.scene_list(warnings))
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

    /// Add a source. An image is read first; without a rectangle it keeps
    /// its own size.
    pub(crate) fn add_source(
        &mut self,
        request: &AddSourceRequest,
    ) -> Result<SceneList, CommandError> {
        let SourceKind::Image { file } = &request.kind else {
            return self.edit_scenes(|scenes| scenes.add_source(request).map(drop));
        };
        let replaced = self.read_image(file, false)?;
        let mut request = request.clone();
        if request.rect.is_none() {
            request.rect = self.images.natural_rect(file);
        }
        self.edit_scenes_reading(file, replaced, |scenes| {
            scenes.add_source(&request).map(drop)
        })
    }

    /// Change a source; an image source's `file` is read first, also when
    /// it is the one shown (a refresh at `control` access).
    pub(crate) fn update_source(
        &mut self,
        request: &UpdateSourceRequest,
    ) -> Result<SceneList, CommandError> {
        let kind = &self.scenes.source(&request.source)?.kind;
        if matches!(kind, SourceKind::Camera { .. })
            && (request.camera_id.is_some() || request.mode.is_some())
        {
            return self.update_camera(request);
        }
        let image = matches!(kind, SourceKind::Image { .. });
        let Some(file) = request.file.as_deref().filter(|_| image) else {
            return self.edit_scenes(|scenes| scenes.update_source(request));
        };
        let replaced = self.read_image(file, true)?;
        self.edit_scenes_reading(file, replaced, |scenes| scenes.update_source(request))
    }

    /// Change a camera source's camera or mode (and whatever else
    /// `request` changes). A camera that runs opens again as the source now
    /// says; if it fails, the change is undone and the camera it had opens
    /// again.
    fn update_camera(&mut self, request: &UpdateSourceRequest) -> Result<SceneList, CommandError> {
        let old = self.scenes.source(&request.source)?.clone();
        let running = self
            .pipeline
            .as_ref()
            .is_some_and(|pipeline| pipeline.has_scene_capture(&old.id));
        if !running {
            return self.edit_scenes(|scenes| scenes.update_source(request));
        }
        let before = self.scenes.snapshot();
        self.scenes.update_source(request)?;
        let new = self.scenes.source(&SourceRef::id(old.id.clone()))?.clone();
        if new.kind == old.kind {
            // It captures that camera in that mode already.
            self.scenes.restore(before);
            return self.edit_scenes(|scenes| scenes.update_source(request));
        }
        match self.restart_capture(&new) {
            Ok(warnings) => Ok(self.scene_list(warnings)),
            Err(mut error) => {
                self.scenes.restore(before);
                if let Err(again) = self.restart_capture(&old) {
                    error.message = format!(
                        "{}; the camera it had did not open again either ({}), so it shows \
                         nothing until restart_source",
                        error.message, again.message
                    );
                }
                Err(error)
            }
        }
    }

    /// Start `source`'s capture again in the running pipeline, as the
    /// scenes now say.
    fn restart_capture(&mut self, source: &SourceSpec) -> Result<Vec<String>, CommandError> {
        let (allow_new_captures, keep) = self.capture_policy();
        match &mut self.pipeline {
            Some(pipeline) => pipeline.restart_capture(
                &mut self.capture,
                self.scenes.active(),
                &self.images,
                source,
                &keep,
                allow_new_captures,
            ),
            None => Err(no_pipeline(&source.name)),
        }
    }

    pub(crate) fn move_source(
        &mut self,
        request: &MoveSourceRequest,
    ) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.move_source(request))
    }

    pub(crate) fn remove_source(&mut self, source: &SourceRef) -> Result<SceneList, CommandError> {
        self.edit_scenes(|scenes| scenes.remove_source(source))
    }

    /// Restart a screen source's capture (e.g. after it ended), optionally
    /// on another source (`capture_target_id`, kept by the scene), or open a
    /// camera source's camera again, while the pipeline and its outputs keep
    /// running; or read an image source's file again, also while idle. On
    /// error nothing changes, except that a camera that does not open again
    /// shows nothing.
    pub(crate) fn restart_source(
        &mut self,
        source: &SourceRef,
        capture_target_id: Option<&str>,
    ) -> Result<SceneList, CommandError> {
        let spec = self.scenes.source(source)?;
        let name = &spec.name.clone();
        // Found once; from here on by its id.
        let source = &SourceRef::id(spec.id.clone());
        match &spec.kind {
            SourceKind::Screen { .. } => {}
            SourceKind::Image { .. } | SourceKind::Camera { .. } if capture_target_id.is_some() => {
                let kind = if matches!(spec.kind, SourceKind::Image { .. }) {
                    "an image"
                } else {
                    "a camera"
                };
                return Err(CommandError::new(
                    ErrorCode::InvalidParams,
                    format!(
                        "`{name}` is {kind} source; `capture_target_id` applies to screen \
                         sources only"
                    ),
                ));
            }
            SourceKind::Image { file } => {
                let file = file.clone();
                let replaced = self.read_image(&file, true)?;
                return self.edit_scenes_reading(&file, replaced, |_| Ok(()));
            }
            SourceKind::Camera { .. } => {}
            SourceKind::ColorBars | SourceKind::Solid { .. } => {
                return Err(CommandError::new(
                    ErrorCode::InvalidParams,
                    format!(
                        "`{name}` does not capture or read a file; only screen, camera and \
                         image sources can be restarted"
                    ),
                ));
            }
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
        if let Some(id) = capture_target_id {
            self.scenes.set_capture_target_id(source, id)?;
        }
        let source = self.scenes.source(source)?.clone();
        match self.restart_capture(&source) {
            Ok(warnings) => Ok(self.scene_list(warnings)),
            Err(error) => {
                self.scenes.restore(before);
                Err(error)
            }
        }
    }

    /// Choose what screen source `source` captures, while nothing captures
    /// it: a running (or ended) capture keeps its target until
    /// `restart_source`. If the source is rendered, its capture starts now.
    pub(crate) fn set_capture_target(
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
                     `capture_target_id`"
                ),
            ));
        }
        let source = SourceRef::id(id);
        self.edit_scenes(|scenes| scenes.set_capture_target_id(&source, screen))
    }

    /// Whether new picker captures may start (open outputs must not wait
    /// for a picker; the preview alone means someone is at the UI), and the
    /// screen and camera sources whose captures keep running.
    fn capture_policy(&self) -> (bool, Vec<String>) {
        (
            self.recording.is_none() && self.streaming.is_none(),
            self.scenes.capture_source_ids(),
        )
    }

    pub(crate) fn mixer_state(&self) -> MixerState {
        let mut state = match &self.pipeline {
            Some(pipeline) => pipeline.mixer_state(),
            None => audio::mixer_state(&self.audio, None),
        };
        state.outputs = OutputKind::ALL
            .into_iter()
            .map(|output| {
                let running = self.output_open(output);
                let live = self.pipeline.as_ref().filter(|_| running);
                // Not running: what it starts with, also when it joins a
                // run (which mixes the configured graph).
                let (bus, tracks) = match (output_id(output), live) {
                    (Some(id), Some(pipeline)) => {
                        (pipeline.output_mix(id), pipeline.output_tracks(id))
                    }
                    (None, Some(pipeline)) => (pipeline.monitor_mix(), Vec::new()),
                    (_, None) if output == OutputKind::Recording => {
                        (None, self.profiles.recording.tracks.clone())
                    }
                    (_, None) => (None, Vec::new()),
                };
                OutputMix {
                    output,
                    bus_id: bus.unwrap_or_else(|| self.profiles.mix(output).to_owned()),
                    tracks,
                    running,
                }
            })
            .collect();
        state
    }

    /// Whether it runs: an open encoder output, or a monitor that plays.
    fn output_open(&self, output: OutputKind) -> bool {
        match output {
            OutputKind::Recording => self.recording.is_some(),
            OutputKind::Stream => self.streaming.is_some(),
            OutputKind::Monitor => self
                .pipeline
                .as_ref()
                .is_some_and(Pipeline::monitor_playing),
        }
    }

    /// Only while idle, like a channel's device.
    pub(crate) fn set_monitor_device(
        &mut self,
        device: Option<String>,
    ) -> Result<OutputProfiles, CommandError> {
        if self.pipeline.is_some() {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                "the monitor's device can only change while the pipeline is idle; stop the \
                 preview and outputs first (switching the monitor on or off is live)",
            ));
        }
        self.profiles.monitor.device = device;
        Ok(self.profiles.clone())
    }

    /// Change an output's profile. The whole new profile is checked
    /// first, so a refused request changes nothing. A running output then
    /// fades to a new mix now; tracks and encoder settings wait for its
    /// next start. While the pipeline runs, the monitor switches on or off
    /// and changes its bus now.
    pub(crate) fn set_output_profile(
        &mut self,
        request: SetOutputProfileRequest,
    ) -> Result<OutputProfileSet, CommandError> {
        let output = request.output;
        if output != OutputKind::Stream && request.video_bitrate_kbps.is_some() {
            return Err(CommandError::new(
                ErrorCode::InvalidParams,
                "`video_bitrate_kbps` is for the stream; a recording's quality is not a bitrate",
            ));
        }
        if output != OutputKind::Recording && request.tracks.is_some() {
            return Err(CommandError::new(
                ErrorCode::InvalidParams,
                "`tracks` is for the recording; a stream carries one audio track",
            ));
        }
        if output != OutputKind::Monitor && request.enabled.is_some() {
            return Err(CommandError::new(
                ErrorCode::InvalidParams,
                "`enabled` is for the monitor; the recording and the stream start and stop",
            ));
        }
        let bus_id = |bus| -> Result<String, CommandError> {
            Ok(match audio::bus_position(&self.audio, bus)? {
                GraphBus::Program => PROGRAM_BUS_ID.to_owned(),
                GraphBus::Aux(index) => self.audio.buses[index].id.clone(),
            })
        };
        let mut next = self.profiles.clone();
        let mix = request.mix.as_ref().map(bus_id).transpose()?;
        if let Some(mix) = &mix {
            *next.mix_mut(output) = mix.clone();
        }
        if let Some(tracks) = &request.tracks {
            next.recording.tracks = tracks
                .iter()
                .enumerate()
                .map(|(index, bus)| bus_id(bus).map_err(|error| in_track(error, index)))
                .collect::<Result<_, _>>()?;
        }
        if let Some(encoder) = request.video_encoder {
            let Some(slot) = next.video_encoder_mut(output) else {
                return Err(CommandError::new(
                    ErrorCode::InvalidParams,
                    "`video_encoder` is for the recording and the stream; the monitor plays audio",
                ));
            };
            *slot = encoder;
        }
        if let Some(enabled) = request.enabled {
            next.monitor.enabled = enabled;
        }
        if let Some(kbps) = request.video_bitrate_kbps {
            next.stream.video_bitrate_kbps = kbps;
        }
        // The rules across the profiles, as a project file is checked: e.g.
        // a new mix that is already a track.
        if let Some(found) = next.problems(&self.audio).into_iter().next() {
            return Err(CommandError::new(ErrorCode::InvalidParams, found.message)
                .with_details(serde_json::json!({ "path": found.path })));
        }

        let running = self.output_open(output);
        // A running recording keeps its tracks until it stops; a mix that is
        // one of them would put the same bus in the file twice.
        if running
            && let (Some(mix), Some(id), Some(pipeline)) = (&mix, output_id(output), &self.pipeline)
            && pipeline.output_tracks(id).contains(mix)
            && let Some(column) = pipeline.mix_column(mix)
        {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                format!(
                    "the running recording records bus `{}` as a track until it stops; choose \
                     another mix, or stop the recording first",
                    pipeline.bus_name(column)
                ),
            )
            .with_details(serde_json::json!({ "bus": mix })));
        }
        let mut applied_now = Vec::new();
        let mut next_start = Vec::new();
        let mut warnings = Vec::new();
        if output == OutputKind::Monitor {
            if let Some(pipeline) = &mut self.pipeline {
                let column = mix_column(pipeline, &next, OutputKind::Monitor);
                match request.enabled {
                    Some(true) => {
                        warnings = pipeline.monitor_on(
                            self.capture.audio.as_mut(),
                            &next.monitor.device,
                            column,
                        );
                        applied_now.push("enabled");
                    }
                    Some(false) => {
                        pipeline.monitor_off();
                        applied_now.push("enabled");
                    }
                    None => {}
                }
                // On a device that runs (on or off), now; otherwise when it
                // is switched on next.
                if mix.is_some() {
                    if pipeline.monitor_device_running() {
                        pipeline.set_monitor_mix(column);
                        applied_now.push("mix");
                    } else {
                        next_start.push("mix");
                    }
                }
            }
        } else if running && let Some(id) = output_id(output) {
            if let Some(mix) = &mix {
                match self
                    .pipeline
                    .as_ref()
                    .and_then(|pipeline| Some((pipeline, pipeline.mix_column(mix)?)))
                {
                    Some((pipeline, column)) => {
                        pipeline.set_output_mix(id, column);
                        applied_now.push("mix");
                    }
                    // The run mixes the configured graph, which has the bus.
                    None => {
                        debug_assert!(false, "the mix `{mix}` is not in this run");
                        next_start.push("mix");
                    }
                }
            }
            for (field, given) in [
                ("tracks", request.tracks.is_some()),
                ("video_encoder", request.video_encoder.is_some()),
                ("video_bitrate_kbps", request.video_bitrate_kbps.is_some()),
            ] {
                if given {
                    next_start.push(field);
                }
            }
        }
        self.profiles = next;
        Ok(OutputProfileSet {
            profiles: self.profiles.clone(),
            applied_now,
            next_start,
            warnings,
        })
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

    /// Live while the pipeline runs.
    pub(crate) fn update_channel(
        &mut self,
        request: UpdateChannelRequest,
    ) -> Result<MixerState, CommandError> {
        // The run mixes the configured graph: change both, the run first
        // (it checks the request the same way).
        if let Some(pipeline) = &mut self.pipeline {
            pipeline.update_channel(&request)?;
        }
        audio::update_channel(&mut self.audio, &request)?;
        Ok(self.mixer_state())
    }

    pub(crate) fn add_bus(&mut self, request: AddBusRequest) -> Result<MixerState, CommandError> {
        self.audio_structure_idle()?;
        audio::add_bus(&mut self.audio, request)?;
        Ok(self.mixer_state())
    }

    /// Refused while an output profile takes the bus as its mix, so a
    /// profile never names a missing bus.
    pub(crate) fn remove_bus(&mut self, bus: &BusRef) -> Result<MixerState, CommandError> {
        self.audio_structure_idle()?;
        if let GraphBus::Aux(index) = audio::bus_position(&self.audio, bus)? {
            let id = &self.audio.buses[index].id;
            let users: Vec<&str> = OutputKind::ALL
                .into_iter()
                .filter(|output| {
                    self.profiles.mix(*output) == id
                        || (*output == OutputKind::Recording
                            && self.profiles.recording.tracks.contains(id))
                })
                .map(OutputKind::as_str)
                .collect();
            if !users.is_empty() {
                return Err(CommandError::new(
                    ErrorCode::InvalidState,
                    format!(
                        "bus `{}` is in the {} profile (its mix or a track); change it with \
                         set_output_profile first",
                        self.audio.buses[index].name,
                        users.join(" and ")
                    ),
                )
                .with_details(serde_json::json!({ "outputs": users })));
            }
        }
        audio::remove_bus(&mut self.audio, bus)?;
        Ok(self.mixer_state())
    }

    /// Live, as [`Engine::update_channel`].
    pub(crate) fn set_send(&mut self, request: SetSendRequest) -> Result<MixerState, CommandError> {
        // The run mixes the configured graph: change both, the run first
        // (it checks the request the same way).
        if let Some(pipeline) = &mut self.pipeline {
            pipeline.set_send(&request)?;
        }
        audio::set_send(&mut self.audio, &request)?;
        Ok(self.mixer_state())
    }

    pub(crate) fn update_bus(
        &mut self,
        request: UpdateBusRequest,
    ) -> Result<MixerState, CommandError> {
        // The run mixes the configured graph: change both, the run first
        // (it checks the request the same way).
        if let Some(pipeline) = &mut self.pipeline {
            pipeline.update_bus(&request)?;
        }
        audio::update_bus(&mut self.audio, &request)?;
        Ok(self.mixer_state())
    }

    pub(crate) fn set_preview(&mut self, enabled: bool) -> Result<PreviewState, CommandError> {
        let mut state = PreviewState {
            enabled: self.preview_enabled,
            width: PREVIEW_WIDTH,
            height: PREVIEW_HEIGHT,
            fps: Rational::integer((u64::from(FPS) / PREVIEW_EVERY) as u32),
            frames_published: None,
            frames_dropped: None,
            pipeline: None,
            warnings: Vec::new(),
        };
        if enabled && !self.preview_enabled {
            self.ensure_pipeline()?;
            self.preview_enabled = true;
            if let Some(pipeline) = &self.pipeline {
                pipeline.set_preview(Some(Arc::clone(&self.preview_frames)));
                state.warnings.extend(pipeline.capture_warnings());
                state.warnings.extend(pipeline.audio_warnings());
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
            if pipeline.stopped {
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

/// A recording track's bus error, naming the entry of `tracks`
/// (`details.track_index`, from 0), since the mix takes `bus_id` and
/// `bus_name` too.
fn in_track(mut error: CommandError, index: usize) -> CommandError {
    error.message = format!("`tracks[{index}]`: {}", error.message);
    if let Some(Value::Object(details)) = &mut error.details {
        details.insert("track_index".to_owned(), index.into());
    }
    error
}
