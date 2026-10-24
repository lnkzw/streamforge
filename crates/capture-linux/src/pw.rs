//! PipeWire client connection helpers.
//!
//! Each operation creates its own main loop on the calling thread and runs it
//! for a bounded time. PipeWire objects are `!Send`, so they never leave the
//! function that created them.

use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use capture_api::{AudioInput, CaptureError};
use pipewire as pw;
use pw::context::ContextRc;
use pw::core::CoreRc;
use pw::main_loop::MainLoopRc;
use pw::stream::{Stream, StreamListener, StreamState};
use pw::types::ObjectType;

pub(crate) const BACKEND: &str = "pipewire";

/// PipeWire client: main loop, context and core connection.
pub(crate) struct Connection {
    // Declaration order is drop order: core before context before loop.
    pub core: CoreRc,
    _context: ContextRc,
    pub main_loop: MainLoopRc,
}

impl Connection {
    /// Connect to the user's PipeWire daemon.
    pub fn connect() -> Result<Self, CaptureError> {
        Self::open(None)
    }

    /// Connect through a file descriptor handed out by a portal.
    pub fn connect_fd(fd: OwnedFd) -> Result<Self, CaptureError> {
        Self::open(Some(fd))
    }

    fn open(fd: Option<OwnedFd>) -> Result<Self, CaptureError> {
        pw::init();
        let main_loop = MainLoopRc::new(None).map_err(|err| {
            CaptureError::unavailable(format!("cannot create a PipeWire main loop: {err}"))
        })?;
        let context = ContextRc::new(&main_loop, None).map_err(|err| {
            CaptureError::unavailable(format!("cannot create a PipeWire context: {err}"))
        })?;
        let core = match fd {
            Some(fd) => context.connect_fd_rc(fd, None),
            None => context.connect_rc(None),
        }
        .map_err(|err| {
            CaptureError::unavailable(format!(
                "cannot connect to PipeWire ({err}); is the PipeWire daemon running? \
                 Check with `systemctl --user status pipewire`"
            ))
        })?;
        Ok(Self {
            core,
            _context: context,
            main_loop,
        })
    }

    /// Run the main loop until a callback quits it or `timeout` elapses.
    /// Returns `true` if the timeout ended the run.
    pub fn run_for(&self, timeout: Duration) -> Result<bool, CaptureError> {
        let timed_out = Rc::new(Cell::new(false));
        let timer = self.main_loop.loop_().add_timer({
            let main_loop = self.main_loop.clone();
            let timed_out = Rc::clone(&timed_out);
            move |_| {
                timed_out.set(true);
                main_loop.quit();
            }
        });
        timer
            .update_timer(Some(timeout), None)
            .into_sync_result()
            .map_err(|err| CaptureError::backend(format!("cannot arm PipeWire timer: {err}")))?;
        self.main_loop.run();
        Ok(timed_out.get())
    }

    /// Run the main loop until a message arrives on `stop` or a callback
    /// quits it. Returns `true` if `stop` ended the run.
    pub fn run_until_stopped(&self, stop: pw::channel::Receiver<()>) -> bool {
        let stopped = Rc::new(Cell::new(false));
        let _attached = stop.attach(self.main_loop.loop_(), {
            let main_loop = self.main_loop.clone();
            let stopped = Rc::clone(&stopped);
            move |()| {
                stopped.set(true);
                main_loop.quit();
            }
        });
        self.main_loop.run();
        stopped.get()
    }

    /// Wait until the server has processed everything sent so far, so that
    /// e.g. all registry globals have been announced.
    pub fn roundtrip(&self, timeout: Duration) -> Result<(), CaptureError> {
        let pending = self
            .core
            .sync(0)
            .map_err(|err| CaptureError::backend(format!("PipeWire sync failed: {err}")))?;
        let done = Rc::new(Cell::new(false));
        let error = Rc::new(RefCell::new(None));
        let _listener = self
            .core
            .add_listener_local()
            .done({
                let done = Rc::clone(&done);
                let main_loop = self.main_loop.clone();
                move |id, seq| {
                    if id == pw::core::PW_ID_CORE && seq == pending {
                        done.set(true);
                        main_loop.quit();
                    }
                }
            })
            .error({
                let error = Rc::clone(&error);
                let main_loop = self.main_loop.clone();
                move |id, _seq, res, message| {
                    *error.borrow_mut() =
                        Some(format!("PipeWire error on object {id}: {message} ({res})"));
                    main_loop.quit();
                }
            })
            .register();
        self.run_for(timeout)?;
        if let Some(message) = error.take() {
            return Err(CaptureError::backend(message));
        }
        if !done.get() {
            return Err(CaptureError::timeout(format!(
                "PipeWire did not answer within {} ms",
                timeout.as_millis()
            )));
        }
        Ok(())
    }
}

