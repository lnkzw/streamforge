//! Every AVFoundation call of this crate, all of it `unsafe` (objc2 marks
//! most AVFoundation methods so): listing cameras and their formats,
//! asking for camera access, and running one camera in a capture session.
//!
//! Invariants kept here:
//! - No Objective-C object leaves the thread that made it (none is
//!   `Send`): the listing builds plain data on its thread, and a camera
//!   thread owns its [`Running`] session.
//! - No call may raise an Objective-C exception, which would abort the
//!   process: inputs and outputs are added only after `canAddInput` and
//!   `canAddOutput`, a format only from the device's own `formats`, a frame
//!   duration only from inside that format's ranges (`camera_modes`) and
//!   while the device is locked for configuration, and the access request
//!   names only video.
//! - The delegate's callbacks end before its state goes: dropping a
//!   [`Running`] stops the session, removes the delegate and runs an empty
//!   block on the delegate's serial queue, which returns once every queued
//!   callback has.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use capture_api::{CaptureError, CaptureErrorKind, CaptureState};
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceDiscoverySession,
    AVCaptureDeviceFormat, AVCaptureDeviceInput, AVCaptureDevicePosition,
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureDeviceTypeContinuityCamera,
    AVCaptureDeviceTypeDeskViewCamera, AVCaptureDeviceTypeExternal, AVCaptureOutput,
    AVCaptureSession, AVCaptureSessionErrorKey, AVCaptureSessionRuntimeErrorNotification,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVError,
    AVFoundationErrorDomain, AVMediaType, AVMediaTypeVideo,
};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetDimensions,
};
use objc2_core_video::{
    CVPixelBuffer, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNotification, NSNotificationCenter, NSNumber, NSString,
};

use crate::camera::Output;
use crate::camera_modes::{
    FormatInfo, MAX_FORMATS, MediaTime, NativeMode, RateRange, selects, shorter,
};

pub(crate) const PERMISSION_REMEDY: &str = "allow the camera for the app that runs StreamForge \
     (System Settings, Privacy & Security, Camera), then restart the source";

fn video() -> Result<&'static AVMediaType, CaptureError> {
    // SAFETY: reading an extern static AVFoundation defines; it is never
    // written.
    unsafe { AVMediaTypeVideo }
        .ok_or_else(|| CaptureError::backend("AVFoundation does not define AVMediaTypeVideo"))
}

/// A camera as AVFoundation lists it.
pub(crate) struct Device {
    device: Retained<AVCaptureDevice>,
}

/// The cameras connected now: built-in, Continuity Camera (reported as
/// built-in unless the app opts in), Desk View and external ones (USB,
/// virtual cameras). Listing asks for no permission.
pub(crate) fn cameras() -> Result<Vec<Device>, CaptureError> {
    let video = video()?;
    // SAFETY: the device types are AVFoundation's own constants (macOS 14,
    // which StreamForge requires); a discovery session is used on the
    // thread that made it.
    let devices = unsafe {
        let types = NSArray::from_slice(&[
            AVCaptureDeviceTypeBuiltInWideAngleCamera,
            AVCaptureDeviceTypeContinuityCamera,
            AVCaptureDeviceTypeDeskViewCamera,
            AVCaptureDeviceTypeExternal,
        ]);
        AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
            &types,
            Some(video),
            AVCaptureDevicePosition::Unspecified,
        )
        .devices()
    };
    Ok(devices.iter().map(|device| Device { device }).collect())
}

fn media_time(time: CMTime) -> Option<MediaTime> {
    time.flags
        .contains(CMTimeFlags::Valid)
        .then_some(MediaTime {
            value: time.value,
            timescale: time.timescale,
        })
}

impl Device {
    /// `AVCaptureDevice.uniqueID`, the `camera_id` (ADR 0019).
    pub(crate) fn id(&self) -> String {
        // SAFETY: a property getter of a live device.
        unsafe { self.device.uniqueID() }.to_string()
    }

    pub(crate) fn name(&self) -> String {
        // SAFETY: a property getter of a live device.
        unsafe { self.device.localizedName() }.to_string()
    }

