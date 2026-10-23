//! The media pipeline shared by all consumers (outputs and the preview):
//! captures, compositor, mixer and a fan-out to the attached consumers.
//!
//! It renders the engine's active scene; scene edits apply live
//! ([`Pipeline::apply_scene`]). Outputs attach and detach while it runs.
//! Each attached output has its own copy pool, queue and epoch, so a
//! stalled output (a slow network) only drops its own frames, never another
//! output's. The preview gets quarter-resolution frames at half the frame
//! rate through a newest-wins mailbox.
//!
//! Timestamps: the producers report the nominal time of each frame/block on
//! the monotonic clock; the fan-out converts it to media time relative to
//! each output's epoch, so audio and video of an output share one timeline.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use audio_engine::{
    BLOCK_FRAMES, BlockSink, BusBlocks, CHANNELS, LiveMixStats, MixerConfig, MixerControl,
    MixerThread,
};
use capture_api::{
    AudioStream, CaptureBackend, CaptureErrorKind, CaptureState, ScreenSelection, VideoStream,
};
use command_api::{
    AudioGraphSpec, CaptureHealth, CaptureKind, CaptureOutcome, CommandError, ErrorCode,
    ErrorSummary, MixerState, PROGRAM_BUS_ID, PROGRAM_BUS_NAME, PipelineReport,
    SceneCaptureOutcome, SceneSpec, SetSendRequest, SourceKind, SourceSpec, UpdateBusRequest,
    UpdateChannelRequest,
};
use encoder_ffmpeg::{MAX_AUDIO_TRACKS, OutputInput};
use media_core::{AudioRing, FrameMailbox, FramePool, MediaTimestamp, VideoFrame};
use video_compositor::{
    Compositor, CompositorConfig, CpuRenderer, FrameSink, LiveRenderStats, downscale,
};

use crate::composite::{CANVAS_HEIGHT, CANVAS_WIDTH, FPS};
use crate::scenes::{self, FrameCache, LiveFrames};
use crate::{audio, command_error, mix};

/// Copies of rendered frames per output waiting for its encoder; beyond
/// this, that output's frames are dropped and counted.
const VIDEO_COPY_BUFFERS: usize = 8;
/// Recording and streaming.
const MAX_OUTPUTS: usize = 2;
/// Sizes the mixer's latency trend buffer (one entry per 10 s).
const EXPECTED_MAX_DURATION: Duration = Duration::from_secs(4 * 60 * 60);
/// Preview frames are a quarter of the canvas in each dimension...
pub(crate) const PREVIEW_FACTOR: u32 = 4;
/// ...at every second frame (30 FPS).
pub(crate) const PREVIEW_EVERY: u64 = 2;
/// The newest preview frame, one in the mailbox and one being drawn.
const PREVIEW_BUFFERS: usize = 3;
/// Capture outcomes of removed screen sources kept for the report (the most
/// recent ones; older ones are counted).
const MAX_RETIRED_CAPTURES: usize = 64;
/// How long finishing an output waits for producers running late.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
/// Frames and mixer blocks per second (both exact integers).
const FRAMES_PER_SECOND: i128 = FPS as i128;
const BLOCKS_PER_SECOND: i128 = (audio_engine::RATE as usize / audio_engine::BLOCK_FRAMES) as i128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputId {
    Recording,
    Streaming,
}

struct SceneCapture {
    /// The scene source's id.
    id: String,
    name: String,
    stream: Box<dyn VideoStream>,
}

/// A device channel's capture for one run.
enum DeviceCapture {
    Running(Box<dyn AudioStream>),
    /// The device was not found; the channel is silent for this run.
    Missing(ErrorSummary),
}

impl DeviceCapture {
    /// Start `device`'s capture. In the configured setup, a device that is
    /// not found leaves its channel silent (ADR 0017): a missing
    /// microphone must not keep a stream from starting. Every other error
    /// (no permission, no audio server), and any error for an output's own
    /// sources, which name their devices for this run, fails the start.
    fn start(
        capture: &mut dyn CaptureBackend,
        device: &Option<String>,
        from_setup: bool,
    ) -> Result<Self, CommandError> {
        match mix::start_device(capture, device) {
            Ok(stream) => Ok(Self::Running(stream)),
            Err(error) if from_setup && error.kind == CaptureErrorKind::NotFound => {
                Ok(Self::Missing(ErrorSummary::from(error)))
            }
            Err(error) => Err(command_error(error)),
        }
    }

    fn samples(&self) -> Option<Arc<AudioRing>> {
        match self {
            Self::Running(stream) => Some(stream.samples()),
            Self::Missing(_) => None,
        }
    }

