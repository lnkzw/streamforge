//! Screen source ids:
//! - `monitor:<GDI device name>`, e.g. `monitor:\\.\DISPLAY1`, stable
//!   while the monitor is connected;
//! - `window:<process id>:<handle in hex>`, e.g. `window:4242:0x2a0b6`,
//!   valid while the window exists. Windows reuses handle values, so the
//!   process id is part of the id: a reused handle of another process does
//!   not resolve to the new window.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceRef {
    Monitor(String),
    Window { process: u32, handle: usize },
}

impl SourceRef {
    pub(crate) fn parse(id: &str) -> Option<Self> {
        if let Some(device) = id.strip_prefix("monitor:") {
            return (!device.is_empty()).then(|| Self::Monitor(device.to_owned()));
        }
        let (process, handle) = id.strip_prefix("window:")?.split_once(':')?;
        let digits = |text: &str, hex: bool| {
            !text.is_empty()
                && text.bytes().all(|byte| {
                    if hex {
                        byte.is_ascii_hexdigit()
                    } else {
                        byte.is_ascii_digit()
                    }
                })
        };
        let hex = handle.strip_prefix("0x")?;
        if !digits(process, false) || !digits(hex, true) {
            return None;
        }
        let process = process.parse().ok()?;
        let handle = usize::from_str_radix(hex, 16).ok().filter(|h| *h != 0)?;
        Some(Self::Window { process, handle })
    }

    pub(crate) fn id(&self) -> String {
        match self {
            Self::Monitor(device) => format!("monitor:{device}"),
            Self::Window { process, handle } => format!("window:{process}:{handle:#x}"),
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
            SourceRef::Window {
                process: 4242,
                handle: 0x2a0b6,
            },
        ] {
            assert_eq!(SourceRef::parse(&source.id()), Some(source));
        }
        assert_eq!(
            SourceRef::Window {
                process: 7,
                handle: 42
            }
            .id(),
            "window:7:0x2a"
        );
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for id in [
            "",
            "monitor:",
            "window:",
            "window:0x2a",
            "window:7:",
            "window:7:0x",
            "window:7:42",
            "window:7:0x0",
            "window:7:0x+2a",
            "window:7:0xzz",
            "window:+7:0x2a",
            "window::0x2a",
            "window:99999999999:0x2a",
            "window:7:0x1ffffffffffffffffff",
            "display:1",
        ] {
            assert_eq!(SourceRef::parse(id), None, "{id}");
        }
    }
}