    /// The device's formats in its order (at most `MAX_FORMATS`); reading
    /// them opens nothing.
    pub(crate) fn formats(&self) -> Vec<FormatInfo> {
        // SAFETY: a property getter of a live device.
        let formats = unsafe { self.device.formats() };
        formats
            .iter()
            .take(MAX_FORMATS)
            .map(|f| format_info(&f))
            .collect()
    }
}

/// What a device format states: its media subtype, size and rate ranges.
fn format_info(format: &AVCaptureDeviceFormat) -> FormatInfo {
    // SAFETY: property getters of a live format; the format description
    // of a video device's format is a video one.
    unsafe {
        let description = format.formatDescription();
        let size = CMVideoFormatDescriptionGetDimensions(&description);
        let ranges = format
            .videoSupportedFrameRateRanges()
            .iter()
            .filter_map(|range| {
                Some(RateRange {
                    min_duration: media_time(range.minFrameDuration())?,
                    max_duration: media_time(range.maxFrameDuration())?,
                })
            })
            .collect();
        FormatInfo {
            subtype: description.media_sub_type(),
            width: size.width,
            height: size.height,
            ranges,
        }
    }
}

/// Whether the app that runs StreamForge may use cameras.
pub(crate) enum Access {
    Granted,
    Denied,
    /// Restricted by a device-management profile or Screen Time.
    Restricted,
    /// Not asked yet; a camera delivers black frames until it is granted.
    Undecided,
}

/// The camera permission, without asking.
pub(crate) fn camera_access() -> Result<Access, CaptureError> {
    let video = video()?;
    // SAFETY: a class method called with AVMediaTypeVideo.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(video) };
    Ok(match status {
        AVAuthorizationStatus::Authorized => Access::Granted,
        AVAuthorizationStatus::Denied => Access::Denied,
        AVAuthorizationStatus::Restricted => Access::Restricted,
        _ => Access::Undecided,
    })
}

/// Ask for the camera permission, which shows macOS's prompt, and wait at
/// most `wait` for the answer; `None` without one.
pub(crate) fn request_camera_access(wait: Duration) -> Result<Option<bool>, CaptureError> {
    let video = video()?;
    let (answer, answered) = mpsc::sync_channel(1);
    let handler = RcBlock::new(move |granted: Bool| {
        let _ = answer.try_send(granted.as_bool());
    });
    // SAFETY: called with AVMediaTypeVideo; the handler is `'static` and
    // only sends on a channel, from whatever queue AVFoundation calls it on.
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(video, &handler) };
    Ok(answered.recv_timeout(wait).ok())
}

/// An AVFoundation error, with its kind where its code says one.
fn ns_error(context: &str, error: &NSError) -> CaptureError {
    let code = error.code();
    // SAFETY: reading an extern static AVFoundation defines.
    let ours = unsafe { AVFoundationErrorDomain }.is_some_and(|domain| *error.domain() == *domain);
    let kind = match AVError(code) {
        _ if !ours => CaptureErrorKind::Unavailable,
        AVError::ApplicationIsNotAuthorizedToUseDevice => CaptureErrorKind::PermissionDenied,
        AVError::DeviceNotConnected | AVError::DeviceWasDisconnected => CaptureErrorKind::NotFound,
        _ => CaptureErrorKind::Unavailable,
    };
    let mut message = format!(
        "{context}: {} ({} {code})",
        error.localizedDescription(),
        error.domain()
    );
    if kind == CaptureErrorKind::PermissionDenied {
        message.push_str("; ");
        message.push_str(PERMISSION_REMEDY);
    }
    CaptureError::new(kind, message)
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `Delegate` does not implement `Drop`; its ivars (`Output`) are
    //   `Sync`, as the callbacks run on AVFoundation's queue.
    #[unsafe(super(NSObject))]
    #[name = "StreamForgeCameraOutput"]
    #[ivars = Output]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for Delegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: Option<&AVCaptureOutput>,
            sample: &CMSampleBuffer,
            _connection: Option<&AVCaptureConnection>,
        ) {
            let output = self.ivars();
            let arrival_ns = output.sink.elapsed_ns();
            // SAFETY: the sample buffer is valid for the callback; its
            // image buffer of a video data output is a pixel buffer.
            let (pts, pixels): (CMTime, Option<_>) =
                unsafe { (sample.presentation_time_stamp(), sample.image_buffer()) };
            let pts_ns = media_time(pts).and_then(crate::camera_modes::media_time_ns);
            let pixels: Option<&CVPixelBuffer> = pixels.as_deref();
            output.frame(arrival_ns, pixels, pts_ns);
        }

        #[unsafe(method(captureOutput:didDropSampleBuffer:fromConnection:))]
        fn did_drop(
            &self,
            _output: Option<&AVCaptureOutput>,
            _sample: Option<&CMSampleBuffer>,
            _connection: Option<&AVCaptureConnection>,
        ) {
            self.ivars().dropped();
        }
    }
);