    fn state(&self) -> CaptureState {
        match self {
            Self::Running(stream) => stream.state(),
            Self::Missing(error) => CaptureState::Ended {
                reason: format!("could not start: {}", error.message),
            },
        }
    }

    fn missing(&self) -> Option<&ErrorSummary> {
        match self {
            Self::Running(_) => None,
            Self::Missing(error) => Some(error),
        }
    }

    fn stop(self, device: &Option<String>) -> CaptureOutcome {
        match self {
            Self::Running(stream) => mix::stop_capture(device, stream),
            Self::Missing(error) => CaptureOutcome {
                input: device.clone(),
                report: None,
                error: Some(error),
            },
        }
    }
}

/// `device`'s name in a message.
fn device_label(device: &Option<String>) -> String {
    device.as_deref().map_or_else(
        || "the default input".to_owned(),
        |name| format!("`{name}`"),
    )
}

/// Field order is drop order: producers stop before the captures end.
pub(crate) struct Pipeline {
    compositor: Compositor,
    mixer: MixerThread,
    control: MixerControl,
    scene_screens: Vec<SceneCapture>,
    retired_screens: Vec<SceneCaptureOutcome>,
    retired_omitted: u64,
    extra_screen: Option<Box<dyn VideoStream>>,
    audio_streams: Vec<DeviceCapture>,
    /// What this run mixes; fader, mute and balance follow live changes.
    /// `audio_streams` are its device channels' captures, in order.
    audio: AudioGraphSpec,
    /// Whether `audio` is the engine's configured setup (rather than an
    /// output's own sources), so live gain changes apply to both.
    audio_from_setup: bool,
    cache: FrameCache,
    fanout: Arc<Fanout>,
}

impl Pipeline {
    pub fn start(
        capture: &mut dyn CaptureBackend,
        scene: &SceneSpec,
        audio: AudioGraphSpec,
        audio_from_setup: bool,
        extra_screen: Option<&ScreenSelection>,
    ) -> Result<Self, CommandError> {
        let extra_screen = match extra_screen {
            Some(selection) => Some(
                capture
                    .start_screen_stream(selection)
                    .map_err(command_error)?,
            ),
            None => None,
        };
        let scene_screens = start_screens(capture, scene, &[])?;
        // On an error, the captures already started are dropped, which
        // stops them.
        let audio_streams = audio::devices(&audio)
            .map(|(_, device)| DeviceCapture::start(capture, device, audio_from_setup))
            .collect::<Result<Vec<_>, _>>()?;
        let fanout = Arc::new(Fanout::default());
        let mut cache = FrameCache::default();
        let update = scenes::build(
            scene,
            &LiveFrames {
                screens: &mailboxes(&scene_screens),
                extra_screen: extra_screen.as_ref().map(|stream| stream.frames()).as_ref(),
            },
            &mut cache,
        )?;
        let compositor = Compositor::spawn(CompositorConfig {
            scene: update.scene,
            fps: FPS,
            sources: update.sources,
            animation: update.animation,
            renderer: CpuRenderer::with_available_parallelism(),
            sink: Some(Box::new(VideoTap {
                fanout: Arc::clone(&fanout),
                frame: 0,
            })),
        })
        .map_err(internal)?;
        let (aux_buses, sends) = audio::mixer_routing(&audio);
        let mixer = MixerThread::spawn(MixerConfig {
            inputs: mix::graph_inputs(&audio, audio_streams.iter().map(DeviceCapture::samples)),
            aux_buses,
            program: audio::program_config(&audio.program),
            sends,
            expected_duration: EXPECTED_MAX_DURATION,
            sink: Some(Box::new(AudioTap(Arc::clone(&fanout)))),
        })
        .map_err(internal)?;
        let control = mixer.control();
        Ok(Self {
            compositor,
            mixer,
            control,
            scene_screens,
            retired_screens: Vec::new(),
            retired_omitted: 0,
            extra_screen,
            audio_streams,
            audio,
            audio_from_setup,
            cache,
            fanout,
        })
    }

    /// `false` once the compositor or the mixer has stopped on its own.
    pub fn is_running(&self) -> bool {
        !self.compositor.is_finished() && !self.mixer.is_finished()
    }

    /// Compositor and mixer counters so far; never waits for their threads.
    pub fn live(&self) -> (LiveRenderStats, LiveMixStats) {
        (self.compositor.live(), self.control.live())
    }

    pub fn audio_from_setup(&self) -> bool {
        self.audio_from_setup
    }

