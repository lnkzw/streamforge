//! A capture backend for the tests: like `Engine::new()`'s, no screen can
//! be captured and nothing played, but it lists audio inputs (one of them
//! desktop audio) and audio outputs, and a listed input reads as unplugged
//! when a pipeline starts, so its channel runs silent. Its cameras
//! (`cameras`) capture.

mod cameras;

// Each test binary uses some of them.
#[allow(unused_imports)]
pub use cameras::{DESK_CAMERA, FRONT_CAMERA, ListedCameras};

use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioDevices, AudioInput,
    AudioOutput, AudioStream, AudioStreamRequest, CaptureError, CaptureErrorKind,
    UnsupportedCapture,
};

pub struct ListedDevices(UnsupportedCapture);

impl ListedDevices {
    pub fn new() -> Self {
        Self(UnsupportedCapture::new("the tests capture nothing"))
    }
}

impl AudioDevices for ListedDevices {
    fn capability(&mut self) -> AudioCaptureCapability {
        self.0.capability()
    }

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
        let input = |name: &str, description: &str, loopback_of: Option<&str>| AudioInput {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            media_class: "Audio/Source".to_owned(),
            loopback_of: loopback_of.map(str::to_owned),
        };
        Ok(vec![
            input("alsa_input.usb-mic", "USB Mic", None),
            input("alsa_input.analog", "Built-in Audio Analog Stereo", None),
            input("speakers.monitor", "Monitor of Speakers", Some("speakers")),
        ])
    }

    fn audio_outputs(&mut self) -> Result<Vec<AudioOutput>, CaptureError> {
        let output = |name: &str, description: &str| AudioOutput {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            media_class: "Audio/Sink".to_owned(),
        };
        Ok(vec![
            output("speakers", "Speakers"),
            output("alsa_output.usb-headset", "USB Headset"),
        ])
    }

    fn probe_audio(
        &mut self,
        request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError> {
        self.0.probe_audio(request)
    }

    fn start_audio_stream(
        &mut self,
        request: &AudioStreamRequest,
    ) -> Result<Box<dyn AudioStream>, CaptureError> {
        Err(CaptureError::new(
            CaptureErrorKind::NotFound,
            format!(
                "{} is unplugged in the tests",
                request.input.as_deref().unwrap_or("the default input")
            ),
        ))
    }

    /// Refused with the text the old trait default gave, so renders of an
    /// ended monitor stay as they were; the refused feed is dropped.
    fn start_playback(
        &mut self,
        _request: &capture_api::PlaybackRequest,
        _feed: media_core::SampleReader,
    ) -> Result<Box<dyn capture_api::PlaybackStream>, CaptureError> {
        Err(CaptureError::unsupported(
            "this backend cannot play audio yet",
        ))
    }
}
