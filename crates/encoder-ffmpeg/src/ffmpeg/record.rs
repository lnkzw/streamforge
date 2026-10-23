//! The `recorder` thread: one Matroska file.
//!
//! The file is finalized (encoders flushed, trailer written) on every path
//! that got past the header, including encoder errors. After a failure the
//! thread keeps receiving until the stop marker and counts everything as
//! dropped.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;

use ff::Dictionary;
use ffmpeg_next as ff;

use super::{EncoderChoice, Session, counts_of, init};
use crate::{
    Item, Ready, RecorderConfig, RecorderError, RecorderErrorKind, RecordingStats, Shared,
    TrackPool,
};

const CONTAINER: &str = "matroska";

pub(crate) fn run(
    config: RecorderConfig,
    rx: Receiver<Item>,
    ready: Ready,
    shared: Arc<Shared>,
    pool: Option<Arc<TrackPool>>,
) -> Result<RecordingStats, RecorderError> {
    let mut session = match open(&config, &shared, pool.clone()) {
        Ok(session) => {
            let _ = ready.send(Ok(session.video.info()));
            session
        }
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };

    let mut failure = None;
    while let Ok(item) = rx.recv() {
        shared.received(&item);
        if matches!(item, Item::Stop) {
            break;
        }
        let written = session.write(item, shared.end_ns.load(Ordering::Acquire));
        shared.publish(session.counts());
        if let Err(error) = written {
            shared.failed.store(true, Ordering::Release);
            failure = Some(error);
            break;
        }
    }

    let (mut video, mut audio, mut extra, finished) = session.finish();
    // Flushing encoded what the encoders still held.
    shared.publish(counts_of(&video, &audio));
    if failure.is_some() {
        // Producers run until the owner stops them: count what they still
        // queue instead of letting it vanish with the channel.
        for item in &rx {
            shared.received(&item);
            match item {
                Item::Video { .. } => video.frames_dropped += 1,
                Item::Audio { extra: block, .. } => {
                    audio.blocks_dropped += 1;
                    for track in &mut extra {
                        track.blocks_dropped += 1;
                    }
                    if let (Some(pool), Some(index)) = (&pool, block) {
                        pool.give_back(index);
                    }
                }
                Item::Stop => break,
            }
            shared.publish(counts_of(&video, &audio));
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
        // Queue drops take every track's block at once.
        extra_audio: extra
            .iter()
            .map(|track| track.stats(shared.audio_dropped.load(Ordering::Relaxed)))
            .collect(),
    };
    match failure.or(finished.err()) {
        None => Ok(stats),
        Some(mut error) => {
            error.partial = Some(Box::new(stats));
            Err(error)
        }
    }
}

fn open(
    config: &RecorderConfig,
    shared: &Arc<Shared>,
    pool: Option<Arc<TrackPool>>,
) -> Result<Session, RecorderError> {
    init(false)?;
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
    Session::open(
        &format!("file:{path}"),
        CONTAINER,
        Dictionary::new(),
        &config.path.display().to_string(),
        RecorderErrorKind::Output,
        &config.video,
        &mut EncoderChoice::new(&config.video),
        (&config.audio, &config.audio_tracks, pool),
        // File I/O is never interrupted: the recorder has no stop deadline.
        Arc::clone(shared),
    )
}