    /// Render `scene` from the next frame on: start captures for its new
    /// screen sources (opens the picker), then swap the compositor's scene,
    /// then end captures of screen sources no longer in any scene (`keep`
    /// lists those that still exist, so switching back and forth between
    /// scenes does not ask again). Starting a picker source waits for the
    /// picker, which `allow_new_captures: false` refuses (open outputs must
    /// not stall behind a human); named sources start without one. On error
    /// nothing changes.
    pub fn apply_scene(
        &mut self,
        capture: &mut dyn CaptureBackend,
        scene: &SceneSpec,
        keep: &[String],
        allow_new_captures: bool,
    ) -> Result<(), CommandError> {
        if !allow_new_captures
            && let Some(source) = scene.sources.iter().find(|source| {
                matches!(
                    source.kind,
                    SourceKind::Screen {
                        source_id: None,
                        ..
                    }
                ) && !self
                    .scene_screens
                    .iter()
                    .any(|screen| screen.id == source.id)
            })
        {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                format!(
                    "screen source `{}` would open the system picker, which would block \
                     the engine while recording or streaming; add picker sources while only \
                     the preview runs, or name a source (`source_id`)",
                    source.name
                ),
            ));
        }
        let started = start_screens(capture, scene, &self.scene_screens)?;
        let mut live = mailboxes(&self.scene_screens);
        live.extend(mailboxes(&started));
        let extra = self.extra_screen.as_ref().map(|stream| stream.frames());
        let applied = scenes::build(
            scene,
            &LiveFrames {
                screens: &live,
                extra_screen: extra.as_ref(),
            },
            &mut self.cache,
        )
        .and_then(|update| self.compositor.update(update).map_err(internal));
        match applied {
            Ok(()) => {
                let mut screens = std::mem::take(&mut self.scene_screens);
                screens.extend(started);
                let (running, retire): (Vec<_>, Vec<_>) = screens
                    .into_iter()
                    .partition(|screen| keep.contains(&screen.id));
                self.scene_screens = running;
                for screen in retire {
                    self.retire(stop_scene_capture(screen));
                }
                Ok(())
            }
            Err(error) => {
                // The scene stays as it was; captures started for it end.
                for screen in started {
                    self.retire(stop_scene_capture(screen));
                }
                Err(error)
            }
        }
    }

    /// Start the running capture of screen source `source` again, then end
    /// the old one. A source of the rendered scene goes through the path of
    /// a scene change, so the compositor gets the new capture's frames; a
    /// kept source of another scene starts alone. On error the old capture
    /// keeps running (a new one that started first is stopped and reported).
    pub fn restart_screen(
        &mut self,
        capture: &mut dyn CaptureBackend,
        rendered: &SceneSpec,
        source: &SourceSpec,
        keep: &[String],
        allow_new_captures: bool,
    ) -> Result<(), CommandError> {
        let index = self
            .scene_screens
            .iter()
            .position(|screen| screen.id == source.id)
            .ok_or_else(|| {
                CommandError::new(
                    ErrorCode::InvalidState,
                    format!(
                        "`{}` has no running capture: its scene was not rendered since the \
                         pipeline started",
                        source.name
                    ),
                )
            })?;
        let picker = matches!(
            source.kind,
            SourceKind::Screen {
                source_id: None,
                ..
            }
        );
        if picker && !allow_new_captures {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                format!(
                    "restarting `{}` would open the system picker, which would block the engine \
                     while recording or streaming; restart it while only the preview runs, or \
                     pass a `source_id` from list_screen_sources",
                    source.name
                ),
            ));
        }
        let old = self.scene_screens.remove(index);
        let started = if rendered.sources.iter().any(|other| other.id == source.id) {
            self.apply_scene(capture, rendered, keep, allow_new_captures)
        } else {
            let alone = SceneSpec {
                id: String::new(),
                name: String::new(),
                sources: vec![source.clone()],
            };
            start_screens(capture, &alone, &[]).map(|started| self.scene_screens.extend(started))
        };
        match started {
            Ok(()) => {
                self.retire(stop_scene_capture(old));
                Ok(())
            }
            Err(error) => {
                self.scene_screens.insert(index, old);
                Err(error)
            }
        }
    }

    /// End the captures of sources that no scene has any more (`keep` lists
    /// the screen sources of every scene). A scene change does this as
    /// part of [`Pipeline::apply_scene`]; this is for edits that leave the
    /// rendered scene as it is, e.g. removing a kept source elsewhere.
    pub fn retire_unkept(&mut self, keep: &[String]) {
        let (running, retire): (Vec<_>, Vec<_>) = std::mem::take(&mut self.scene_screens)
            .into_iter()
            .partition(|screen| keep.contains(&screen.id));
        self.scene_screens = running;
        for screen in retire {
            self.retire(stop_scene_capture(screen));
        }
    }

    /// Whether scene source `id` has a capture (running or ended).
    pub fn has_scene_capture(&self, id: &str) -> bool {
        self.scene_screens.iter().any(|screen| screen.id == id)
    }

    /// State of every running capture; never blocks.
    pub fn captures(&self) -> Vec<CaptureHealth> {
        let screens = self.scene_screens.iter().map(|screen| CaptureHealth {
            kind: CaptureKind::Screen,
            id: Some(screen.id.clone()),
            name: screen.name.clone(),
            state: screen.stream.state(),
        });
        let layer = self.extra_screen.iter().map(|stream| CaptureHealth {
            kind: CaptureKind::Screen,
            id: None,
            name: "screen layer".to_owned(),
            state: stream.state(),
        });
        let audio =
            audio::devices(&self.audio)
                .zip(&self.audio_streams)
                .map(|((channel, _), capture)| CaptureHealth {
                    kind: CaptureKind::Audio,
                    id: Some(channel.id.clone()),
                    name: channel.name.clone(),
                    state: capture.state(),
                });
        screens.chain(layer).chain(audio).collect()
    }

    fn retire(&mut self, outcome: SceneCaptureOutcome) {
        if self.retired_screens.len() == MAX_RETIRED_CAPTURES {
            self.retired_screens.remove(0);
            self.retired_omitted += 1;
        }
        self.retired_screens.push(outcome);
    }

    /// Feed `input` from now on; returns the output's epoch (media time 0).
    /// Hand the output the frames and the mix of bus `mix` (see
    /// [`Pipeline::mix_column`]) from now on; returns its time zero.
    /// `tracks` are the columns of further audio tracks (a recording's).
    pub fn attach(
        &self,
        id: OutputId,
        input: OutputInput,
        mix: usize,
        tracks: Vec<usize>,
    ) -> Instant {
        let t0 = Instant::now();
        let progress = Arc::new(Progress::default());
        lock(&self.fanout.video).push(VideoOut {
            id,
            input: input.clone(),
            pool: FramePool::new(VIDEO_COPY_BUFFERS),
            t0,
            progress: Arc::clone(&progress),
        });
        lock(&self.fanout.audio).push(AudioOut {
            id,
            input,
            t0,
            progress,
            mix,
            tracks,
            fading_from: None,
            faded: vec![0.0; BLOCK_FRAMES * CHANNELS as usize],
        });
        t0
    }

    /// The id of the bus in `column`.
    fn bus_id(&self, column: usize) -> String {
        match column {
            0 => PROGRAM_BUS_ID.to_owned(),
            column => self.audio.buses[column - 1].id.clone(),
        }
    }

    /// The name of the bus in `column`.
    pub fn bus_name(&self, column: usize) -> String {
        match column {
            0 => PROGRAM_BUS_NAME.to_owned(),
            column => self.audio.buses[column - 1].name.clone(),
        }
    }

    /// The ids of output `id`'s further audio tracks.
    pub fn output_tracks(&self, id: OutputId) -> Vec<String> {
        lock(&self.fanout.audio)
            .iter()
            .find(|out| out.id == id)
            .map(|out| {
                out.tracks
                    .iter()
                    .map(|column| self.bus_id(*column))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The column of the bus with id `bus` in this run's mixer (the program
    /// bus, then the aux buses); `None` if the run has no such bus, e.g. a
    /// run with an output's own sources has no aux buses.
    pub fn mix_column(&self, bus: &str) -> Option<usize> {
        if bus == PROGRAM_BUS_ID {
            return Some(0);
        }
        self.audio
            .buses
            .iter()
            .position(|spec| spec.id == bus)
            .map(|index| index + 1)
    }

    /// The id of the bus output `id` takes now.
    pub fn output_mix(&self, id: OutputId) -> Option<String> {
        let column = lock(&self.fanout.audio)
            .iter()
            .find(|out| out.id == id)?
            .mix;
        Some(self.bus_id(column))
    }

    /// Let output `id` take bus `mix` from the next block on, fading over
    /// one block so nothing clicks. The fade starts from the bus the output
    /// last played, even when the mix changes twice within a block.
    pub fn set_output_mix(&self, id: OutputId, mix: usize) {
        if let Some(out) = lock(&self.fanout.audio).iter_mut().find(|out| out.id == id) {
            switch_mix(&mut out.mix, &mut out.fading_from, mix);
        }
    }

    /// Detach output `id` once both producers have handed it everything
    /// stamped before `end` (its recorder or streamer cuts at `end`), so
    /// both tracks end together even if a producer runs a few frames late.
    /// Waits at most [`DRAIN_TIMEOUT`]; frames and blocks still missing then
    /// are counted as dropped, never lost silently.
    pub fn finish_output(&self, id: OutputId, end: MediaTimestamp) {
        let end_ns = end.ticks;
        let found = lock(&self.fanout.video)
            .iter()
            .find(|out| out.id == id)
            .map(|out| (Arc::clone(&out.progress), out.input.clone()));
        if let Some((progress, input)) = found {
            let deadline = Instant::now() + DRAIN_TIMEOUT;
            while !progress.past(end_ns) && Instant::now() < deadline && self.is_running() {
                std::thread::sleep(Duration::from_millis(2));
            }
            let (video, audio) = progress.missing_before(end_ns);
            input.count_dropped(video, audio);
        }
        lock(&self.fanout.video).retain(|out| out.id != id);
        lock(&self.fanout.audio).retain(|out| out.id != id);
    }

    /// Start (`Some`) or stop (`None`) publishing preview frames. Stopping
    /// returns how many frames were published and dropped (no free buffer,
    /// e.g. a viewer holding frames).
    pub fn set_preview(&self, mailbox: Option<Arc<FrameMailbox>>) -> Option<(u64, u64)> {
        let previous = std::mem::replace(
            &mut *lock(&self.fanout.preview),
            mailbox.map(|mailbox| PreviewOut {
                mailbox,
                pool: FramePool::new(PREVIEW_BUFFERS),
                published: 0,
                dropped: 0,
            }),
        );
        previous.map(|out| (out.published, out.dropped))
    }

    /// The run's channels and buses with their levels, and its silent
    /// channels among the warnings.
    pub fn mixer_state(&self) -> MixerState {
        let mut state = audio::mixer_state(&self.audio, Some(&self.control.levels()));
        state.warnings.extend(self.audio_warnings());
        state
    }

    /// One warning per device channel that is silent because its device
    /// was not found.
    pub fn audio_warnings(&self) -> Vec<String> {
        audio::devices(&self.audio)
            .zip(&self.audio_streams)
            .filter_map(|((channel, device), capture)| {
                let error = capture.missing()?;
                Some(format!(
                    "channel `{}` is silent: {} was not found ({}); connect the device, or stop \
                     the outputs and preview and choose another input with set_channel_input",
                    channel.name,
                    device_label(device),
                    error.message
                ))
            })
            .collect()
    }

    /// Apply a fader, mute or balance change live.
    pub fn update_channel(&mut self, request: &UpdateChannelRequest) -> Result<(), CommandError> {
        let index = audio::update_channel(&mut self.audio, request)?;
        self.control
            .set_channel(index, audio::channel_config(&self.audio.channels[index]));
        Ok(())
    }

    /// Apply a bus change live.
    pub fn update_bus(&mut self, request: &UpdateBusRequest) -> Result<(), CommandError> {
        let bus = audio::update_bus(&mut self.audio, request)?;
        self.control
            .set_bus(bus, audio::config_of(&self.audio, bus));
        Ok(())
    }

    /// Apply a send change live (ramped; a tap change fades out and in).
    pub fn set_send(&mut self, request: &SetSendRequest) -> Result<(), CommandError> {
        let (channel, bus) = audio::set_send(&mut self.audio, request)?;
        self.control
            .set_send(channel, bus, audio::send_of(&self.audio, channel, bus));
        Ok(())
    }

    /// Stop producers, then captures; report everything.
    pub fn stop(self) -> (PipelineReport, Vec<String>) {
        let Self {
            compositor,
            mixer,
            control: _,
            scene_screens,
            mut retired_screens,
            retired_omitted,
            extra_screen,
            audio_streams,
            audio,
            audio_from_setup: _,
            cache: _,
            fanout: _,
        } = self;
        let mut warnings = Vec::new();
        let render = compositor
            .stop()
            .map(|run| run.stats)
            .map_err(|err| warnings.push(format!("compositor failed: {err}")))
            .ok();
        let mix = mixer
            .stop()
            .map_err(|err| warnings.push(format!("mixer failed: {err}")))
            .ok();
        let (screen_capture, screen_capture_error) = match extra_screen.map(|stream| stream.stop())
        {
            Some(Ok(report)) => (Some(report), None),
            Some(Err(error)) => (None, Some(ErrorSummary::from(error))),
            None => (None, None),
        };
        retired_screens.extend(scene_screens.into_iter().map(stop_scene_capture));
        // A missing device's channel was silent the whole run: say which.
        let (mut silent, mut failed) = (Vec::new(), Vec::new());
        let mut audio_captures = Vec::with_capacity(audio_streams.len());
        for ((channel, device), capture) in audio::devices(&audio).zip(audio_streams) {
            let missing = capture.missing().map(|error| {
                format!(
                    "channel `{}` was silent for the whole run: {} was not found ({})",
                    channel.name,
                    device_label(device),
                    error.message
                )
            });
            let outcome = capture.stop(device);
            match (missing, &outcome.error) {
                (Some(warning), _) => silent.push(warning),
                (None, Some(error)) => failed.push(format!("capture failed: {}", error.message)),
                (None, None) => {}
            }
            audio_captures.push(outcome);
        }

        if let Some(render) = &render
            && (render.late_frames > 0 || render.skipped_ticks > 0)
        {
            warnings.push(format!(
                "compositor missed its deadline: {} late frame(s), {} skipped tick(s)",
                render.late_frames, render.skipped_ticks
            ));
        }
        if let Some(mix) = &mix {
            warnings.extend(mix::mix_warnings(mix));
        }
        warnings.extend(silent);
        warnings.extend(failed);
        if let Some(error) = &screen_capture_error {
            warnings.push(format!("screen capture failed: {}", error.message));
        }
        for outcome in &retired_screens {
            if let Some(error) = &outcome.error {
                warnings.push(format!(
                    "screen capture of source `{}` failed: {}",
                    outcome.source, error.message
                ));
            }
        }
        let report = PipelineReport {
            pipeline_stopped: true,
            render,
            mix,
            screen_capture,
            screen_capture_error,
            audio_captures,
            scene_captures: retired_screens,
            scene_captures_omitted: retired_omitted,
        };
        (report, warnings)
    }
}

/// Start captures for the screen sources of `scene` not in `running`. On
/// error, the captures started by this call are stopped again.
fn start_screens(
    capture: &mut dyn CaptureBackend,
    scene: &SceneSpec,
    running: &[SceneCapture],
) -> Result<Vec<SceneCapture>, CommandError> {
    let mut started = Vec::new();
    for source in &scene.sources {
        let SourceKind::Screen {
            source_types,
            cursor,
            source_id,
        } = &source.kind
        else {
            continue;
        };
        if running.iter().any(|screen| screen.id == source.id) {
            continue;
        }
        let selection = ScreenSelection {
            source_kinds: source_types.clone(),
            cursor: *cursor,
            source: source_id.clone(),
        };
        let stream = capture.start_screen_stream(&selection).map_err(|error| {
            command_error(error).with_details(serde_json::json!({ "source": source.name }))
        })?;
        started.push(SceneCapture {
            id: source.id.clone(),
            name: source.name.clone(),
            stream,
        });
    }
    Ok(started)
}

fn mailboxes(screens: &[SceneCapture]) -> Vec<(String, Arc<FrameMailbox>)> {
    screens
        .iter()
        .map(|screen| (screen.id.clone(), screen.stream.frames()))
        .collect()
}

fn stop_scene_capture(screen: SceneCapture) -> SceneCaptureOutcome {
    match screen.stream.stop() {
        Ok(report) => SceneCaptureOutcome {
            id: screen.id,
            source: screen.name,
            report: Some(report),
            error: None,
        },
        Err(error) => SceneCaptureOutcome {
            id: screen.id,
            source: screen.name,
            report: None,
            error: Some(ErrorSummary::from(error)),
        },
    }
}

/// The attached consumers. The compositor and mixer threads lock their list
/// once per frame/block; the engine thread locks it only to attach or
/// detach (a push or removal, never while waiting on anything), so the
/// producers never wait on I/O. Capacity is reserved up front.
struct Fanout {
    video: Mutex<Vec<VideoOut>>,
    audio: Mutex<Vec<AudioOut>>,
    preview: Mutex<Option<PreviewOut>>,
}

impl Default for Fanout {
    fn default() -> Self {
        Self {
            video: Mutex::new(Vec::with_capacity(MAX_OUTPUTS)),
            audio: Mutex::new(Vec::with_capacity(MAX_OUTPUTS)),
            preview: Mutex::new(None),
        }
    }
}

struct VideoOut {
    id: OutputId,
    input: OutputInput,
    pool: FramePool,
    t0: Instant,
    progress: Arc<Progress>,
}

struct AudioOut {
    id: OutputId,
    input: OutputInput,
    t0: Instant,
    progress: Arc<Progress>,
    /// The column of the bus it takes (program first).
    mix: usize,
    /// The columns of its further audio tracks; fixed.
    tracks: Vec<usize>,
    /// For one block after the mix changed: the bus it fades from.
    fading_from: Option<usize>,
    /// Preallocated block for that fade.
    faded: Vec<f32>,
}

/// Media time (ns) of the latest frame and block handed to an output.
struct Progress {
    video: AtomicI64,
    audio: AtomicI64,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            video: AtomicI64::new(-1),
            audio: AtomicI64::new(-1),
        }
    }
}

