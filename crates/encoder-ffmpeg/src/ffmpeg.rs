//! FFmpeg implementation of the recorder thread (Linux).
//!
//! All FFmpeg objects are created and used on the `recorder` thread. The
//! output is finalized (encoders flushed, trailer written) on every path that
//! got past the header, including encoder errors. After a failure the thread
//! keeps receiving until the stop marker and counts everything as dropped.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender};

use ff::format::Pixel;
use ff::format::Sample;
use ff::format::sample::Type as SampleType;
use ff::util::color;
use ff::{ChannelLayout, Dictionary, Packet, Rational, codec, encoder, format, frame, software};
use ffmpeg_next as ff;
use media_core::{MediaTimestamp, VideoFrame};

use crate::audio_fifo::AudioFifo;
use crate::{
    AudioSettings, AudioTrackStats, Item, RecorderConfig, RecorderError, RecorderErrorKind,
    RecordingStats, Shared, VideoSettings, VideoTrackStats,
};

const CONTAINER: &str = "matroska";

pub(crate) fn run(
    config: RecorderConfig,
    rx: Receiver<Item>,
    ready: SyncSender<Result<(), RecorderError>>,
    shared: Arc<Shared>,
) -> Result<RecordingStats, RecorderError> {
    let (mut output, mut video, mut audio) = match open(&config) {
        Ok(opened) => {
            let _ = ready.send(Ok(()));
            opened
        }
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };

    let mut failure = None;
    while let Ok(item) = rx.recv() {
        let end_ns = shared.end_ns.load(Ordering::Acquire);
        let result = match item {
            Item::Video { frame, pts } => video.encode(&mut output, &frame, pts, end_ns),
            Item::Audio { samples, len, pts } => {
                audio.push(&mut output, &samples[..len], pts, end_ns)
            }
            Item::Stop => break,
        };
        if let Err(error) = result {
            shared.failed.store(true, Ordering::Release);
            failure = Some(error);
            break;
        }
    }

    // Flush both tracks even if one fails, then write the trailer; the first
    // error wins.
    let video_flushed = video.flush(&mut output);
    let audio_flushed = audio.flush(&mut output);
    let trailer = output
        .write_trailer()
        .map_err(|err| error(RecorderErrorKind::Output, "cannot finalize the file", err));
    drop(output);
    if failure.is_some() {
        // Producers run until the owner stops them: count what they still
        // queue instead of letting it vanish with the channel.
        for item in &rx {
            match item {
                Item::Video { .. } => video.frames_dropped += 1,
                Item::Audio { .. } => audio.blocks_dropped += 1,
                Item::Stop => break,
            }
        }
    }
    drop(rx);

    let stats = RecordingStats {
        path: config.path.display().to_string(),
        container: CONTAINER.to_owned(),
        duration_ms: video.end_ms().max(audio.end_ms()),
        file_bytes: std::fs::metadata(&config.path).ok().map(|m| m.len()),
        video: video.stats(shared.video_dropped.load(Ordering::Relaxed)),
        audio: audio.stats(shared.audio_dropped.load(Ordering::Relaxed)),
    };
    let first_error = failure
        .or(video_flushed.err())
        .or(audio_flushed.err())
        .or(trailer.err());
    match first_error {
        None => Ok(stats),
        Some(mut error) => {
            error.partial = Some(Box::new(stats));
            Err(error)
        }
    }
}

fn open(
    config: &RecorderConfig,
) -> Result<(format::context::Output, VideoTrack, AudioTrack), RecorderError> {
    ff::init()
        .map_err(|err| error(RecorderErrorKind::Internal, "cannot initialize FFmpeg", err))?;
    // FFmpeg logs to stderr; keep it to real errors.
    ff::log::set_level(ff::log::Level::Error);
    let path = config.path.to_str().ok_or_else(|| {
        RecorderError::new(
            RecorderErrorKind::Output,
            format!(
                "the output path {} is not valid UTF-8",
                config.path.display()
            ),
        )
    })?;
    // The `file:` prefix keeps FFmpeg from reading the name as another
    // protocol (e.g. `pipe:1.mkv`); the container is fixed, not guessed.
    let url = format!("file:{path}");
    let mut output = format::output_as(&url, CONTAINER).map_err(|err| {
        error(
            RecorderErrorKind::Output,
            &format!("cannot create {}", config.path.display()),
            err,
        )
    })?;
    let global_header = output
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER);
    let mut video = VideoTrack::open(&mut output, &config.video, global_header)?;
    let mut audio = AudioTrack::open(&mut output, &config.audio, global_header)?;
    output.write_header().map_err(|err| {
        error(
            RecorderErrorKind::Output,
            "cannot write the file header",
            err,
        )
    })?;
    // The muxer may pick its own stream time bases while writing the header.
    video.stream_time_base = stream_time_base(&output, video.stream);
    audio.stream_time_base = stream_time_base(&output, audio.stream);
    Ok((output, video, audio))
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