/// When a capture stream ends normally.
pub(crate) enum RunUntil {
    /// After a fixed probe duration.
    Elapsed(Duration),
    /// When the owner sends a stop message.
    Stopped(pw::channel::Receiver<()>),
}

#[derive(Default)]
struct Lifecycle {
    reached_streaming: bool,
    error: Option<String>,
}

/// Lifecycle supervision shared by all capture streams: records whether the
/// stream reached `Streaming`, stops the loop on stream or core errors and
/// when a streaming source disappears.
///
/// Attach before `Stream::connect` so no state change is missed.
pub(crate) struct StreamWatch {
    lifecycle: Rc<RefCell<Lifecycle>>,
    _core_listener: pw::core::Listener,
    _stream_listener: StreamListener<()>,
}

impl StreamWatch {
    pub fn attach(connection: &Connection, stream: &Stream) -> Result<Self, CaptureError> {
        let lifecycle = Rc::new(RefCell::new(Lifecycle::default()));
        let core_listener = connection
            .core
            .add_listener_local()
            .error({
                let lifecycle = Rc::clone(&lifecycle);
                let main_loop = connection.main_loop.clone();
                move |id, _seq, res, message| {
                    lifecycle.borrow_mut().error =
                        Some(format!("PipeWire error on object {id}: {message} ({res})"));
                    main_loop.quit();
                }
            })
            .register();
        let stream_listener = stream
            .add_local_listener_with_user_data(())
            .state_changed({
                let lifecycle = Rc::clone(&lifecycle);
                let main_loop = connection.main_loop.clone();
                move |_, _, _old, new| {
                    let mut lifecycle = lifecycle.borrow_mut();
                    match new {
                        StreamState::Streaming => lifecycle.reached_streaming = true,
                        StreamState::Error(message) => {
                            lifecycle.error = Some(format!("PipeWire stream error: {message}"));
                            main_loop.quit();
                        }
                        // The source disappeared or the user stopped sharing.
                        StreamState::Unconnected if lifecycle.reached_streaming => main_loop.quit(),
                        _ => {}
                    }
                }
            })
            .register()
            .map_err(|err| {
                CaptureError::backend(format!("cannot register stream state listener: {err}"))
            })?;
        Ok(Self {
            lifecycle,
            _core_listener: core_listener,
            _stream_listener: stream_listener,
        })
    }

    /// Run the loop until `until` (or until the stream ends), then disconnect.
    pub fn run(
        self,
        connection: &Connection,
        stream: &Stream,
        until: RunUntil,
        start: Instant,
    ) -> Result<StreamOutcome, CaptureError> {
        let (ran_full_duration, duration) = match until {
            RunUntil::Elapsed(duration) => (connection.run_for(duration)?, duration),
            RunUntil::Stopped(stop) => (connection.run_until_stopped(stop), start.elapsed()),
        };
        let elapsed = start.elapsed();
        // Disconnecting emits a final state change; the loop is no longer running.
        let disconnect_error = stream
            .disconnect()
            .err()
            .map(|err| format!("disconnecting the stream failed: {err}"));
        let lifecycle = self.lifecycle.take();
        Ok(StreamOutcome {
            duration,
            elapsed,
            reached_streaming: lifecycle.reached_streaming,
            ran_full_duration,
            error: lifecycle.error,
            disconnect_error,
        })
    }
}

pub(crate) struct StreamOutcome {
    duration: Duration,
    elapsed: Duration,
    reached_streaming: bool,
    ran_full_duration: bool,
    error: Option<String>,
    disconnect_error: Option<String>,
}

impl StreamOutcome {
    /// Turn lifecycle problems into an error or warnings and return the
    /// capture window length. Warnings gathered so far (e.g. format
    /// negotiation failures) travel in the error's details so no cause is
    /// lost. `map_error` converts a backend error message into a `CaptureError`.
    pub fn finish(
        self,
        what: &str,
        warnings: &mut Vec<String>,
        map_error: impl FnOnce(String) -> CaptureError,
    ) -> Result<Duration, CaptureError> {
        warnings.extend(self.disconnect_error);
        let with_warnings = |error: CaptureError, warnings: &[String]| {
            if warnings.is_empty() {
                error
            } else {
                error.with_detail("warnings", warnings.to_vec())
            }
        };
        if let Some(message) = self.error {
            return Err(with_warnings(map_error(message), warnings));
        }
        if !self.reached_streaming {
            return Err(with_warnings(
                CaptureError::timeout(format!(
                    "the {what} stream did not start within {} ms",
                    self.duration.as_millis()
                )),
                warnings,
            ));
        }
        if !self.ran_full_duration {
            warnings.push("the stream ended before the probe duration elapsed".to_owned());
        }
        Ok(self.elapsed)
    }
}