impl Delegate {
    fn new(output: Output) -> Retained<Self> {
        let this = Self::alloc().set_ivars(output);
        // SAFETY: NSObject's `init` on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// A camera running in its own capture session; dropping it stops the
/// session and waits for the delegate's last callback.
pub(crate) struct Running {
    session: Retained<AVCaptureSession>,
    data: Retained<AVCaptureVideoDataOutput>,
    queue: DispatchRetained<DispatchQueue>,
    observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    device: Retained<AVCaptureDevice>,
    _delegate: Retained<Delegate>,
}

/// The mode a camera runs in after its start, read back from the device.
pub(crate) struct Active {
    pub(crate) format: FormatInfo,
    pub(crate) duration: Option<MediaTime>,
}

/// Open `device` in `mode` and start it; frames go to `output`. A runtime
/// error of the session ends `output`'s sink with AVFoundation's reason.
/// Returns once the session runs (before the first frame).
pub(crate) fn start(
    device: &Device,
    mode: &NativeMode,
    output: Output,
) -> Result<Running, CaptureError> {
    let name = device.name();
    let sink = std::sync::Arc::clone(&output.sink);
    // SAFETY: see the module docs: every object stays on this thread, the
    // format and duration come from the device (`mode` was built from its
    // formats on this thread), and nothing is added without its `canAdd`
    // check.
    unsafe {
        let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&device.device)
            .map_err(|err| ns_error(&format!("cannot open camera `{name}`"), &err))?;
        let session = AVCaptureSession::new();
        let data = AVCaptureVideoDataOutput::new();
        // kCVPixelBufferPixelFormatTypeKey is a CFString, toll-free bridged
        // to NSString.
        let key: &NSString =
            &*(std::ptr::from_ref(kCVPixelBufferPixelFormatTypeKey).cast::<NSString>());
        let bgra = NSNumber::new_u32(kCVPixelFormatType_32BGRA);
        let settings = NSDictionary::<NSString, AnyObject>::from_slices(&[key], &[&*bgra]);
        data.setVideoSettings(Some(&settings));
        // Late frames are dropped by AVFoundation and counted (`did_drop`).
        data.setAlwaysDiscardsLateVideoFrames(true);
        let queue = DispatchQueue::new("org.streamforge.camera", DispatchQueueAttr::SERIAL);
        let delegate = Delegate::new(output);
        data.setSampleBufferDelegate_queue(
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        );
        let mut running = Running {
            session,
            data,
            queue,
            observer: None,
            device: device.device.clone(),
            _delegate: delegate,
        };
        running.observe_runtime_errors(std::sync::Arc::clone(&sink));
        let session = &running.session;
        session.beginConfiguration();
        let added = session.canAddInput(&input) && {
            session.addInput(&input);
            session.canAddOutput(&running.data)
        };
        if !added {
            session.commitConfiguration();
            return Err(CaptureError::new(
                CaptureErrorKind::Unavailable,
                format!("camera `{name}` cannot be added to a capture session"),
            ));
        }
        session.addOutput(&running.data);
        if let Err(err) = device.device.lockForConfiguration() {
            session.commitConfiguration();
            return Err(ns_error(&format!("cannot configure camera `{name}`"), &err));
        }
        // The format is read again under the lock: setting one whose ranges
        // do not hold the duration would raise an exception.
        let formats = device.device.formats();
        let format = (mode.format_index < formats.count())
            .then(|| formats.objectAtIndex(mode.format_index))
            .filter(|format| selects(&format_info(format), mode));
        let Some(format) = format else {
            device.device.unlockForConfiguration();
            session.commitConfiguration();
            return Err(CaptureError::backend(format!(
                "camera `{name}` changed its formats while opening; try again"
            )));
        };
        device.device.setActiveFormat(&format);
        let duration = CMTime {
            value: mode.duration.value,
            timescale: mode.duration.timescale,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        // Never a minimum above the maximum in between: a longer duration
        // than the format's current maximum moves the maximum first.
        let longer = media_time(device.device.activeVideoMaxFrameDuration())
            .is_some_and(|max| shorter(max, mode.duration));
        if longer {
            device.device.setActiveVideoMaxFrameDuration(duration);
            device.device.setActiveVideoMinFrameDuration(duration);
        } else {
            device.device.setActiveVideoMinFrameDuration(duration);
            device.device.setActiveVideoMaxFrameDuration(duration);
        }
        session.commitConfiguration();
        // Locked until it runs, so starting keeps the format and rate.
        session.startRunning();
        device.device.unlockForConfiguration();
        if !session.isRunning() {
            // The runtime error observer may have said why.
            let reason = match sink.state() {
                CaptureState::Ended { reason } => reason,
                CaptureState::Running => "no reason given".to_owned(),
            };
            return Err(CaptureError::new(
                CaptureErrorKind::Unavailable,
                format!("camera `{name}` did not start: {reason}"),
            ));
        }
        Ok(running)
    }
}

impl Running {
    fn observe_runtime_errors(&mut self, sink: std::sync::Arc<capture_api::FrameSink>) {
        let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
            // SAFETY: the notification center passes a valid notification
            // for the block's duration.
            let notification = unsafe { notification.as_ref() };
            sink.end(runtime_error(notification));
        });
        let session: &AnyObject = &self.session;
        // SAFETY: observing a notification AVFoundation posts for this
        // session; the block is `'static` and `Send` (it holds an `Arc`),
        // and is removed when `self` drops.
        self.observer = Some(unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(AVCaptureSessionRuntimeErrorNotification),
                Some(session),
                None,
                &block,
            )
        });
    }

    /// Whether the device is still connected.
    pub(crate) fn is_connected(&self) -> bool {
        // SAFETY: a property getter of a live device.
        unsafe { self.device.isConnected() }
    }

    /// Whether the session still runs (AVFoundation stops it on a runtime
    /// error).
    pub(crate) fn is_running(&self) -> bool {
        // SAFETY: a property getter of a live session on its thread.
        unsafe { self.session.isRunning() }
    }

    /// The format and rate the camera runs in.
    pub(crate) fn active(&self) -> Active {
        // SAFETY: property getters of a live device on its session's thread.
        unsafe {
            Active {
                format: format_info(&self.device.activeFormat()),
                duration: media_time(self.device.activeVideoMinFrameDuration()),
            }
        }
    }
}

