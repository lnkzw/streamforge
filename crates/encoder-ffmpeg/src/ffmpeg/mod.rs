//! FFmpeg implementation of the output threads: encoder tracks and
//! containers shared by the recorder and the streamer.
//!
//! All FFmpeg objects of an output are created and used on its thread.

use std::sync::{Arc, Mutex, PoisonError};

use ff::format::Pixel;
use ff::format::Sample;
use ff::format::sample::Type as SampleType;
use ff::util::color;
use ff::{ChannelLayout, Dictionary, Packet, Rational, codec, encoder, format, frame, software};
use ffmpeg_next as ff;
use media_core::{MediaTimestamp, VideoFrame};

use crate::audio_fifo::AudioFifo;
use crate::{
    AudioSettings, AudioTrackStats, Counts, EncoderPreference, Item, RecorderError,
    RecorderErrorKind, Shared, SkippedEncoder, VideoEncoderInfo, VideoSettings, VideoTrackStats,
};

pub(crate) mod encoders;
mod interrupt;
pub(crate) mod record;
pub(crate) mod stream;
#[cfg(target_os = "linux")]
mod vaapi;

use interrupt::Interrupt;

/// Once a stream was opened, FFmpeg's own log output stays off: RTMP error
/// messages can contain the stream path, i.e. the key. The lock makes the
/// check and the switch one step, so a concurrent recorder start cannot turn
/// logging back on after a streamer turned it off.
static QUIET: Mutex<bool> = Mutex::new(false);

/// Mutes FFmpeg's log output while it lives: trying hardware encoders
/// fails noisily where a GPU, driver or runtime is missing, and those
/// failures are reported in `skipped`. The level comes back when the guard
/// drops (also on a panic), unless a streamer has turned logging off for
/// good meanwhile. No lock is held while the guard lives; other outputs'
/// FFmpeg messages are muted for those few moments too.
struct Muted;

impl Muted {
    fn new() -> Self {
        ff::log::set_level(ff::log::Level::Quiet);
        Self
    }
}

impl Drop for Muted {
    fn drop(&mut self) {
        let stays_quiet = QUIET.lock().unwrap_or_else(PoisonError::into_inner);
        if !*stays_quiet {
            ff::log::set_level(ff::log::Level::Error);
        }
    }
}

/// The order in which an output tries encoders. The one that opened moves
/// to the front, so a stream's reconnects try it first and still fall back
/// to the others if it stops working.
pub(crate) struct EncoderChoice {
    candidates: Vec<encoders::Candidate>,
}

impl EncoderChoice {
    pub(crate) fn new(settings: &VideoSettings) -> Self {
        Self {
            candidates: encoders::candidates(settings.encoders),
        }
    }

    fn prefer(&mut self, chosen: encoders::Candidate) {
        self.candidates.retain(|candidate| *candidate != chosen);
        self.candidates.insert(0, chosen);
    }
}

fn init(quiet: bool) -> Result<(), RecorderError> {
    ff::init()
        .map_err(|err| error(RecorderErrorKind::Internal, "cannot initialize FFmpeg", err))?;
    let mut stays_quiet = QUIET.lock().unwrap_or_else(PoisonError::into_inner);
    *stays_quiet |= quiet;
    ff::log::set_level(if *stays_quiet {
        ff::log::Level::Quiet
    } else {
        // FFmpeg logs to stderr; keep it to real errors.
        ff::log::Level::Error
    });
    Ok(())
}

/// One open container with its two encoded tracks. Field order is drop
/// order: the output (whose I/O may still call the interrupt callback while
/// closing) goes before the interrupt state it points to.
struct Session {
    output: format::context::Output,
    video: VideoTrack,
    /// Audio track 1.
    audio: AudioTrack,
    /// Audio tracks 2 and on; their samples come in each item's
    /// [`crate::TrackBlock`].
    extra: Vec<AudioTrack>,
    interrupt: Arc<Interrupt>,
}

