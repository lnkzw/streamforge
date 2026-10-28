//! Live tests against the running desktop session.
//!
//! Ignored by default because they need PipeWire and xdg-desktop-portal, and
//! the audio probe opens a real input device. Playback plays into a null
//! sink the test creates, so nothing is audible; camera tests capture a
//! virtual camera they create. Run with:
//!
//! ```text
//! cargo test -p capture-linux --test live -- --ignored --test-threads=1
//! ```
//!
//! Screen capture needs a human to pick a source in the portal dialog, so it
//! is covered by the manual procedure in `docs/capture-linux.md` instead.
#![cfg(target_os = "linux")]

use std::f32::consts::TAU;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use capture_api::{
    AudioCaptureRequest, AudioDevices, AudioStreamRequest, CameraCapture, CameraRequest, Capture,
    CaptureErrorKind, CaptureState, PlaybackRequest, Rational, VideoCaptureReport,
};
use capture_linux::{PipeWireAudio, PipeWireCamera, PortalScreen};
use media_core::{PixelFormat, VideoFrame, sample_ring};
use pipewire as pw;

fn session_available() -> bool {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let available = runtime_dir
        .as_ref()
        .is_some_and(|dir| std::path::Path::new(dir).join("pipewire-0").exists());
    if !available {
        eprintln!("skipping: no PipeWire socket in $XDG_RUNTIME_DIR (no desktop session)");
    }
    available
}

