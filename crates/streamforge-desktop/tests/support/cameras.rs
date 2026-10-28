//! A camera backend for the tests: two cameras with their modes. A started
//! camera delivers small frames in its own colour at about 30 fps until it
//! stops; an unplugged one is neither listed nor found.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use capture_api::{
    Camera, CameraCapture, CameraCaptureCapability, CameraMode, CameraRequest, CameraStream,
    CaptureError, CaptureErrorKind, CaptureState, VideoCaptureReport, VideoFormatInfo,
    VideoSourceInfo, VideoStatsAccumulator, VideoStream,
};
use media_core::{FrameBuffer, FrameMailbox, PixelFormat, Rational, VideoFrame};

pub const FRONT_CAMERA: &str = "v4l2_input.usb-front";
pub const DESK_CAMERA: &str = "v4l2_input.usb-desk";

/// Id, name, modes (width, height, fps) largest first, and the colour of
/// its frames (BGR).
type Listed = (
    &'static str,
    &'static str,
    &'static [(u32, u32, u32)],
    [u8; 3],
);

const CAMERAS: [Listed; 2] = [
    (
        FRONT_CAMERA,
        "Front Camera",
        &[(1920, 1080, 30), (1280, 720, 30), (640, 480, 30)],
        [60, 140, 230],
    ),
    (
        DESK_CAMERA,
        "Desk Camera",
        &[(1280, 720, 60), (1280, 720, 30)],
        [170, 150, 40],
    ),
];

#[derive(Clone, Default)]
pub struct ListedCameras {
    unplugged: Arc<Mutex<HashSet<String>>>,
}

impl ListedCameras {
    #[allow(dead_code)] // Not every test binary unplugs a camera.
    pub fn plug(&self, camera_id: &str, plugged: bool) {
        let mut unplugged = self.unplugged.lock().expect("unplugged");
        if plugged {
            unplugged.remove(camera_id);
        } else {
            unplugged.insert(camera_id.to_owned());
        }
    }

    fn listed(&self) -> Vec<Listed> {
        let unplugged = self.unplugged.lock().expect("unplugged");
        CAMERAS
            .into_iter()
            .filter(|(id, ..)| !unplugged.contains(*id))
            .collect()
    }
}

impl CameraCapture for ListedCameras {
    fn capability(&mut self) -> CameraCaptureCapability {
        CameraCaptureCapability {
            backend: Some("test".to_owned()),
            available: true,
            unavailable_reason: None,
        }
    }

    fn cameras(&mut self) -> Result<Vec<Camera>, CaptureError> {
        Ok(self
            .listed()
            .into_iter()
            .map(|(id, name, modes, _)| Camera {
                camera_id: id.to_owned(),
                name: name.to_owned(),
                modes: modes
                    .iter()
                    .map(|&(width, height, fps)| CameraMode {
                        width,
                        height,
                        fps: Rational::integer(fps),
                    })
                    .collect(),
            })
            .collect())
    }

    fn start_camera_stream(
        &mut self,
        request: &CameraRequest,
    ) -> Result<CameraStream, CaptureError> {
        let id = request.camera_id.as_str();
        let Some((_, _, modes, [b, g, r])) =
            self.listed().into_iter().find(|(listed, ..)| *listed == id)
        else {
            return Err(CaptureError::new(
                CaptureErrorKind::NotFound,
                format!("camera `{id}` not found; see list_cameras"),
            ));
        };
        let wanted = |&&(width, height, fps): &&(u32, u32, u32)| {
            request.size.is_none_or(|size| size == (width, height))
                && request
                    .fps
                    .is_none_or(|rate| rate == Rational::integer(fps))
        };
        let mut warnings = Vec::new();
        let &(width, height, fps) = modes.iter().find(wanted).unwrap_or_else(|| {
            warnings.push(format!("camera `{id}` has no such mode; using its first"));
            &modes[0]
        });
        let mailbox = Arc::new(FrameMailbox::new());
        let stop = Arc::new(AtomicBool::new(false));
        let (frame_width, frame_height) = (width / 20, height / 20);
        let thread = {
            let mailbox = Arc::clone(&mailbox);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let pixels = [b, g, r, 0].repeat((frame_width * frame_height) as usize);
                    let frame = VideoFrame::new(
                        frame_width,
                        frame_height,
                        frame_width as usize * 4,
                        PixelFormat::Bgrx8,
                        None,
                        FrameBuffer::unpooled(pixels),
                    )
                    .expect("frame");
                    mailbox.publish(frame);
                    thread::sleep(Duration::from_millis(33));
                }
            })
        };
        let source = VideoSourceInfo {
            kind: None,
            width: Some(width),
            height: Some(height),
        };
        Ok(CameraStream {
            stream: Box::new(TestCameraStream {
                mailbox,
                stop,
                thread: Some(thread),
                source,
            }),
            format: Some(VideoFormatInfo {
                pixel_format: "YUY2".to_owned(),
                width,
                height,
                framerate: Rational::integer(fps),
            }),
            warnings,
        })
    }
}

struct TestCameraStream {
    mailbox: Arc<FrameMailbox>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    source: VideoSourceInfo,
}

impl Drop for TestCameraStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl VideoStream for TestCameraStream {
    fn frames(&self) -> Arc<FrameMailbox> {
        Arc::clone(&self.mailbox)
    }

    fn source(&self) -> &VideoSourceInfo {
        &self.source
    }

    fn state(&self) -> CaptureState {
        CaptureState::Running
    }

    fn stop(self: Box<Self>) -> Result<VideoCaptureReport, CaptureError> {
        Ok(VideoCaptureReport {
            backend: "test".to_owned(),
            source: self.source.clone(),
            format: None,
            buffer_memory: None,
            stats: VideoStatsAccumulator::new().finish(Duration::from_millis(1)),
            warnings: Vec::new(),
        })
    }
}