struct VideoTrack {
    encoder: encoder::Video,
    name: String,
    settings: VideoSettings,
    stream: usize,
    time_base: Rational,
    stream_time_base: Rational,
    scaler: software::scaling::Context,
    /// Reused BGRA staging frame; only the scaler reads it.
    staging: frame::Video,
    frames_encoded: u64,
    frames_dropped: u64,
    frames_after_end: u64,
    first_pts: Option<i64>,
    last_pts: Option<i64>,
}

impl VideoTrack {
    fn open(
        output: &mut format::context::Output,
        settings: &VideoSettings,
        global_header: bool,
    ) -> Result<Self, RecorderError> {
        let codec = encoder::find_by_name(&settings.encoder)
            .or_else(|| encoder::find(codec::Id::H264))
            .ok_or_else(|| {
                RecorderError::new(
                    RecorderErrorKind::Unsupported,
                    "this FFmpeg build has no H.264 encoder",
                )
            })?;
        let name = codec.name().to_owned();
        let encoder_error = |context: &str, err| error(RecorderErrorKind::Encoder, context, err);
        let mut context = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(|err| encoder_error("cannot create the video encoder", err))?;
        let fps = positive_i32(settings.fps, "frame rate")?;
        let time_base = Rational::new(1, fps);
        context.set_width(settings.width);
        context.set_height(settings.height);
        context.set_format(Pixel::YUV420P);
        context.set_time_base(time_base);
        context.set_frame_rate(Some(Rational::new(fps, 1)));
        // Tag what the conversion below produces: swscale's default BGRA to
        // YUV matrix is BT.601 with limited range. The canvas is sRGB
        // (BT.709 primaries, sRGB transfer; ADR 0003). Untagged HD video
        // would be shown with the BT.709 matrix, shifting hues.
        context.set_colorspace(color::Space::SMPTE170M);
        context.set_color_range(color::Range::MPEG);
        context.set_color_primaries(color::Primaries::BT709);
        if global_header {
            context.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let mut options = Dictionary::new();
        // No typed setter for the transfer in ffmpeg-next; it is a codec
        // context option.
        options.set("color_trc", "iec61966-2-1");
        for (key, value) in &settings.options {
            options.set(key, value);
        }
        let encoder = context
            .open_with(options)
            .map_err(|err| encoder_error(&format!("cannot open video encoder {name}"), err))?;
        let stream = {
            let mut stream = output.add_stream(codec).map_err(|err| {
                error(
                    RecorderErrorKind::Output,
                    "cannot add the video stream",
                    err,
                )
            })?;
            stream.set_parameters(&encoder);
            stream.set_time_base(time_base);
            stream.index()
        };
        let scaler = software::scaling::Context::get(
            Pixel::BGRA,
            settings.width,
            settings.height,
            Pixel::YUV420P,
            settings.width,
            settings.height,
            software::scaling::Flags::BILINEAR,
        )
        .map_err(|err| encoder_error("cannot create the color converter", err))?;
        Ok(Self {
            encoder,
            name,
            settings: settings.clone(),
            stream,
            time_base,
            stream_time_base: time_base,
            scaler,
            staging: frame::Video::new(Pixel::BGRA, settings.width, settings.height),
            frames_encoded: 0,
            frames_dropped: 0,
            frames_after_end: 0,
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
        let mut picture =
            frame::Video::new(Pixel::YUV420P, self.settings.width, self.settings.height);
        self.scaler
            .run(&self.staging, &mut picture)
            .map_err(|err| error(RecorderErrorKind::Encoder, "color conversion failed", err))?;
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
        )
    }

    fn flush(&mut self, output: &mut format::context::Output) -> Result<(), RecorderError> {
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
        )
    }

    fn pts_ms(&self, pts: i64) -> f64 {
        pts as f64 * 1000.0 / f64::from(self.settings.fps)
    }

    fn end_ms(&self) -> f64 {
        self.last_pts.map_or(0.0, |pts| self.pts_ms(pts + 1))
    }

    fn stats(&self, queue_dropped: u64) -> VideoTrackStats {
        VideoTrackStats {
            encoder: self.name.clone(),
            width: self.settings.width,
            height: self.settings.height,
            fps: self.settings.fps,
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
    blocks_dropped: u64,
    blocks_after_end: u64,
    padding_samples: u64,
    first_pts: Option<i64>,
    end_pts: Option<i64>,
}

impl AudioTrack {
    fn open(
        output: &mut format::context::Output,
        settings: &AudioSettings,
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
            stream.index()
        };
        let channels = settings.channels as usize;
        Ok(Self {
            encoder,
            name,
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
        )
    }

    fn pts_ms(&self, pts: i64) -> f64 {
        pts as f64 * 1000.0 / f64::from(self.settings.rate)
    }

    fn end_ms(&self) -> f64 {
        self.end_pts.map_or(0.0, |pts| self.pts_ms(pts))
    }

    fn stats(&self, queue_dropped: u64) -> AudioTrackStats {
        let fifo = self.fifo.stats();
        AudioTrackStats {
            encoder: self.name.clone(),
            rate: self.settings.rate,
            channels: self.settings.channels,
            bitrate: self.settings.bitrate,
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