/// Version string reported by the PipeWire server.
pub(crate) fn server_version(timeout: Duration) -> Result<String, CaptureError> {
    let connection = Connection::connect()?;
    let version = Rc::new(RefCell::new(None));
    let _listener = connection
        .core
        .add_listener_local()
        .info({
            let version = Rc::clone(&version);
            move |info| *version.borrow_mut() = Some(info.version().to_owned())
        })
        .register();
    connection.roundtrip(timeout)?;
    version
        .take()
        .ok_or_else(|| CaptureError::backend("PipeWire server did not send core info"))
}

/// Audio source nodes currently known to the PipeWire server, sorted by name.
pub(crate) fn audio_inputs(timeout: Duration) -> Result<Vec<AudioInput>, CaptureError> {
    let connection = Connection::connect()?;
    let registry = connection
        .core
        .get_registry()
        .map_err(|err| CaptureError::backend(format!("cannot get PipeWire registry: {err}")))?;
    let inputs = Rc::new(RefCell::new(Vec::new()));
    let _listener = registry
        .add_listener_local()
        .global({
            let inputs = Rc::clone(&inputs);
            move |global| {
                if global.type_ != ObjectType::Node {
                    return;
                }
                let Some(props) = global.props else {
                    return;
                };
                let (Some(class), Some(name)) = (
                    props.get(*pw::keys::MEDIA_CLASS),
                    props.get(*pw::keys::NODE_NAME),
                ) else {
                    return;
                };
                if is_audio_input_class(class) {
                    inputs.borrow_mut().push(AudioInput {
                        name: name.to_owned(),
                        description: props.get(*pw::keys::NODE_DESCRIPTION).map(str::to_owned),
                        media_class: class.to_owned(),
                        // Sources only: no sink monitors listed.
                        loopback_of: None,
                    });
                }
            }
        })
        .register();
    connection.roundtrip(timeout)?;
    let mut inputs = inputs.take();
    inputs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(inputs)
}

/// Capture-capable audio nodes: hardware and virtual sources, duplex devices.
fn is_audio_input_class(class: &str) -> bool {
    class == "Audio/Source" || class.starts_with("Audio/Source/") || class == "Audio/Duplex"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(
        reached_streaming: bool,
        ran_full_duration: bool,
        error: Option<&str>,
    ) -> StreamOutcome {
        StreamOutcome {
            duration: Duration::from_millis(500),
            elapsed: Duration::from_millis(420),
            reached_streaming,
            ran_full_duration,
            error: error.map(str::to_owned),
            disconnect_error: Some("disconnect failed".to_owned()),
        }
    }

    #[test]
    fn errors_carry_earlier_warnings() {
        let mut warnings = vec!["format rejected".to_owned()];
        let err = outcome(true, false, Some("boom"))
            .finish("audio", &mut warnings, CaptureError::backend)
            .unwrap_err();
        assert_eq!(err.message, "boom");
        assert_eq!(
            err.details,
            Some(serde_json::json!({"warnings": ["format rejected", "disconnect failed"]}))
        );
    }

    #[test]
    fn stream_that_never_started_times_out_with_warnings() {
        let mut warnings = vec!["format rejected".to_owned()];
        let err = outcome(false, true, None)
            .finish("screen", &mut warnings, CaptureError::backend)
            .unwrap_err();
        assert_eq!(err.kind, capture_api::CaptureErrorKind::Timeout);
        assert!(
            err.message
                .contains("screen stream did not start within 500 ms")
        );
        assert!(err.details.is_some());
    }

    #[test]
    fn early_end_is_a_warning() {
        let mut warnings = Vec::new();
        let elapsed = outcome(true, false, None)
            .finish("audio", &mut warnings, CaptureError::backend)
            .expect("ok");
        assert_eq!(elapsed, Duration::from_millis(420));
        assert_eq!(
            warnings,
            vec![
                "disconnect failed".to_owned(),
                "the stream ended before the probe duration elapsed".to_owned()
            ]
        );
    }

    #[test]
    fn audio_input_classes() {
        assert!(is_audio_input_class("Audio/Source"));
        assert!(is_audio_input_class("Audio/Source/Virtual"));
        assert!(is_audio_input_class("Audio/Duplex"));
        assert!(!is_audio_input_class("Audio/Sink"));
        assert!(!is_audio_input_class("Stream/Input/Audio"));
        assert!(!is_audio_input_class("Video/Source"));
    }
}
