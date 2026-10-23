//! Schema version 1's audio (ADR 0015), frozen for migration (ADR 0017).
//!
//! Version 1 stored the parameters of the former `set_audio_sources`
//! command: live inputs and test tones mixed into a master. That command is
//! gone, so its rules live on here, with its limits as they were, and do not
//! follow later changes to the commands. A version 1 file migrates on load:
//! inputs, then tones, become channels sending to the program bus, and the
//! master gain becomes the program bus fader.

use command_api::{
    AudioGraphSpec, CHANNEL_ID_PREFIX, ChannelInput, ChannelSpec, ProgramBusSpec, ProjectProblem,
    new_id,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{problem, structure};

/// Version 1's limits.
const MAX_SOURCES: usize = 8;
const MIN_GAIN_DB: f32 = -96.0;
const MAX_GAIN_DB: f32 = 24.0;
const NYQUIST_HZ: f64 = 24_000.0;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Audio {
    inputs: Vec<Input>,
    tones: Vec<Tone>,
    master_gain_db: f32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    /// `null`: the system default input.
    input: Option<String>,
    gain_db: f32,
    mute: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tone {
    frequency_hz: f64,
    level_dbfs: f64,
    gain_db: f32,
    mute: bool,
}

/// Check version 1's `audio` and migrate it to the graph.
pub(crate) fn migrate(audio: Value) -> Result<AudioGraphSpec, Vec<ProjectProblem>> {
    let audio: Audio = structure("audio", audio).map_err(|found| vec![found])?;
    let mut problems = Vec::new();
    let sources = audio.inputs.len() + audio.tones.len();
    if sources > MAX_SOURCES {
        problems.push(problem(
            "audio",
            format!("at most {MAX_SOURCES} sources in `inputs` and `tones`, got {sources}"),
        ));
    }
    let gain = |problems: &mut Vec<ProjectProblem>, path: String, db: f32| {
        if !(db.is_finite() && (MIN_GAIN_DB..=MAX_GAIN_DB).contains(&db)) {
            problems.push(problem(
                &path,
                format!("must be between {MIN_GAIN_DB} and {MAX_GAIN_DB} dB"),
            ));
        }
    };
    gain(
        &mut problems,
        "audio.master_gain_db".to_owned(),
        audio.master_gain_db,
    );
    let mut channels = Vec::with_capacity(sources);
    for (index, input) in audio.inputs.into_iter().enumerate() {
        let at = format!("audio.inputs[{index}]");
        if input.input.as_deref().is_some_and(str::is_empty) {
            problems.push(problem(
                &format!("{at}.input"),
                "must not be empty".to_owned(),
            ));
        }
        gain(&mut problems, format!("{at}.gain_db"), input.gain_db);
        channels.push((
            format!("Input {}", index + 1),
            ChannelInput::Device {
                device: input.input,
            },
            input.gain_db,
            input.mute,
        ));
    }
    for (index, tone) in audio.tones.into_iter().enumerate() {
        let at = format!("audio.tones[{index}]");
        if !(tone.frequency_hz.is_finite()
            && tone.frequency_hz > 0.0
            && tone.frequency_hz < NYQUIST_HZ)
        {
            problems.push(problem(
                &format!("{at}.frequency_hz"),
                format!("must be between 0 and {NYQUIST_HZ} (exclusive)"),
            ));
        }
        if !(tone.level_dbfs.is_finite()
            && (f64::from(MIN_GAIN_DB)..=0.0).contains(&tone.level_dbfs))
        {
            problems.push(problem(
                &format!("{at}.level_dbfs"),
                format!("must be between {MIN_GAIN_DB} and 0"),
            ));
        }
        gain(&mut problems, format!("{at}.gain_db"), tone.gain_db);
        channels.push((
            format!("Tone {}", index + 1),
            ChannelInput::Tone {
                frequency_hz: tone.frequency_hz,
                level_dbfs: tone.level_dbfs,
            },
            tone.gain_db,
            tone.mute,
        ));
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let mut migrated: Vec<ChannelSpec> = Vec::with_capacity(channels.len());
    for (name, input, gain_db, muted) in channels {
        let id = new_id(CHANNEL_ID_PREFIX, |id| {
            migrated.iter().any(|channel| channel.id == id)
        })
        .map_err(|error| vec![problem("audio", error.message)])?;
        migrated.push(ChannelSpec {
            id,
            name,
            input,
            gain_db,
            muted,
            balance: 0.0,
        });
    }
    Ok(AudioGraphSpec {
        channels: migrated,
        program: ProgramBusSpec {
            gain_db: audio.master_gain_db,
            muted: false,
        },
    })
}