impl Session {
    /// Open `url` with the muxer `container` (FFmpeg protocol `options`, e.g.
    /// timeouts), add the video track and the audio tracks (one per name in
    /// `tracks`, or one unnamed track; tracks after the first read their
    /// samples from `pool`) and write the header. Errors name the output as
    /// `display` (never `url`, which may hold a secret) and use `kind` for
    /// failures to open or write the output.
    #[allow(clippy::too_many_arguments)]
    fn open(
        url: &str,
        container: &str,
        options: Dictionary,
        display: &str,
        kind: RecorderErrorKind,
        video: &VideoSettings,
        encoders: &mut EncoderChoice,
        (settings, tracks): (&AudioSettings, &[String]),
        shared: Arc<Shared>,
    ) -> Result<Self, RecorderError> {
        let interrupt = Interrupt::new(shared);
        let mut output = interrupt::open_output(url, container, options, &interrupt)
            .map_err(|err| error(kind, &format!("cannot open {display}"), err))?;
        let global_header = output
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);
        let mut video = VideoTrack::open(&mut output, video, encoders, global_header)?;
        let mut audio = AudioTrack::open(
            &mut output,
            settings,
            tracks.first().cloned(),
            global_header,
        )?;
        let mut extra = tracks
            .iter()
            .skip(1)
            .map(|name| AudioTrack::open(&mut output, settings, Some(name.clone()), global_header))
            .collect::<Result<Vec<_>, _>>()?;
        output
            .write_header()
            .map_err(|err| error(kind, &format!("cannot start {display}"), err))?;
        // The muxer may pick its own stream time bases while writing the header.
        video.stream_time_base = stream_time_base(&output, video.stream);
        audio.stream_time_base = stream_time_base(&output, audio.stream);
        for track in &mut extra {
            track.stream_time_base = stream_time_base(&output, track.stream);
        }
        Ok(Self {
            output,
            video,
            audio,
            extra,
            interrupt,
        })
    }

    /// Make the session's blocked and future I/O fail at once (the peer is
    /// gone; do not wait for timeouts while closing).
    fn abort(&self) {
        self.interrupt.abort();
    }

    /// This session's totals so far.
    fn counts(&self) -> Counts {
        counts_of(&self.video, &self.audio)
    }

    /// Encode one item; items stamped at or after `end_ns` are counted as
    /// after the end. The stop marker is ignored (the caller handles it).
    fn write(&mut self, item: Item, end_ns: i64) -> Result<(), RecorderError> {
        match item {
            Item::Video { frame, pts } => self.video.encode(&mut self.output, &frame, pts, end_ns),
            Item::Audio {
                samples,
                len,
                pts,
                extra,
            } => {
                let first = self
                    .audio
                    .push(&mut self.output, &samples[..len], pts, end_ns);
                // The block owns its buffer: nothing is locked while the
                // tracks encode, and it goes back to its pool when dropped.
                let rest =
                    extra.map_or(Ok(()), |block| {
                        self.extra.iter_mut().zip(block.tracks()).try_for_each(
                            |(track, samples)| track.push(&mut self.output, samples, pts, end_ns),
                        )
                    });
                first.and(rest)
            }
            Item::Stop => Ok(()),
        }
    }

    /// Flush every track even if one fails, then write the trailer and
    /// close the output; the first error wins. Returns the video track,
    /// audio track 1 and the further audio tracks.
    fn finish(
        mut self,
    ) -> (
        VideoTrack,
        AudioTrack,
        Vec<AudioTrack>,
        Result<(), RecorderError>,
    ) {
        let video_flushed = self.video.flush(&mut self.output);
        let audio_flushed = self.audio.flush(&mut self.output);
        let mut extra_flushed = Ok(());
        for track in &mut self.extra {
            let flushed = track.flush(&mut self.output);
            extra_flushed = extra_flushed.and(flushed);
        }
        let trailer = self
            .output
            .write_trailer()
            .map_err(|err| error(RecorderErrorKind::Output, "cannot finalize the output", err));
        drop(self.output);
        drop(self.interrupt);
        let result = video_flushed
            .and(audio_flushed)
            .and(extra_flushed)
            .and(trailer);
        (self.video, self.audio, self.extra, result)
    }
}