impl Progress {
    /// Both producers handed over something stamped at or after `end_ns`.
    fn past(&self, end_ns: i64) -> bool {
        self.video.load(Ordering::Acquire) >= end_ns && self.audio.load(Ordering::Acquire) >= end_ns
    }

    /// Frames and blocks stamped before `end_ns` that were never handed over.
    /// Producers stamp exact multiples of their period, so indices follow
    /// from the rates without accumulated rounding.
    fn missing_before(&self, end_ns: i64) -> (u64, u64) {
        const NS: i128 = 1_000_000_000;
        let missing = |handled: i64, per_second: i128| {
            // Items before the end: ceil(end * rate / 1 s).
            let before_end = (i128::from(end_ns) * per_second + NS - 1).div_euclid(NS);
            // Index of the next item owed: after the one handed over last.
            let next = if handled < 0 {
                0
            } else {
                (i128::from(handled) * per_second + NS / 2).div_euclid(NS) + 1
            };
            u64::try_from(before_end - next).unwrap_or(0)
        };
        (
            missing(self.video.load(Ordering::Acquire), FRAMES_PER_SECOND),
            missing(self.audio.load(Ordering::Acquire), BLOCKS_PER_SECOND),
        )
    }
}

struct PreviewOut {
    mailbox: Arc<FrameMailbox>,
    pool: FramePool,
    published: u64,
    dropped: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking producer leaves a consistent list behind.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Copies each rendered frame into a pooled buffer per output, and a
/// downscaled one for the preview.
struct VideoTap {
    fanout: Arc<Fanout>,
    frame: u64,
}

impl FrameSink for VideoTap {
    fn frame(&mut self, frame: &VideoFrame, presented_at: Instant) {
        for out in lock(&self.fanout.video).iter_mut() {
            // Frames from before the output started are not part of it.
            let Some(pts) = since(out.t0, presented_at) else {
                continue;
            };
            out.progress.video.store(pts.ticks, Ordering::Release);
            let Some(mut buffer) = out.pool.acquire(frame.data().len()) else {
                out.input.count_dropped_video();
                continue;
            };
            buffer.copy_from_slice(frame.data());
            match VideoFrame::new(
                frame.width(),
                frame.height(),
                frame.stride(),
                frame.format(),
                None,
                buffer,
            ) {
                Ok(copy) => {
                    out.input.send_video(copy, pts);
                }
                Err(_) => out.input.count_dropped_video(),
            }
        }
        self.frame += 1;
        if !self.frame.is_multiple_of(PREVIEW_EVERY) {
            return;
        }
        if let Some(preview) = lock(&self.fanout.preview).as_mut() {
            let len = (frame.width() / PREVIEW_FACTOR) as usize
                * (frame.height() / PREVIEW_FACTOR) as usize
                * 4;
            // No free buffer means the viewer holds frames; the preview is
            // best effort, but drops are counted.
            match preview
                .pool
                .acquire(len)
                .map(|buffer| downscale(frame, PREVIEW_FACTOR, buffer))
            {
                Some(Ok(small)) => {
                    preview.mailbox.publish(small);
                    preview.published += 1;
                }
                _ => preview.dropped += 1,
            }
        }
    }
}

/// Forwards each mixed block to every output; does not allocate.
struct AudioTap(Arc<Fanout>);

impl BlockSink for AudioTap {
    fn block(&mut self, buses: &BusBlocks<'_>, at: Instant) {
        for out in lock(&self.0.audio).iter_mut() {
            let Some(pts) = since(out.t0, at) else {
                continue;
            };
            let mix = buses.column(out.mix).unwrap_or(buses.program());
            let samples = match out.fading_from.take().and_then(|from| buses.column(from)) {
                Some(from) => {
                    crossfade(from, mix, &mut out.faded);
                    out.faded.as_slice()
                }
                None => mix,
            };
            // Track 1 is the mix; one item carries every track.
            let mut tracks: [&[f32]; MAX_AUDIO_TRACKS] = [&[]; MAX_AUDIO_TRACKS];
            tracks[0] = samples;
            for (slot, column) in tracks[1..].iter_mut().zip(&out.tracks) {
                *slot = buses.column(*column).unwrap_or(buses.program());
            }
            out.input
                .send_audio_tracks(&tracks[..1 + out.tracks.len()], pts);
            out.progress.audio.store(pts.ticks, Ordering::Release);
        }
    }
}

/// Take bus `to` from the next block on, fading from the bus last played:
/// a fade still pending keeps its start, and going back to that bus needs
/// none.
fn switch_mix(mix: &mut usize, fading_from: &mut Option<usize>, to: usize) {
    if *mix == to {
        return;
    }
    *fading_from = match *fading_from {
        None => Some(*mix),
        Some(from) if from == to => None,
        pending => pending,
    };
    *mix = to;
}

/// `to` faded in over `from` across one interleaved stereo block, linearly
/// from frame to frame, into `out`.
fn crossfade(from: &[f32], to: &[f32], out: &mut [f32]) {
    let channels = CHANNELS as usize;
    let frames = out.len() / channels;
    for (frame, ((out, from), to)) in out
        .chunks_exact_mut(channels)
        .zip(from.chunks_exact(channels))
        .zip(to.chunks_exact(channels))
        .enumerate()
    {
        let t = (frame + 1) as f32 / frames as f32;
        for ((out, from), to) in out.iter_mut().zip(from).zip(to) {
            *out = from * (1.0 - t) + to * t;
        }
    }
}

/// Media time of `at` on the timeline starting at `t0`, in nanoseconds;
/// `None` before `t0`. Saturates after about 292 years.
pub(crate) fn since(t0: Instant, at: Instant) -> Option<MediaTimestamp> {
    let elapsed = at.checked_duration_since(t0)?;
    Some(MediaTimestamp {
        ticks: i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX),
        time_base_num: 1,
        time_base_den: 1_000_000_000,
    })
}

