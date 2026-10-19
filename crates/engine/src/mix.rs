//! `probe_audio_mix`: mix live inputs and test tones on the mixer thread.

use std::thread;
use std::time::{Duration, Instant};

use audio_engine::{
    CHANNELS, InputSource, MAX_CORRECTION, MixerConfig, MixerInput, MixerThread, RATE, StripConfig,
    Tone,
};
use capture_api::{AudioStream, AudioStreamRequest, CaptureBackend};
use command_api::{CaptureOutcome, CommandError, ErrorCode, ErrorSummary, MixReport, MixRequest};
use serde_json::{Value, json};

use crate::command_error;

/// How often the waiting engine thread checks whether the mixer died.
const WATCH_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn probe(
    capture: &mut dyn CaptureBackend,
    request: &MixRequest,
) -> Result<MixReport, CommandError> {
    // Start every live input first; on error the already started streams
    // are dropped, which stops them.
    let mut streams: Vec<Box<dyn AudioStream>> = Vec::with_capacity(request.inputs.len());
    for spec in &request.inputs {
        let stream = capture
            .start_audio_stream(&AudioStreamRequest {
                input: spec.input.clone(),
                rate: RATE,
                channels: CHANNELS,
            })
            .map_err(command_error)?;
        streams.push(stream);
    }

    let mut inputs = Vec::with_capacity(request.inputs.len() + request.tones.len());
    for (spec, stream) in request.inputs.iter().zip(&streams) {
        inputs.push(MixerInput {
            name: spec.input.clone().unwrap_or_else(|| "default".to_owned()),
            source: InputSource::Live(stream.samples()),
            strip: StripConfig {
                gain_db: spec.gain_db,
                muted: spec.muted,
            },
        });
    }
    for tone in &request.tones {
        inputs.push(MixerInput {
            name: format!("tone {} Hz", tone.frequency_hz),
            source: InputSource::Tone(Tone::new(tone.frequency_hz, tone.level_dbfs, RATE)),
            strip: StripConfig {
                gain_db: tone.gain_db,
                muted: tone.muted,
            },
        });
    }

    let mixer = MixerThread::spawn(MixerConfig {
        inputs,
        master_gain_db: request.master_gain_db,
        expected_duration: request.duration,
    })
    .map_err(internal)?;
    let deadline = Instant::now() + request.duration;
    while !mixer.is_finished() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(WATCH_INTERVAL.min(deadline - now));
    }
    let stats = mixer.stop();

    // Stop every capture on every path, so its diagnostics are kept.
    let captures: Vec<CaptureOutcome> = request
        .inputs
        .iter()
        .zip(streams)
        .map(|(spec, stream)| match stream.stop() {
            Ok(report) => CaptureOutcome {
                input: spec.input.clone(),
                report: Some(report),
                error: None,
            },
            Err(error) => CaptureOutcome {
                input: spec.input.clone(),
                report: None,
                error: Some(ErrorSummary::from(error)),
            },
        })
        .collect();
    let mix = match stats {
        Ok(stats) => stats,
        Err(error) => {
            let details = json!({ "captures": to_value(&captures) });
            return Err(internal(error).with_details(details));
        }
    };

    let mut warnings = Vec::new();
    for input in &mix.inputs {
        if input.kind == "live" && input.ring.is_none_or(|ring| ring.pulled_frames == 0) {
            warnings.push(format!(
                "input `{}` delivered no audio during the probe (it stayed in warm-up); \
                 see `captures` for its negotiated format and errors",
                input.name
            ));
        }
        if input
            .drift_ppm
            .is_some_and(|ppm| ppm.abs() >= 0.9 * MAX_CORRECTION * 1e6)
        {
            warnings.push(format!(
                "input `{}` drifts at the resampler's correction limit; expect underruns or trims",
                input.name
            ));
        }
        let overrun = input.ring.map_or(0, |ring| ring.overrun_frames);
        if input.underrun_frames > 0 || input.trimmed_frames > 0 || overrun > 0 {
            warnings.push(format!(
                "input `{}`: {} frame(s) padded with silence (underrun), {} trimmed to bound \
                 latency, {} overwritten in the ring",
                input.name, input.underrun_frames, input.trimmed_frames, overrun
            ));
        }
    }
    if mix.clipped_samples > 0 {
        warnings.push(format!(
            "{} sample(s) clipped on the master bus; lower the gains",
            mix.clipped_samples
        ));
    }
    if mix.late_blocks > 0 || mix.skipped_blocks > 0 {
        warnings.push(format!(
            "mixer missed its deadline: {} late block(s), {} skipped",
            mix.late_blocks, mix.skipped_blocks
        ));
    }
    for capture in &captures {
        if let Some(error) = &capture.error {
            warnings.push(format!("capture failed: {}", error.message));
        }
    }
    Ok(MixReport {
        mix,
        captures,
        warnings,
    })
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|err| Value::String(err.to_string()))
}

fn internal(error: impl std::fmt::Display) -> CommandError {
    CommandError::new(ErrorCode::Internal, format!("mixer failed: {error}"))
}
