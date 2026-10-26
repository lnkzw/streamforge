//! A capture backend for the tests: like `Engine::new()`'s, nothing can be
//! captured, but it lists audio inputs (one of them desktop audio), and a
//! listed device reads as unplugged when a pipeline starts, so its channel
//! runs silent.

use capture_api::{
    AudioCaptureReport, AudioCaptureRequest, AudioInput, AudioStream, AudioStreamRequest,
    CaptureBackend, CaptureCapabilities, CaptureError, CaptureErrorKind, ScreenCaptureRequest,
    ScreenSelection, UnsupportedCapture, VideoCaptureReport, VideoStream,
};

pub struct ListedInputs(UnsupportedCapture);

impl ListedInputs {
    pub fn new() -> Self {
        Self(UnsupportedCapture::new("the tests capture nothing"))
    }
}

impl CaptureBackend for ListedInputs {
    fn capabilities(&mut self) -> CaptureCapabilities {
        self.0.capabilities()
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

    fn probe_screen(
        &mut self,
        request: &ScreenCaptureRequest,
    ) -> Result<VideoCaptureReport, CaptureError> {
        self.0.probe_screen(request)
    }

    fn probe_audio(
        &mut self,
        request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError> {
        self.0.probe_audio(request)
    }

    fn start_screen_stream(
        &mut self,
        selection: &ScreenSelection,
    ) -> Result<Box<dyn VideoStream>, CaptureError> {
        self.0.start_screen_stream(selection)
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
}
