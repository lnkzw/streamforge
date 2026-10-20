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

use super::{Session, init};
use crate::{
    Item, Ready, RecorderConfig, RecorderError, RecorderErrorKind, RecordingStats, Shared,
};

const CONTAINER: &str = "matroska";

pub(crate) fn run(
    config: RecorderConfig,
    rx: Receiver<Item>,
    ready: Ready,
    shared: Arc<Shared>,
) -> Result<RecordingStats, RecorderError> {
    let mut session = match open(&config, &shared) {
        Ok(session) => {
            let _ = ready.send(Ok(()));
            session
        }
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };

    let mut failure = None;
    while let Ok(item) = rx.recv() {
        if matches!(item, Item::Stop) {
            break;
        }
        if let Err(error) = session.write(item, shared.end_ns.load(Ordering::Acquire)) {
            shared.failed.store(true, Ordering::Release);
            failure = Some(error);
            break;
        }
    }

    let (mut video, mut audio, finished) = session.finish();
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
    match failure.or(finished.err()) {
        None => Ok(stats),
        Some(mut error) => {
            error.partial = Some(Box::new(stats));
            Err(error)
        }
    }
}

fn open(config: &RecorderConfig, shared: &Arc<Shared>) -> Result<Session, RecorderError> {
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
        &config.audio,
        // File I/O is never interrupted: the recorder has no stop deadline.
        Arc::clone(shared),
    )
}
