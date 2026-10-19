//! PipeWire client connection helpers.
//!
//! Each operation creates its own main loop on the calling thread and runs it
//! for a bounded time. PipeWire objects are `!Send`, so they never leave the
//! function that created them.

use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::Duration;

use capture_api::{AudioInput, CaptureError};
use pipewire as pw;
use pw::context::ContextRc;
use pw::core::CoreRc;
use pw::main_loop::MainLoopRc;
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