fn stream_time_base(output: &format::context::Output, index: usize) -> Rational {
    output
        .stream(index)
        .map_or(Rational::new(1, 1000), |stream| stream.time_base())
}

fn error(kind: RecorderErrorKind, context: &str, err: ff::Error) -> RecorderError {
    RecorderError::new(kind, format!("{context}: {err}"))
}

/// Receive all pending packets from `encoder` and write them interleaved.
/// "Needs more input" and "fully flushed" end the loop; anything else is an
/// encoder error.
fn drain(
    encoder: &mut encoder::Encoder,
    output: &mut format::context::Output,
    stream: usize,
    encoder_time_base: Rational,
    stream_time_base: Rational,
    written: &mut u64,
) -> Result<(), RecorderError> {
    let mut packet = Packet::empty();
    loop {
        match encoder.receive_packet(&mut packet) {
            Ok(()) => {}
            Err(ff::Error::Eof) => return Ok(()),
            Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => return Ok(()),
            Err(err) => {
                return Err(error(
                    RecorderErrorKind::Encoder,
                    "the encoder failed to produce a packet",
                    err,
                ));
            }
        }
        packet.set_stream(stream);
        packet.rescale_ts(encoder_time_base, stream_time_base);
        packet
            .write_interleaved(output)
            .map_err(|err| error(RecorderErrorKind::Output, "cannot write a packet", err))?;
        *written += 1;
    }
}

/// Whether `pts` is at or after the requested end (`end_ns`).
fn after_end(pts: MediaTimestamp, end_ns: i64) -> bool {
    pts.to_nanoseconds()
        .is_ok_and(|ns| ns >= i128::from(end_ns))
}

/// A positive `u32` setting as FFmpeg's `int`.
fn positive_i32(value: u32, what: &str) -> Result<i32, RecorderError> {
    i32::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            RecorderError::new(
                RecorderErrorKind::Unsupported,
                format!("{what} {value} is out of range"),
            )
        })
}

