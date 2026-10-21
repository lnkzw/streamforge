//! Screen source ids: `display:<CGDirectDisplayID>` and
//! `window:<CGWindowID>`, the numbers ScreenCaptureKit reports. Window
//! numbers are not reused while the window server runs.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceRef {
    Display(u32),
    Window(u32),
}

impl SourceRef {
    pub(crate) fn parse(id: &str) -> Option<Self> {
        let (kind, number) = id.split_once(':')?;
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let number = number.parse().ok()?;
        match kind {
            "display" => Some(Self::Display(number)),
            "window" => Some(Self::Window(number)),
            _ => None,
        }
    }

    pub(crate) fn id(self) -> String {
        match self {
            Self::Display(number) => format!("display:{number}"),
            Self::Window(number) => format!("window:{number}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for source in [SourceRef::Display(1), SourceRef::Window(4711)] {
            assert_eq!(SourceRef::parse(&source.id()), Some(source));
        }
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for id in [
            "",
            "display:",
            "display:+1",
            "display:-1",
            "display:1x",
            "window:99999999999",
            "monitor:1",
            "display",
        ] {
            assert_eq!(SourceRef::parse(id), None, "{id}");
        }
    }
}