/// The reason in a session's runtime error notification.
fn runtime_error(notification: &NSNotification) -> String {
    // SAFETY: reading an extern static AVFoundation defines; the error key
    // of this notification holds an NSError.
    let error = notification
        .userInfo()
        .and_then(|info| info.objectForKey(unsafe { AVCaptureSessionErrorKey }))
        .and_then(|error| error.downcast::<NSError>().ok());
    match error {
        Some(error) => format!(
            "the camera stopped: {} ({} {})",
            error.localizedDescription(),
            error.domain(),
            error.code()
        ),
        None => "the camera stopped with a runtime error".to_owned(),
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // SAFETY: see the module docs; after the empty block on the
        // delegate's queue no callback runs, so the delegate may go.
        unsafe {
            self.session.stopRunning();
            self.data.setSampleBufferDelegate_queue(None, None);
        }
        self.queue.exec_sync(|| {});
        if let Some(observer) = self.observer.take() {
            // SAFETY: removing the observer this session added.
            unsafe { NSNotificationCenter::defaultCenter().removeObserver(observer.as_ref()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use capture_api::{FrameSink, VideoSourceInfo};
    use media_core::MediaTimestamp;
    use objc2_core_foundation::CFRetained;
    use objc2_core_media::{
        CMSampleTimingInfo, CMVideoFormatDescriptionCreateForImageBuffer, kCMTimeInvalid,
    };

    use super::*;
    use crate::camera::Event;
    use crate::pixels::test_buffer;

    /// A ready sample buffer holding `pixels`, presented at `pts`.
    fn sample_buffer(pixels: &CVPixelBuffer, pts: CMTime) -> CFRetained<CMSampleBuffer> {
        let mut description = std::ptr::null();
        // SAFETY: valid out pointers; the created objects are +1 and owned
        // by the returned `CFRetained`s.
        unsafe {
            let status = CMVideoFormatDescriptionCreateForImageBuffer(
                None,
                pixels,
                NonNull::from(&mut description),
            );
            assert_eq!(status, 0);
            let description =
                CFRetained::from_raw(NonNull::new(description.cast_mut()).expect("description"));
            let mut timing = CMSampleTimingInfo {
                duration: CMTime {
                    value: 1,
                    timescale: 30,
                    flags: CMTimeFlags::Valid,
                    epoch: 0,
                },
                presentationTimeStamp: pts,
                decodeTimeStamp: kCMTimeInvalid,
            };
            let mut sample = std::ptr::null_mut();
            let status = CMSampleBuffer::create_ready_with_image_buffer(
                None,
                pixels,
                &description,
                NonNull::from(&mut timing),
                NonNull::from(&mut sample),
            );
            assert_eq!(status, 0);
            CFRetained::from_raw(NonNull::new(sample).expect("sample"))
        }
    }

    #[test]
    fn the_delegate_receives_frames_and_drops_through_the_runtime() {
        let (pixels, _) = test_buffer(4, 2, kCVPixelFormatType_32BGRA);
        let pts = CMTime {
            value: 3,
            timescale: 30,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let sample = sample_buffer(&pixels, pts);
        let (events, received) = mpsc::sync_channel(4);
        // Registers the class, which checks the methods against the
        // protocol's types in debug builds.
        let delegate = Delegate::new(Output::new(Arc::new(FrameSink::new(true, "BGRA")), events));
        let output: Option<&AVCaptureOutput> = None;
        let connection: Option<&AVCaptureConnection> = None;
        // SAFETY: the selectors are the delegate's, with their argument
        // types; AVFoundation would pass the same.
        unsafe {
            let _: () = msg_send![
                &*delegate,
                captureOutput: output,
                didOutputSampleBuffer: &*sample,
                fromConnection: connection
            ];
            let _: () = msg_send![
                &*delegate,
                captureOutput: output,
                didDropSampleBuffer: &*sample,
                fromConnection: connection
            ];
        }
        let sink = &delegate.ivars().sink;
        let frame = sink.mailbox().take().expect("frame");
        assert_eq!((frame.width(), frame.height()), (4, 2));
        assert_eq!(
            frame.pts(),
            MediaTimestamp::new(100_000_000, 1, 1_000_000_000).ok()
        );
        assert!(matches!(received.try_recv(), Ok(Event::Delivered)));
        let report = sink.report(
            "test",
            VideoSourceInfo {
                kind: None,
                width: None,
                height: None,
            },
            "test",
            Vec::new(),
            Duration::from_secs(1),
        );
        assert_eq!((report.stats.frames, report.stats.dropped_frames), (1, 1));
    }

    #[test]
    fn listing_and_access_answer_without_a_camera() {
        // CI runners have no camera; the calls still answer.
        let cameras = cameras().expect("listing");
        for camera in &cameras {
            assert!(!camera.id().is_empty());
        }
        camera_access().expect("access status");
    }
}