fn frames_i64(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// Totals of a session's tracks (also after the session was finished).
fn counts_of(video: &VideoTrack, audio: &AudioTrack) -> Counts {
    let (frames_encoded, frames_dropped) = video.counts();
    let (samples_encoded, blocks_dropped) = audio.counts();
    Counts {
        frames_encoded,
        frames_dropped,
        samples_encoded,
        blocks_dropped,
    }
}

struct VideoTrack {
    encoder: encoder::Video,
    name: String,
    hardware: bool,
    skipped: Vec<SkippedEncoder>,
    settings: VideoSettings,
    /// What the scaler produces and the encoder (or upload) takes.
    picture_format: Pixel,
    /// VAAPI surfaces when the encoder needs them (`h264_vaapi`).
    #[cfg(target_os = "linux")]
    surfaces: Option<vaapi::Surfaces>,
    stream: usize,
    time_base: Rational,
    stream_time_base: Rational,
    scaler: software::scaling::Context,
    /// Reused BGRA staging frame; only the scaler reads it.
    staging: frame::Video,
    frames_encoded: u64,
    pub(super) frames_dropped: u64,
    frames_after_end: u64,
    /// Packets handed to the muxer without error.
    packets_written: u64,
    first_pts: Option<i64>,
    last_pts: Option<i64>,
}

/// The codec a candidate names (`ANY_H264`: FFmpeg's default H.264 encoder).
fn find_codec(candidate: encoders::Candidate) -> Option<ff::Codec> {
    if candidate.name == encoders::ANY_H264 {
        encoder::find(codec::Id::H264)
    } else {
        encoder::find_by_name(candidate.name)
    }
}

/// Opening alone does not prove an encoder works: some drivers accept the
/// session and reject the first frame. So a throwaway instance encodes one
/// frame first; only then is the real one opened.
fn try_encoder(
    codec: ff::Codec,
    candidate: encoders::Candidate,
    settings: &VideoSettings,
    fps: i32,
    global_header: bool,
) -> Result<Opened, String> {
    let mut probe = open_encoder(codec, candidate, settings, fps, global_header)?;
    let mut picture = frame::Video::new(probe.picture_format, settings.width, settings.height);
    #[cfg(target_os = "linux")]
    if let Some(surfaces) = &probe.surfaces {
        picture = surfaces.upload(&picture)?;
    }
    picture.set_pts(Some(0));
    probe
        .encoder
        .send_frame(&picture)
        .and_then(|()| probe.encoder.send_eof())
        .map_err(|err| format!("rejects frames: {err}"))?;
    let mut packet = Packet::empty();
    loop {
        match probe.encoder.receive_packet(&mut packet) {
            Ok(()) => {}
            Err(ff::Error::Eof) => break,
            Err(err) => return Err(format!("rejects frames: {err}")),
        }
    }
    drop(probe);
    open_encoder(codec, candidate, settings, fps, global_header)
}

/// Open one candidate; the error says why it cannot be used here.
fn open_encoder(
    codec: ff::Codec,
    candidate: encoders::Candidate,
    settings: &VideoSettings,
    fps: i32,
    global_header: bool,
) -> Result<Opened, String> {
    let mut context = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(|err| format!("cannot create the encoder: {err}"))?;
    context.set_width(settings.width);
    context.set_height(settings.height);
    context.set_time_base(Rational::new(1, fps));
    context.set_frame_rate(Some(Rational::new(fps, 1)));
    // Tag what the conversion produces: swscale's default BGRA to YUV
    // matrix is BT.601 with limited range. The canvas is sRGB (BT.709
    // primaries, sRGB transfer; ADR 0003). Untagged HD video would be shown
    // with the BT.709 matrix, shifting hues.
    context.set_colorspace(color::Space::SMPTE170M);
    context.set_color_range(color::Range::MPEG);
    context.set_color_primaries(color::Primaries::BT709);
    if global_header {
        context.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    #[cfg(target_os = "linux")]
    let mut surfaces = None;
    let picture_format = match candidate.upload {
        encoders::Upload::Software => {
            let formats: Vec<Pixel> = codec
                .video()
                .ok()
                .and_then(|video| video.formats())
                .map(Iterator::collect)
                .unwrap_or_default();
            let picture = if formats.is_empty() || formats.contains(&Pixel::YUV420P) {
                Pixel::YUV420P
            } else if formats.contains(&Pixel::NV12) {
                Pixel::NV12
            } else {
                return Err("takes neither YUV 4:2:0 nor NV12 frames".to_owned());
            };
            context.set_format(picture);
            picture
        }
        #[cfg(target_os = "linux")]
        encoders::Upload::Vaapi => {
            let created = vaapi::Surfaces::new(settings.width, settings.height)?;
            context.set_format(Pixel::VAAPI);
            created.attach(&mut context)?;
            surfaces = Some(created);
            Pixel::NV12
        }
        #[cfg(not(target_os = "linux"))]
        encoders::Upload::Vaapi => return Err("VAAPI exists on Linux only".to_owned()),
    };
    let mut options = Dictionary::new();
    // No typed setter for the transfer in ffmpeg-next; it is a codec
    // context option.
    options.set("color_trc", "iec61966-2-1");
    for (key, value) in encoders::options(codec.name(), settings.rate, settings.fps) {
        options.set(key, &value);
    }
    let encoder = context
        .open_with(options)
        .map_err(|err| format!("cannot open: {err}"))?;
    Ok(Opened {
        encoder,
        codec,
        picture_format,
        #[cfg(target_os = "linux")]
        surfaces,
    })
}

/// One opened encoder of the candidates.
struct Opened {
    encoder: encoder::Video,
    codec: ff::Codec,
    picture_format: Pixel,
    #[cfg(target_os = "linux")]
    surfaces: Option<vaapi::Surfaces>,
}

impl VideoTrack {
    /// Open the first candidate encoder that works and add its stream.
    fn open(
        output: &mut format::context::Output,
        settings: &VideoSettings,
        choice: &mut EncoderChoice,
        global_header: bool,
    ) -> Result<Self, RecorderError> {
        let fps = positive_i32(settings.fps, "frame rate")?;
        let time_base = Rational::new(1, fps);
        let mut skipped: Vec<SkippedEncoder> = Vec::new();
        let mut opened = None;
        for candidate in &choice.candidates {
            let Some(codec) = find_codec(*candidate) else {
                if candidate.name != encoders::ANY_H264 {
                    skipped.push(SkippedEncoder {
                        encoder: candidate.name.to_owned(),
                        reason: "not in this FFmpeg build".to_owned(),
                    });
                }
                continue;
            };
            // FFmpeg's default H.264 encoder is usually one tried already.
            if skipped.iter().any(|skip| skip.encoder == codec.name()) {
                continue;
            }
            let attempt = if candidate.hardware {
                let _muted = Muted::new();
                try_encoder(codec, *candidate, settings, fps, global_header)
            } else {
                try_encoder(codec, *candidate, settings, fps, global_header)
            };
            match attempt {
                Ok(encoder) => {
                    opened = Some((*candidate, encoder));
                    break;
                }
                Err(reason) => skipped.push(SkippedEncoder {
                    encoder: codec.name().to_owned(),
                    reason,
                }),
            }
        }
        let Some((candidate, opened)) = opened else {
            let tried: Vec<String> = skipped
                .iter()
                .map(|skip| format!("{}: {}", skip.encoder, skip.reason))
                .collect();
            let kind = match settings.encoders {
                EncoderPreference::Hardware => "hardware ",
                EncoderPreference::Software => "software ",
                EncoderPreference::Auto => "",
            };
            return Err(RecorderError::new(
                RecorderErrorKind::Unsupported,
                format!("no {kind}H.264 encoder opened ({})", tried.join("; ")),
            ));
        };
        choice.prefer(candidate);
        let stream = {
            let mut stream = output.add_stream(opened.codec).map_err(|err| {
                error(
                    RecorderErrorKind::Output,
                    "cannot add the video stream",
                    err,
                )
            })?;
            stream.set_parameters(&opened.encoder);
            stream.set_time_base(time_base);
            stream.index()
        };
        let scaler = software::scaling::Context::get(
            Pixel::BGRA,
            settings.width,
            settings.height,
            opened.picture_format,
            settings.width,
            settings.height,
            software::scaling::Flags::BILINEAR,
        )
        .map_err(|err| {
            error(
                RecorderErrorKind::Encoder,
                "cannot create the color converter",
                err,
            )
        })?;
        Ok(Self {
            name: opened.codec.name().to_owned(),
            encoder: opened.encoder,
            hardware: candidate.hardware,
            skipped,
            settings: settings.clone(),
            picture_format: opened.picture_format,
            #[cfg(target_os = "linux")]
            surfaces: opened.surfaces,
            stream,
            time_base,
            stream_time_base: time_base,
            scaler,
            staging: frame::Video::new(Pixel::BGRA, settings.width, settings.height),
            frames_encoded: 0,
            frames_dropped: 0,
            frames_after_end: 0,
            packets_written: 0,
            first_pts: None,
            last_pts: None,
        })
    }

    fn encode(
        &mut self,
        output: &mut format::context::Output,
        source: &VideoFrame,
        pts: MediaTimestamp,
        end_ns: i64,
    ) -> Result<(), RecorderError> {
        if after_end(pts, end_ns) {
            self.frames_after_end += 1;
            return Ok(());
        }
        let Ok(pts) = pts.rescale(1, self.settings.fps) else {
            self.frames_dropped += 1;
            return Ok(());
        };
        let wrong_size =
            source.width() != self.settings.width || source.height() != self.settings.height;
        if wrong_size || self.last_pts.is_some_and(|last| pts <= last) {
            self.frames_dropped += 1;
            return Ok(());
        }
        let stride = self.staging.stride(0);
        let data = self.staging.data_mut(0);
        for y in 0..source.height() {
            let row = source.row(y);
            data[y as usize * stride..][..row.len()].copy_from_slice(row);
        }
        // A fresh frame per picture: the encoder may keep references to it
        // (lookahead), so a reused buffer could be overwritten too early.
        let mut picture = frame::Video::new(
            self.picture_format,
            self.settings.width,
            self.settings.height,
        );
        self.scaler
            .run(&self.staging, &mut picture)
            .map_err(|err| error(RecorderErrorKind::Encoder, "color conversion failed", err))?;
        #[cfg(target_os = "linux")]
        if let Some(surfaces) = &self.surfaces {
            picture = surfaces
                .upload(&picture)
                .map_err(|reason| RecorderError::new(RecorderErrorKind::Encoder, reason))?;
        }
        picture.set_pts(Some(pts));
        self.encoder
            .send_frame(&picture)
            .map_err(|err| error(RecorderErrorKind::Encoder, "video encoding failed", err))?;
        self.frames_encoded += 1;
        self.first_pts.get_or_insert(pts);
        self.last_pts = Some(pts);
        drain(
            &mut self.encoder,
            output,
            self.stream,
            self.time_base,
            self.stream_time_base,
            &mut self.packets_written,
        )
    }

    fn flush(&mut self, output: &mut format::context::Output) -> Result<(), RecorderError> {
        // Nothing to flush; and FFmpeg 8's h264_vaapi crashes when it is
        // flushed without ever having received a frame.
        if self.frames_encoded == 0 {
            return Ok(());
        }
        self.encoder.send_eof().map_err(|err| {
            error(
                RecorderErrorKind::Encoder,
                "cannot flush the video encoder",
                err,
            )
        })?;
        drain(
            &mut self.encoder,
            output,
            self.stream,
            self.time_base,
            self.stream_time_base,
            &mut self.packets_written,
        )
    }

    fn pts_ms(&self, pts: i64) -> f64 {
        pts as f64 * 1000.0 / f64::from(self.settings.fps)
    }

    fn end_ms(&self) -> f64 {
        self.last_pts.map_or(0.0, |pts| self.pts_ms(pts + 1))
    }

    /// Frames accepted by the encoder that never reached the muxer.
    fn undelivered(&self) -> u64 {
        self.frames_encoded.saturating_sub(self.packets_written)
    }

    /// Frames encoded and dropped by this track so far.
    fn counts(&self) -> (u64, u64) {
        (self.frames_encoded, self.frames_dropped)
    }

    fn info(&self) -> VideoEncoderInfo {
        VideoEncoderInfo {
            encoder: self.name.clone(),
            hardware: self.hardware,
            skipped: self.skipped.clone(),
        }
    }

    fn stats(&self, queue_dropped: u64) -> VideoTrackStats {
        VideoTrackStats {
            encoder: self.info(),
            width: self.settings.width,
            height: self.settings.height,
            fps: media_core::Rational::integer(self.settings.fps),
            frames_encoded: self.frames_encoded,
            frames_dropped: self.frames_dropped + queue_dropped,
            frames_after_end: self.frames_after_end,
            first_pts_ms: self.first_pts.map(|pts| self.pts_ms(pts)),
            end_ms: self.last_pts.map(|pts| self.pts_ms(pts + 1)),
        }
    }
}

struct AudioTrack {
    encoder: encoder::Audio,
    name: String,
    /// The track's name in the file.
    title: Option<String>,
    settings: AudioSettings,
    layout: ChannelLayout,
    stream: usize,
    time_base: Rational,
    stream_time_base: Rational,
    frame_size: usize,
    fifo: AudioFifo,
    /// De-interleaving scratch, one buffer per channel.
    planes: Vec<Vec<f32>>,
    samples_encoded: u64,
    pub(super) blocks_dropped: u64,
    blocks_after_end: u64,
    /// Packets (one encoder frame each) handed to the muxer without error.
    packets_written: u64,
    padding_samples: u64,
    first_pts: Option<i64>,
    end_pts: Option<i64>,
}

impl AudioTrack {
    fn open(
        output: &mut format::context::Output,
        settings: &AudioSettings,
        title: Option<String>,
        global_header: bool,
    ) -> Result<Self, RecorderError> {
        let codec = encoder::find(codec::Id::AAC).ok_or_else(|| {
            RecorderError::new(
                RecorderErrorKind::Unsupported,
                "this FFmpeg build has no AAC encoder",
            )
        })?;
        let name = codec.name().to_owned();
        let encoder_error = |context: &str, err| error(RecorderErrorKind::Encoder, context, err);
        let layout = match settings.channels {
            1 => ChannelLayout::MONO,
            2 => ChannelLayout::STEREO,
            other => {
                return Err(RecorderError::new(
                    RecorderErrorKind::Unsupported,
                    format!("{other} audio channels are not supported; use 1 or 2"),
                ));
            }
        };
        let mut context = codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .map_err(|err| encoder_error("cannot create the audio encoder", err))?;
        let rate = positive_i32(settings.rate, "sample rate")?;
        let bitrate = usize::try_from(settings.bitrate).map_err(|_| {
            RecorderError::new(
                RecorderErrorKind::Unsupported,
                format!("bit rate {} is out of range", settings.bitrate),
            )
        })?;
        let time_base = Rational::new(1, rate);
        context.set_rate(rate);
        context.set_channel_layout(layout);
        context.set_format(Sample::F32(SampleType::Planar));
        context.set_bit_rate(bitrate);
        context.set_time_base(time_base);
        if global_header {
            context.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let encoder = context
            .open_as(codec)
            .map_err(|err| encoder_error(&format!("cannot open audio encoder {name}"), err))?;
        let frame_size = match encoder.frame_size() {
            0 => 1024,
            size => size as usize,
        };
        let stream = {
            let mut stream = output.add_stream(codec).map_err(|err| {
                error(
                    RecorderErrorKind::Output,
                    "cannot add the audio stream",
                    err,
                )
            })?;
            stream.set_parameters(&encoder);
            stream.set_time_base(time_base);
            if let Some(title) = &title {
                let mut metadata = Dictionary::new();
                metadata.set("title", title);
                stream.set_metadata(metadata);
            }
            stream.index()
        };
        let channels = settings.channels as usize;
        Ok(Self {
            encoder,
            name,
            title,
            settings: settings.clone(),
            layout,
            stream,
            time_base,
            stream_time_base: time_base,
            frame_size,
            fifo: AudioFifo::new(channels, settings.rate as usize),
            planes: vec![vec![0.0; frame_size]; channels],
            samples_encoded: 0,
            blocks_dropped: 0,
            blocks_after_end: 0,
            packets_written: 0,
            padding_samples: 0,
            first_pts: None,
            end_pts: None,
        })
    }

    fn push(
        &mut self,
        output: &mut format::context::Output,
        block: &[f32],
        pts: MediaTimestamp,
        end_ns: i64,
    ) -> Result<(), RecorderError> {
        if after_end(pts, end_ns) {
            self.blocks_after_end += 1;
            return Ok(());
        }
        let Ok(pts) = pts.rescale(1, self.settings.rate) else {
            self.blocks_dropped += 1;
            return Ok(());
        };
        if self.fifo.push(block, pts) {
            // Timeline restarts: encode what is queued (padded), then begin anew.
            self.encode_queued(output, true)?;
            self.fifo.restart(block, pts);
        }
        self.encode_queued(output, false)
    }

    /// Encode full frames; with `pad`, also the remainder, padded with silence
    /// (AAC needs full frames except at the very end).
    fn encode_queued(
        &mut self,
        output: &mut format::context::Output,
        pad: bool,
    ) -> Result<(), RecorderError> {
        loop {
            let queued = self.fifo.frames();
            if queued == 0 || (queued < self.frame_size && !pad) {
                return Ok(());
            }
            for plane in &mut self.planes {
                plane.fill(0.0);
            }
            let (pts, count) = {
                let mut planes: Vec<&mut [f32]> =
                    self.planes.iter_mut().map(Vec::as_mut_slice).collect();
                match self.fifo.pop_planar(self.frame_size, &mut planes) {
                    Some(popped) => popped,
                    None => return Ok(()),
                }
            };
            self.padding_samples += (self.frame_size - count) as u64;
            let mut frame = frame::Audio::new(
                Sample::F32(SampleType::Planar),
                self.frame_size,
                self.layout,
            );
            frame.set_rate(self.settings.rate);
            for (index, plane) in self.planes.iter().enumerate() {
                frame.plane_mut::<f32>(index)[..self.frame_size].copy_from_slice(plane);
            }
            frame.set_pts(Some(pts));
            self.encoder
                .send_frame(&frame)
                .map_err(|err| error(RecorderErrorKind::Encoder, "audio encoding failed", err))?;
            self.samples_encoded += count as u64;
            self.first_pts.get_or_insert(pts);
            self.end_pts = Some(pts.saturating_add(frames_i64(count)));
            drain(
                &mut self.encoder,
                output,
                self.stream,
                self.time_base,
                self.stream_time_base,
                &mut self.packets_written,
            )?;
        }
    }

    fn flush(&mut self, output: &mut format::context::Output) -> Result<(), RecorderError> {
        self.encode_queued(output, true)?;
        self.encoder.send_eof().map_err(|err| {
            error(
                RecorderErrorKind::Encoder,
                "cannot flush the audio encoder",
                err,
            )
        })?;
        drain(
            &mut self.encoder,
            output,
            self.stream,
            self.time_base,
            self.stream_time_base,
            &mut self.packets_written,
        )
    }

    fn pts_ms(&self, pts: i64) -> f64 {
        pts as f64 * 1000.0 / f64::from(self.settings.rate)
    }

    fn end_ms(&self) -> f64 {
        self.end_pts.map_or(0.0, |pts| self.pts_ms(pts))
    }

    /// Samples accepted by the encoder that never reached the muxer, in
    /// whole encoder frames.
    fn undelivered(&self) -> u64 {
        let delivered = self.packets_written.saturating_mul(self.frame_size as u64);
        self.samples_encoded.saturating_sub(delivered)
    }

    /// Samples encoded and blocks dropped by this track so far.
    fn counts(&self) -> (u64, u64) {
        (self.samples_encoded, self.blocks_dropped)
    }

    fn stats(&self, queue_dropped: u64) -> AudioTrackStats {
        let fifo = self.fifo.stats();
        AudioTrackStats {
            name: self.title.clone(),
            encoder: self.name.clone(),
            sample_rate_hz: self.settings.rate,
            channels: self.settings.channels,
            bitrate_bps: self.settings.bitrate,
            samples_encoded: self.samples_encoded,
            blocks_dropped: self.blocks_dropped + queue_dropped,
            blocks_after_end: self.blocks_after_end,
            gap_samples: fifo.gap_samples,
            overlap_samples: fifo.overlap_samples,
            discontinuities: fifo.discontinuities,
            padding_samples: self.padding_samples,
            first_pts_ms: self.first_pts.map(|pts| self.pts_ms(pts)),
            end_ms: self.end_pts.map(|pts| self.pts_ms(pts)),
        }
    }
}
