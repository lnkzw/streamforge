//! Decodes a recording from start to end: every video frame and every
//! audio track, counting decoder errors, timestamps that go backwards and
//! gaps between frames, and where each track starts and ends.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ffmpeg_next as ff;
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct FileCheck {
    pub video: VideoCheck,
    pub audio: Vec<AudioCheck>,
    pub decode_errors: u64,
    /// The first decoder error, for the report.
    pub first_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct VideoCheck {
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    /// The first packet's start and the last-ending packet's end.
    pub start_s: f64,
    pub end_s: f64,
    /// Decoded frames whose timestamp was not after the previous one's.
    pub out_of_order: u64,
    /// Steps between frames longer than one and a half frame durations.
    pub gaps: u64,
    pub longest_step_s: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AudioCheck {
    /// Per channel.
    pub samples: u64,
    pub start_s: f64,
    pub end_s: f64,
}

enum Decoder {
    Video(ff::decoder::Video, VideoCheck, Option<i64>),
    Audio(ff::decoder::Audio, AudioCheck),
}

/// Decode `path`, expecting `fps` frames per second. Calls `progress` with
/// the position reached, in seconds, every few seconds.
pub fn inspect(path: &Path, fps: f64, mut progress: impl FnMut(f64)) -> Result<FileCheck> {
    let mut input =
        ff::format::input(path).with_context(|| format!("cannot open {}", path.display()))?;
    let video_index = input
        .streams()
        .best(ff::media::Type::Video)
        .context("the recording has no video track")?
        .index();
    let mut decoders = Vec::new();
    for stream in input.streams() {
        let mut context = ff::codec::context::Context::from_parameters(stream.parameters())
            .context("cannot read a track's codec parameters")?;
        let time_base = f64::from(stream.time_base());
        let decoder = if stream.index() == video_index {
            // Frame threads: an hour at 60 FPS is 216 000 frames.
            context.set_threading(ff::codec::threading::Config::kind(
                ff::codec::threading::Type::Frame,
            ));
            let decoder = context.decoder().video().context("no video decoder")?;
            let check = VideoCheck {
                width: decoder.width(),
                height: decoder.height(),
                start_s: f64::MAX,
                ..VideoCheck::default()
            };
            Decoder::Video(decoder, check, None)
        } else if stream.parameters().medium() == ff::media::Type::Audio {
            let decoder = context.decoder().audio().context("no audio decoder")?;
            let check = AudioCheck {
                start_s: f64::MAX,
                ..AudioCheck::default()
            };
            Decoder::Audio(decoder, check)
        } else {
            continue;
        };
        decoders.push((stream.index(), time_base, decoder));
    }

    let mut check = FileCheck::default();
    let mut errors = Errors::default();
    let step_limit = 1.5 / fps;
    let mut picture = ff::frame::Video::empty();
    let mut sound = ff::frame::Audio::empty();
    let mut reported = Instant::now();
    for (stream, packet) in input.packets() {
        let Some((_, time_base, decoder)) = decoders
            .iter_mut()
            .find(|(index, ..)| *index == stream.index())
        else {
            continue;
        };
        let time_base = *time_base;
        match decoder {
            Decoder::Video(decoder, video, last) => {
                if let Some(pts) = packet.pts() {
                    video.start_s = video.start_s.min(pts as f64 * time_base);
                    video.end_s = video
                        .end_s
                        .max((pts + packet.duration()) as f64 * time_base);
                }
                if let Err(error) = decoder.send_packet(&packet) {
                    errors.count(format!("video packet: {error}"));
                }
                receive_pictures(
                    decoder,
                    &mut picture,
                    time_base,
                    step_limit,
                    video,
                    last,
                    &mut errors,
                );
            }
            Decoder::Audio(decoder, audio) => {
                if let Some(pts) = packet.pts() {
                    audio.start_s = audio.start_s.min(pts as f64 * time_base);
                    audio.end_s = audio
                        .end_s
                        .max((pts + packet.duration()) as f64 * time_base);
                }
                if let Err(error) = decoder.send_packet(&packet) {
                    errors.count(format!("audio packet: {error}"));
                }
                receive_sound(decoder, &mut sound, audio, &mut errors);
            }
        }
        if reported.elapsed() >= Duration::from_secs(10)
            && let Some(pts) = packet.pts()
        {
            progress(pts as f64 * time_base);
            reported = Instant::now();
        }
    }
    for (_, time_base, decoder) in &mut decoders {
        match decoder {
            Decoder::Video(decoder, video, last) => {
                if let Err(error) = decoder.send_eof() {
                    errors.count(format!("video end: {error}"));
                }
                receive_pictures(
                    decoder,
                    &mut picture,
                    *time_base,
                    step_limit,
                    video,
                    last,
                    &mut errors,
                );
                check.video = video.clone();
            }
            Decoder::Audio(decoder, audio) => {
                if let Err(error) = decoder.send_eof() {
                    errors.count(format!("audio end: {error}"));
                }
                receive_sound(decoder, &mut sound, audio, &mut errors);
                check.audio.push(audio.clone());
            }
        }
    }
    check.decode_errors = errors.count;
    check.first_error = errors.first;
    Ok(check)
}

#[derive(Default)]
struct Errors {
    count: u64,
    first: Option<String>,
}

impl Errors {
    fn count(&mut self, error: String) {
        self.count += 1;
        self.first.get_or_insert(error);
    }
}

/// `true` for the decoder's "nothing more now" answers.
fn drained(error: &ff::Error) -> bool {
    matches!(error, ff::Error::Eof)
        || matches!(error, ff::Error::Other { errno } if *errno == ff::util::error::EAGAIN)
}

fn receive_pictures(
    decoder: &mut ff::decoder::Video,
    picture: &mut ff::frame::Video,
    time_base: f64,
    step_limit: f64,
    video: &mut VideoCheck,
    last: &mut Option<i64>,
    errors: &mut Errors,
) {
    loop {
        match decoder.receive_frame(picture) {
            Ok(()) => {}
            Err(error) if drained(&error) => return,
            Err(error) => {
                errors.count(format!("video frame: {error}"));
                return;
            }
        }
        video.frames += 1;
        let Some(pts) = picture.timestamp() else {
            continue;
        };
        if let Some(previous) = *last {
            if pts <= previous {
                video.out_of_order += 1;
            } else {
                let step = (pts - previous) as f64 * time_base;
                video.longest_step_s = video.longest_step_s.max(step);
                if step > step_limit {
                    video.gaps += 1;
                }
            }
        }
        *last = Some(pts);
    }
}

fn receive_sound(
    decoder: &mut ff::decoder::Audio,
    sound: &mut ff::frame::Audio,
    audio: &mut AudioCheck,
    errors: &mut Errors,
) {
    loop {
        match decoder.receive_frame(sound) {
            Ok(()) => audio.samples += sound.samples() as u64,
            Err(error) if drained(&error) => return,
            Err(error) => {
                errors.count(format!("audio frame: {error}"));
                return;
            }
        }
    }
}
