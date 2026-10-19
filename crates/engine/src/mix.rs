//! `probe_audio_mix`: mix live inputs and test tones on the mixer thread.

use std::thread;
use std::time::{Duration, Instant};

use audio_engine::{
    CHANNELS, InputSource, MAX_CORRECTION, MixerConfig, MixerInput, MixerThread, RATE, StripConfig,
    Tone,
};
use capture_api::{AudioStream, AudioStreamRequest, CaptureBackend};
use command_api::{
    CaptureOutcome, CommandError, ErrorCode, ErrorSummary, LiveInputSpec, MixReport, MixRequest,
    ToneSpec,
};
use serde_json::{Value, json};

use crate::command_error;

/// How often the waiting engine thread checks whether the mixer died.
const WATCH_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn probe(
    capture: &mut dyn CaptureBackend,
    request: &MixRequest,
) -> Result<MixReport, CommandError> {
    let streams = start_live_inputs(capture, &request.inputs)?;
    let inputs = mixer_inputs(&request.inputs, &streams, &request.tones);

    let mixer = MixerThread::spawn(MixerConfig {
        inputs,
        master_gain_db: request.master_gain_db,
        expected_duration: request.duration,
        sink: None,
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
    let captures = stop_captures(&request.inputs, streams);
    let mix = match stats {
        Ok(stats) => stats,
        Err(error) => {
            let details = json!({ "captures": to_value(&captures) });
            return Err(internal(error).with_details(details));
        }
    };

    let mut warnings = mix_warnings(&mix);
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

/// Start one stream per live input; on error the streams already started
/// are dropped, which stops them.
pub(crate) fn start_live_inputs(
    capture: &mut dyn CaptureBackend,
    specs: &[LiveInputSpec],
) -> Result<Vec<Box<dyn AudioStream>>, CommandError> {
    specs
        .iter()
        .map(|spec| {
            capture
                .start_audio_stream(&AudioStreamRequest {
                    input: spec.input.clone(),
                    rate: RATE,
                    channels: CHANNELS,
                })
                .map_err(command_error)
        })
        .collect()
}

pub(crate) fn mixer_inputs(
    specs: &[LiveInputSpec],
    streams: &[Box<dyn AudioStream>],
    tones: &[ToneSpec],
) -> Vec<MixerInput> {
    let live = specs.iter().zip(streams).map(|(spec, stream)| MixerInput {
        name: spec.input.clone().unwrap_or_else(|| "default".to_owned()),
        source: InputSource::Live(stream.samples()),
        strip: StripConfig {
            gain_db: spec.gain_db,
            muted: spec.muted,
        },
    });
    let generated = tones.iter().map(|tone| MixerInput {
        name: format!("tone {} Hz", tone.frequency_hz),
        source: InputSource::Tone(Tone::new(tone.frequency_hz, tone.level_dbfs, RATE)),
        strip: StripConfig {
            gain_db: tone.gain_db,
            muted: tone.muted,
        },
    });
    live.chain(generated).collect()
}

pub(crate) fn stop_captures(
    specs: &[LiveInputSpec],
    streams: Vec<Box<dyn AudioStream>>,
) -> Vec<CaptureOutcome> {
    specs
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
        .collect()
}

/// Warnings about buffering, drift, clipping and timing in a mix.
pub(crate) fn mix_warnings(mix: &audio_engine::MixStats) -> Vec<String> {
    let mut warnings = Vec::new();
    for input in &mix.inputs {
        if input.kind == "live" && input.ring.is_none_or(|ring| ring.pulled_frames == 0) {
            warnings.push(format!(
                "input `{}` delivered no audio (it stayed in warm-up); \
                 see the capture reports for its negotiated format and errors",
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
    warnings
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|err| Value::String(err.to_string()))
}

fn internal(error: impl std::fmt::Display) -> CommandError {
    CommandError::new(ErrorCode::Internal, format!("mixer failed: {error}"))
}
