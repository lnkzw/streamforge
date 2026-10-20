//! Screen source ids: `monitor:<GDI device name>` (e.g.
//! `monitor:\\.\DISPLAY1`, stable while the monitor is connected) and
//! `window:<handle in hex>` (e.g. `window:0x2a0b6`, valid while the window
//! exists).

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceRef {
    Monitor(String),
    Window(usize),
}

impl SourceRef {
    pub(crate) fn parse(id: &str) -> Option<Self> {
        if let Some(device) = id.strip_prefix("monitor:") {
            return (!device.is_empty()).then(|| Self::Monitor(device.to_owned()));
        }
        let hex = id.strip_prefix("window:0x")?;
        if hex.is_empty() || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        usize::from_str_radix(hex, 16)
            .ok()
            .filter(|handle| *handle != 0)
            .map(Self::Window)
    }

    pub(crate) fn id(&self) -> String {
        match self {
            Self::Monitor(device) => format!("monitor:{device}"),
            Self::Window(handle) => format!("window:{handle:#x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for source in [
            SourceRef::Monitor(r"\\.\DISPLAY1".to_owned()),
            SourceRef::Window(0x2a0b6),
        ] {
            assert_eq!(SourceRef::parse(&source.id()), Some(source));
        }
        assert_eq!(SourceRef::Window(42).id(), "window:0x2a");
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for id in [
            "",
            "monitor:",
            "window:",
            "window:0x",
            "window:42",
            "window:0x0",
            "window:0x+2a",
            "window:0xzz",
            "window:0x1ffffffffffffffffff",
            "display:1",
        ] {
            assert_eq!(SourceRef::parse(id), None, "{id}");
        }
    }
}