#[test]
#[ignore = "needs a desktop session with PipeWire and xdg-desktop-portal"]
fn capabilities_report_running_services() {
    if !session_available() {
        return;
    }
    // Through `Capture`, as the engine queries them: the portal and the
    // cameras on their own threads, audio on this one.
    let caps = Capture {
        screen: Box::new(PortalScreen::new()),
        audio: Box::new(PipeWireAudio::new()),
        camera: Box::new(PipeWireCamera::new()),
    }
    .capabilities();
    assert_eq!(caps.platform, "linux");
    assert!(caps.audio.available, "{:?}", caps.audio.unavailable_reason);
    assert!(
        caps.camera.available,
        "{:?}",
        caps.camera.unavailable_reason
    );
    assert_eq!(caps.camera.backend.as_deref(), Some("pipewire"));
    assert!(caps.audio.server_version.is_some());
    assert!(
        caps.screen.available,
        "{:?}",
        caps.screen.unavailable_reason
    );
    assert!(!caps.screen.target_kinds.is_empty());
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn audio_inputs_are_listed_with_names() {
    if !session_available() {
        return;
    }
    let inputs = PipeWireAudio::new().audio_inputs().expect("list inputs");
    for input in &inputs {
        assert!(!input.name.is_empty());
        assert!(input.media_class.starts_with("Audio/"), "{input:?}");
    }
    eprintln!("found {} audio input(s)", inputs.len());
}

#[test]
#[ignore = "opens a real audio input for 0.5 s (statistics only, nothing is stored)"]
fn audio_probe_reports_format_and_positions() {
    if !session_available() {
        return;
    }
    let mut backend = PipeWireAudio::new();
    let Some(input) = backend
        .audio_inputs()
        .expect("list inputs")
        .into_iter()
        .next()
    else {
        eprintln!("skipping: no audio input device present");
        return;
    };
    let report = backend
        .probe_audio(&AudioCaptureRequest {
            input: Some(input.name.clone()),
            duration: Duration::from_millis(500),
        })
        .expect("probe");
    let format = report.format.expect("negotiated format");
    assert_eq!(format.sample_format, "f32le");
    assert!(format.sample_rate_hz > 0 && format.channels > 0);
    assert!(report.stats.frames > 0, "{:?}", report.stats);
    let first = report.stats.first_position.expect("first position");
    let last = report.stats.last_position.expect("last position");
    assert!(last.ticks >= first.ticks);
    assert_eq!(report.stats.buffer_starvation, 0);
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn unknown_audio_input_is_not_found() {
    if !session_available() {
        return;
    }
    let err = PipeWireAudio::new()
        .probe_audio(&AudioCaptureRequest {
            input: Some("streamforge.test.does-not-exist".into()),
            duration: Duration::from_millis(100),
        })
        .unwrap_err();
    assert_eq!(err.kind, capture_api::CaptureErrorKind::NotFound);
    assert!(err.details.is_some());
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn audio_outputs_are_listed_with_their_monitors() {
    if !session_available() {
        return;
    }
    let mut backend = PipeWireAudio::new();
    let outputs = backend.audio_outputs().expect("list outputs");
    let inputs = backend.audio_inputs().expect("list inputs");
    for output in &outputs {
        assert!(!output.name.is_empty());
        assert!(output.media_class.starts_with("Audio/"), "{output:?}");
        if output.media_class.starts_with("Audio/Sink") {
            let monitor = format!("{}.monitor", output.name);
            assert!(
                inputs.iter().any(|input| input.name == monitor
                    && input.loopback_of.as_ref() == Some(&output.name)),
                "no monitor input for {output:?}"
            );
        }
    }
    eprintln!("found {} audio output(s)", outputs.len());
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn unknown_audio_output_is_not_found() {
    if !session_available() {
        return;
    }
    let (writer, reader) = sample_ring(2, 4_800);
    let request = PlaybackRequest {
        output: Some("streamforge.test.does-not-exist".into()),
        rate: 48_000,
        channels: 2,
    };
    let Err(err) = PipeWireAudio::new().start_playback(&request, reader) else {
        panic!("played on an unknown output");
    };
    assert_eq!(err.kind, CaptureErrorKind::NotFound);
    assert!(err.details.is_some());
    assert!(writer.is_closed());
}

/// A PipeWire node this test owns: a null sink nobody hears, for playback,
/// or a camera. It lives while its client, a thread here, stays connected.
struct TestNode {
    name: String,
    stop: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

/// What a test node's thread keeps alive, and what it calls every 33 ms.
type Built = (Box<dyn std::any::Any>, Option<Box<dyn Fn()>>);

impl TestNode {
    fn sink() -> Self {
        Self::spawn("streamforge-test-sink", |core, name| {
            let sink: pw::node::Node = core
                .create_object(
                    "adapter",
                    &pw::properties::properties! {
                        "factory.name" => "support.null-audio-sink",
                        *pw::keys::NODE_NAME => name,
                        *pw::keys::NODE_DESCRIPTION => "StreamForge test sink",
                        *pw::keys::MEDIA_CLASS => "Audio/Sink",
                        "audio.position" => "[ FL FR ]",
                    },
                )
                .expect("create the sink");
            (Box::new(sink), None)
        })
    }

    /// A camera as virtual cameras are made: an output stream that drives
    /// itself at 30 fps with one fixed mode. (PipeWire's `videotestsrc`
    /// node would be simpler, but PipeWire 1.6 aborts when one is
    /// destroyed.)
    fn camera(format: TestFormat) -> Self {
        Self::spawn("streamforge-test-camera", move |core, name| {
            let stream = pw::stream::StreamRc::new(
                core.clone(),
                "streamforge-test-camera",
                pw::properties::properties! {
                    *pw::keys::NODE_NAME => name,
                    *pw::keys::NODE_DESCRIPTION => "StreamForge test camera",
                    *pw::keys::MEDIA_TYPE => "Video",
                    *pw::keys::MEDIA_CLASS => "Video/Source",
                    *pw::keys::MEDIA_ROLE => "Camera",
                },
            )
            .expect("create the camera stream");
            let (frame, stride) = format.frame();
            let buffers = buffers_pod(frame.len(), stride);
            let listener = stream
                .add_local_listener_with_user_data(())
                .param_changed(move |stream, _, id, param| {
                    if id == pw::spa::param::ParamType::Format.as_raw() && param.is_some() {
                        let pod = pw::spa::pod::Pod::from_bytes(&buffers).expect("pod");
                        stream.update_params(&mut [pod]).expect("buffer params");
                    }
                })
                .process(move |stream, _| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    let Some(data) = buffer.datas_mut().first_mut() else {
                        return;
                    };
                    let Some(memory) = data.data() else {
                        return;
                    };
                    let size = frame.len().min(memory.len());
                    memory[..size].copy_from_slice(&frame[..size]);
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.size_mut() = size as u32;
                    *chunk.stride_mut() = stride;
                })
                .register()
                .expect("camera listener");
            let format_pod = format.enum_format();
            stream
                .connect(
                    pw::spa::utils::Direction::Output,
                    None,
                    pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut [pw::spa::pod::Pod::from_bytes(&format_pod).expect("pod")],
                )
                .expect("connect the camera stream");
            let tick = {
                let stream = stream.clone();
                Box::new(move || {
                    let _ = stream.trigger_process();
                }) as Box<dyn Fn()>
            };
            (Box::new((stream, listener)), Some(tick))
        })
    }

    /// Run `build` on a thread with its own PipeWire connection until the
    /// node is removed.
    fn spawn(
        prefix: &str,
        build: impl FnOnce(&pw::core::CoreRc, &str) -> Built + Send + 'static,
    ) -> Self {
        let name = format!("{prefix}-{}", std::process::id());
        let (stop, stopped) = pw::channel::channel::<()>();
        let (ready, created) = mpsc::channel();
        let thread = thread::spawn({
            let name = name.clone();
            move || {
                pw::init();
                let main_loop = pw::main_loop::MainLoopRc::new(None).expect("main loop");
                let context = pw::context::ContextRc::new(&main_loop, None).expect("context");
                let core = context.connect_rc(None).expect("connect to PipeWire");
                let (objects, tick) = build(&core, &name);
                let timer = tick.map(|tick| {
                    let timer = main_loop.loop_().add_timer(move |_| tick());
                    timer
                        .update_timer(
                            Some(Duration::from_millis(1)),
                            Some(Duration::from_millis(33)),
                        )
                        .into_sync_result()
                        .expect("arm the timer");
                    timer
                });
                let _stopped = stopped.attach(main_loop.loop_(), {
                    let main_loop = main_loop.clone();
                    move |()| main_loop.quit()
                });
                ready.send(()).expect("test waits");
                main_loop.run();
                drop(timer);
                drop(objects);
            }
        });
        created.recv().expect("the node thread failed");
        Self {
            name,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    /// Remove the node, as when a device is unplugged.
    fn remove(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("node thread");
        }
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Poll `done` every 20 ms until it holds or `timeout` passes.
fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !done() {
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
#[ignore = "creates a temporary PipeWire null sink (nothing is audible) and plays a tone into it"]
fn playback_plays_on_a_sink_and_ends_with_it() {
    if !session_available() {
        return;
    }
    let mut sink = TestNode::sink();
    let mut backend = PipeWireAudio::new();
    assert!(
        wait_until(Duration::from_secs(3), || {
            let outputs = backend.audio_outputs().expect("list outputs");
            outputs.iter().any(|output| output.name == sink.name)
        }),
        "the test sink was not listed"
    );
    let monitor = backend
        .audio_inputs()
        .expect("list inputs")
        .into_iter()
        .find(|input| input.loopback_of.as_ref() == Some(&sink.name))
        .expect("the sink's monitor is an input");
    assert_eq!(monitor.name, format!("{}.monitor", sink.name));

    // Capture what the sink plays, then play a -20 dBFS tone into it.
    let capture = backend
        .start_audio_stream(&AudioStreamRequest {
            input: Some(monitor.name),
            rate: 48_000,
            channels: 2,
        })
        .expect("capture the monitor");
    let (mut writer, reader) = sample_ring(2, 24_000);
    let request = PlaybackRequest {
        output: Some(sink.name.clone()),
        rate: 48_000,
        channels: 2,
    };
    let playback = backend.start_playback(&request, reader).expect("play");
    assert_eq!(playback.state(), CaptureState::Running);
    let mut phase = 0.0_f32;
    let mut block = vec![0.0; 480 * 2];
    let mut captured = vec![0.0; 48_000 * 2];
    let mut peak = 0.0_f32;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(1) {
        while writer.fill_frames() < 4_800 {
            for frame in block.as_chunks_mut::<2>().0 {
                frame.fill(0.1 * (phase * TAU).sin());
                phase = (phase + 440.0 / 48_000.0).fract();
            }
            writer.write(&block);
        }
        let ring = capture.samples();
        loop {
            let frames = ring.pull(&mut captured);
            if frames == 0 {
                break;
            }
            peak = captured[..frames * 2]
                .iter()
                .fold(peak, |peak, sample| peak.max(sample.abs()));
        }
        thread::sleep(Duration::from_millis(5));
    }
    let stats = writer.stats();
    eprintln!("feed after 1 s: {stats:?}; captured peak {peak}");
    assert!((0.08..=0.12).contains(&peak), "captured peak {peak}");
    assert!(stats.read_frames >= 24_000, "{stats:?}");

    // Unplugged: the playback ends and closes its feed.
    sink.remove();
    assert!(
        wait_until(Duration::from_secs(3), || playback.state()
            != CaptureState::Running),
        "the playback did not end with its sink"
    );
    let state = playback.state();
    eprintln!("after the sink went away: {state:?}");
    // WirePlumber reports the missing target (`... is gone`), or the stream
    // just disconnects (`... went away`); either names the output.
    let CaptureState::Ended { reason } = state else {
        unreachable!()
    };
    assert!(reason.contains(&sink.name), "{reason}");
    assert!(writer.is_closed());
    playback.stop().expect("stop after the end");
    let _ = capture.stop();
}

#[test]
#[ignore = "creates and removes a temporary PipeWire null sink; nothing is audible"]
fn an_output_gone_since_the_listing_does_not_fall_back_to_the_default() {
    if !session_available() {
        return;
    }
    let mut sink = TestNode::sink();
    let mut backend = PipeWireAudio::new();
    assert!(
        wait_until(Duration::from_secs(3), || {
            let outputs = backend.audio_outputs().expect("list outputs");
            outputs.iter().any(|output| output.name == sink.name)
        }),
        "the test sink was not listed"
    );
    // The listing is still fresh enough for the name check; PipeWire must
    // not play on the default sink instead.
    sink.remove();
    let (writer, reader) = sample_ring(2, 4_800);
    let request = PlaybackRequest {
        output: Some(sink.name.clone()),
        rate: 48_000,
        channels: 2,
    };
    let Err(err) = backend.start_playback(&request, reader) else {
        panic!("played although the output is gone");
    };
    eprintln!("{err:?}");
    assert_eq!(err.kind, CaptureErrorKind::NotFound, "{err:?}");
    assert!(writer.is_closed());
}

/// The one mode a test camera offers.
#[derive(Debug, Clone, Copy)]
enum TestFormat {
    /// 640x480, every pixel the same red.
    Yuy2,
    /// 640x480, every pixel the same red.
    Nv12,
    /// 1920x1080, the `frame-decode` camera fixture.
    Mjpeg,
}

const MJPEG_FRAME: &[u8] = include_bytes!("../../frame-decode/tests/data/camera-1080p-422.jpg");
/// Pure red in BT.601 limited range, the default for formats that do not say.
const RED_YUV: (u8, u8, u8) = (81, 90, 240);

impl TestFormat {
    fn size(self) -> (u32, u32) {
        match self {
            Self::Yuy2 | Self::Nv12 => (640, 480),
            Self::Mjpeg => (1920, 1080),
        }
    }

    /// One frame and its row stride.
    fn frame(self) -> (Vec<u8>, i32) {
        let (width, height) = self.size();
        let (w, h) = (width as usize, height as usize);
        let (y, u, v) = RED_YUV;
        match self {
            Self::Yuy2 => ([y, u, y, v].repeat(w / 2 * h), (w * 2) as i32),
            Self::Nv12 => {
                let mut frame = vec![y; w * h];
                frame.extend([u, v].repeat(w / 2 * h / 2));
                (frame, w as i32)
            }
            Self::Mjpeg => (MJPEG_FRAME.to_vec(), 0),
        }
    }

    fn enum_format(self) -> Vec<u8> {
        use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
        use pw::spa::pod::{Object, Property, Value};
        use pw::spa::utils::{Fraction, Id, Rectangle, SpaTypes};

        let (width, height) = self.size();
        let id =
            |key: FormatProperties, value: u32| Property::new(key.as_raw(), Value::Id(Id(value)));
        let mut properties = vec![id(FormatProperties::MediaType, MediaType::Video.as_raw())];
        match self {
            Self::Yuy2 | Self::Nv12 => {
                properties.push(id(
                    FormatProperties::MediaSubtype,
                    MediaSubtype::Raw.as_raw(),
                ));
                let format = match self {
                    Self::Yuy2 => pw::spa::sys::SPA_VIDEO_FORMAT_YUY2,
                    _ => pw::spa::sys::SPA_VIDEO_FORMAT_NV12,
                };
                properties.push(id(FormatProperties::VideoFormat, format));
            }
            Self::Mjpeg => {
                properties.push(id(
                    FormatProperties::MediaSubtype,
                    MediaSubtype::Mjpg.as_raw(),
                ));
            }
        }
        properties.push(Property::new(
            FormatProperties::VideoSize.as_raw(),
            Value::Rectangle(Rectangle { width, height }),
        ));
        properties.push(Property::new(
            FormatProperties::VideoFramerate.as_raw(),
            Value::Fraction(Fraction { num: 30, denom: 1 }),
        ));
        serialize(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: pw::spa::param::ParamType::EnumFormat.as_raw(),
            properties,
        })
    }
}

/// One block per buffer, large enough for a frame.
fn buffers_pod(size: usize, stride: i32) -> Vec<u8> {
    use pw::spa::pod::{Object, Property, Value};
    let int = |key: u32, value: i32| Property::new(key, Value::Int(value));
    serialize(Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: pw::spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            int(pw::spa::sys::SPA_PARAM_BUFFERS_buffers, 4),
            int(pw::spa::sys::SPA_PARAM_BUFFERS_blocks, 1),
            int(pw::spa::sys::SPA_PARAM_BUFFERS_size, size as i32),
            int(pw::spa::sys::SPA_PARAM_BUFFERS_stride, stride),
        ],
    })
}

fn serialize(object: pw::spa::pod::Object) -> Vec<u8> {
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(object),
    )
    .expect("serialize")
    .0
    .into_inner()
}

#[test]
#[ignore = "creates a temporary PipeWire camera"]
fn a_test_camera_is_listed_with_its_mode() {
    if !session_available() {
        return;
    }
    let node = TestNode::camera(TestFormat::Yuy2);
    let cameras = PipeWireCamera::new().cameras().expect("cameras");
    let camera = cameras
        .iter()
        .find(|camera| camera.camera_id == node.name)
        .unwrap_or_else(|| panic!("{} not in {cameras:?}", node.name));
    assert_eq!(camera.name, "StreamForge test camera");
    assert_eq!(camera.modes.len(), 1, "{:?}", camera.modes);
    let mode = &camera.modes[0];
    assert_eq!(
        (mode.width, mode.height, mode.fps),
        (640, 480, Rational::integer(30))
    );
}

/// Capture a test camera for two seconds; returns the last frame and the
/// report.
fn capture_test_camera(format: TestFormat) -> (VideoFrame, VideoCaptureReport) {
    let node = TestNode::camera(format);
    let stream = PipeWireCamera::new()
        .start_camera_stream(&CameraRequest {
            camera_id: node.name.clone(),
            size: None,
            fps: None,
        })
        .unwrap_or_else(|err| panic!("start the {format:?} camera: {err:?}"));
    let (width, height) = format.size();
    assert_eq!(
        (stream.source().width, stream.source().height),
        (Some(width), Some(height))
    );
    let frames = stream.frames();
    let mut last = None;
    let mut seen = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(frame) = frames.take() {
            assert_eq!((frame.width(), frame.height()), (width, height));
            assert_eq!(frame.format(), PixelFormat::Bgrx8);
            last = Some(frame);
            seen += 1;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(stream.state(), CaptureState::Running);
    let report = stream.stop().expect("report");
    // The test camera attaches no buffer headers; V4L2 cameras do.
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| warning.contains("no presentation timestamps")),
        "{:?}",
        report.warnings
    );
    // About 60 frames in two seconds; a loaded machine delivers fewer.
    assert!(report.stats.frames >= 20, "{:?}", report.stats);
    assert_eq!(report.stats.frames_discarded, 0, "{:?}", report.stats);
    assert!(seen >= 10, "took {seen} frames");
    (last.expect("a frame"), report)
}

/// The BGRX pixel in the middle of `frame`.
fn middle(frame: &VideoFrame) -> [u8; 4] {
    let (x, y) = (frame.width() as usize / 2, frame.height() as usize / 2);
    let at = y * frame.stride() + x * 4;
    frame.data()[at..at + 4].try_into().expect("pixel")
}

#[test]
#[ignore = "creates a temporary PipeWire camera and captures it for two seconds"]
fn a_yuy2_camera_delivers_converted_frames() {
    if !session_available() {
        return;
    }
    let (frame, report) = capture_test_camera(TestFormat::Yuy2);
    let format = report.format.expect("negotiated format");
    assert_eq!(format.pixel_format, "YUY2");
    assert_eq!(format.framerate, Rational::integer(30));
    let [b, g, r, x] = middle(&frame);
    assert!(
        b <= 2 && g <= 2 && r >= 252 && x == 255,
        "{:?}",
        [b, g, r, x]
    );
}

#[test]
#[ignore = "creates a temporary PipeWire camera and captures it for two seconds"]
fn an_nv12_camera_delivers_converted_frames() {
    if !session_available() {
        return;
    }
    let (frame, report) = capture_test_camera(TestFormat::Nv12);
    assert_eq!(report.format.expect("format").pixel_format, "NV12");
    let [b, g, r, x] = middle(&frame);
    assert!(
        b <= 2 && g <= 2 && r >= 252 && x == 255,
        "{:?}",
        [b, g, r, x]
    );
}

#[test]
#[ignore = "creates a temporary PipeWire camera and captures it for two seconds"]
fn an_mjpeg_camera_delivers_decoded_frames() {
    if !session_available() {
        return;
    }
    let (frame, report) = capture_test_camera(TestFormat::Mjpeg);
    assert_eq!(report.format.expect("format").pixel_format, "MJPG");
    let image = frame_decode::decode_image(MJPEG_FRAME, frame_decode::Limits::IMAGE)
        .expect("decode the fixture");
    // The same decoder: BGRX with X = 255 equals opaque premultiplied BGRA.
    assert_eq!(middle(&frame), middle(&image));
}

#[test]
#[ignore = "creates and removes a temporary PipeWire camera"]
fn a_removed_camera_ends_its_capture() {
    if !session_available() {
        return;
    }
    let mut node = TestNode::camera(TestFormat::Yuy2);
    let stream = PipeWireCamera::new()
        .start_camera_stream(&CameraRequest {
            camera_id: node.name.clone(),
            size: None,
            fps: None,
        })
        .expect("start the camera");
    node.remove();
    assert!(
        wait_until(Duration::from_secs(3), || stream.state()
            != CaptureState::Running),
        "the capture still runs"
    );
    let CaptureState::Ended { reason } = stream.state() else {
        unreachable!();
    };
    assert_eq!(
        reason,
        format!("camera `{}` is gone; see list_cameras", node.name)
    );
    let _ = stream.stop();
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn an_unknown_camera_is_not_found() {
    if !session_available() {
        return;
    }
    let Err(err) = PipeWireCamera::new().start_camera_stream(&CameraRequest {
        camera_id: "streamforge-no-such-camera".into(),
        size: None,
        fps: None,
    }) else {
        panic!("captured an unknown camera");
    };
    assert_eq!(err.kind, CaptureErrorKind::NotFound);
    assert!(err.details.is_some());
}
