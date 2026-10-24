//! Scene, mixer and preview commands. They change engine state; changes to
//! what the running pipeline renders or mixes apply live.

use std::sync::Arc;

use audio_engine::BusRef as GraphBus;
use command_api::{
    AddBusRequest, AddChannelRequest, AddSourceRequest, BusRef, ChannelRef, CommandError,
    ErrorCode, MixerState, OutputKind, OutputMix, OutputProfileSet, OutputProfiles, PROGRAM_BUS_ID,
    PreviewState, SceneList, SceneRef, SetChannelInputRequest, SetOutputProfileRequest,
    SetSendRequest, SourceKind, SourceRef, UpdateBusRequest, UpdateChannelRequest,
    UpdateSourceRequest,
};
use media_core::FrameMailbox;

use crate::composite::FPS;
use crate::outputs::{mix_column, output_id};
use crate::pipeline::{PREVIEW_EVERY, PREVIEW_HEIGHT, PREVIEW_WIDTH, Pipeline};
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
                    bus: bus.unwrap_or_else(|| self.profiles.mix(output).to_owned()),
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
        let bus_id = |bus, fields| -> Result<String, CommandError> {
            Ok(match audio::bus_position(&self.audio, bus, fields)? {
                GraphBus::Program => PROGRAM_BUS_ID.to_owned(),
                GraphBus::Aux(index) => self.audio.buses[index].id.clone(),
            })
        };
        let mut next = self.profiles.clone();
        let mix = request
            .mix
            .as_ref()
            .map(|bus| bus_id(bus, ("mix_bus_id", "mix_bus")))
            .transpose()?;
        if let Some(mix) = &mix {
            *next.mix_mut(output) = mix.clone();
        }
        if let Some(tracks) = &request.tracks {
            next.recording.tracks = tracks
                .iter()
                .map(|bus| bus_id(bus, ("tracks", "tracks")))
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
                            self.capture.as_mut(),
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
        if let GraphBus::Aux(index) = audio::bus_position(&self.audio, bus, ("id", "name"))? {
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