/// Preview frame size.
pub(crate) const PREVIEW_WIDTH: u32 = CANVAS_WIDTH / PREVIEW_FACTOR;
pub(crate) const PREVIEW_HEIGHT: u32 = CANVAS_HEIGHT / PREVIEW_FACTOR;

fn internal(error: impl std::fmt::Display) -> CommandError {
    CommandError::new(
        ErrorCode::Internal,
        format!("media pipeline failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mix_change_fades_from_the_bus_last_played() {
        let (mut mix, mut from) = (0, None);
        switch_mix(&mut mix, &mut from, 1);
        assert_eq!((mix, from), (1, Some(0)));
        // Again within the block: still from the bus that played.
        switch_mix(&mut mix, &mut from, 2);
        assert_eq!((mix, from), (2, Some(0)));
        // Back to it: no fade at all.
        switch_mix(&mut mix, &mut from, 0);
        assert_eq!((mix, from), (0, None));
        // The same bus changes nothing.
        switch_mix(&mut mix, &mut from, 0);
        assert_eq!((mix, from), (0, None));
    }

    #[test]
    fn a_mix_change_fades_linearly_over_one_block() {
        // Two frames of stereo: from 1.0 to 0.0.
        let (from, to) = ([1.0; 4], [0.0; 4]);
        let mut out = [9.0; 4];
        crossfade(&from, &to, &mut out);
        assert_eq!(out, [0.5, 0.5, 0.0, 0.0]);
        // Over a whole block, it ends on the new mix and never overshoots.
        let len = BLOCK_FRAMES * CHANNELS as usize;
        let (from, to) = (vec![0.25; len], vec![1.0; len]);
        let mut out = vec![0.0; len];
        crossfade(&from, &to, &mut out);
        assert_eq!(out[len - 1], 1.0);
        assert!(out.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(out[0] > 0.25);
    }

    #[test]
    fn missing_media_before_the_end_is_counted_exactly() {
        let progress = Progress::default();
        let ms = |value: i64| value * 1_000_000;
        // Nothing handed over yet: every frame (0, 16.7, 33.3 ms) and block
        // (0..40 ms) before 50 ms is missing.
        assert_eq!(progress.missing_before(ms(50)), (3, 5));
        progress.video.store(33_333_333, Ordering::Release);
        progress.audio.store(ms(20), Ordering::Release);
        // The frame at 33.3 ms is the last before 50 ms: nothing missing;
        // blocks at 30 and 40 ms are.
        assert_eq!(progress.missing_before(ms(50)), (0, 2));
        assert!(!progress.past(ms(50)));
        progress.video.store(ms(50), Ordering::Release);
        progress.audio.store(ms(50), Ordering::Release);
        assert!(progress.past(ms(50)));
        assert_eq!(progress.missing_before(ms(50)), (0, 0));
    }
}
