//! Desktop audio: each output whose playback can be captured is an input
//! too, named `<output>.monitor` as on Linux, with `loopback_of` set
//! (ADR 0017). Kept apart from the backend so its tests run on every OS.

use capture_api::AudioInput;

/// Suffix of an output's monitor input name.
pub(crate) const MONITOR_SUFFIX: &str = ".monitor";

/// The name of the input that captures what output `name` plays.
pub(crate) fn monitor_name(name: &str) -> String {
    format!("{name}{MONITOR_SUFFIX}")
}

/// The output whose monitor `wanted` names, if it names one.
pub(crate) fn monitored(wanted: &str) -> Option<&str> {
    wanted
        .strip_suffix(MONITOR_SUFFIX)
        .filter(|output| !output.is_empty())
}

/// `inputs`, then the monitor of each output (name and description). A
/// name selects one input: an input wins over a monitor of the same name,
/// so that monitor is left out.
pub(crate) fn with_monitors(
    mut inputs: Vec<AudioInput>,
    outputs: impl IntoIterator<Item = (String, Option<String>)>,
) -> Vec<AudioInput> {
    for (name, description) in outputs {
        let monitor = monitor_name(&name);
        if inputs.iter().any(|input| input.name == monitor) {
            continue;
        }
        inputs.push(AudioInput {
            name: monitor,
            description: Some(format!(
                "Monitor of {}",
                description.as_deref().unwrap_or(&name)
            )),
            media_class: "Audio/Sink".to_owned(),
            loopback_of: Some(name),
        });
    }
    inputs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> AudioInput {
        AudioInput {
            name: name.to_owned(),
            description: None,
            media_class: "Audio/Source".to_owned(),
            loopback_of: None,
        }
    }

    #[test]
    fn every_output_is_listed_as_its_monitor_after_the_inputs() {
        let listed = with_monitors(
            vec![input("mic")],
            [
                (
                    "{0.0.0.00000000}.{a1}".to_owned(),
                    Some("Speakers".to_owned()),
                ),
                ("hdmi".to_owned(), None),
            ],
        );
        let summary: Vec<_> = listed
            .iter()
            .map(|input| {
                (
                    input.name.as_str(),
                    input.description.as_deref(),
                    input.loopback_of.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("mic", None, None),
                (
                    "{0.0.0.00000000}.{a1}.monitor",
                    Some("Monitor of Speakers"),
                    Some("{0.0.0.00000000}.{a1}")
                ),
                ("hdmi.monitor", Some("Monitor of hdmi"), Some("hdmi")),
            ]
        );
        assert_eq!(listed[1].media_class, "Audio/Sink");
    }

    #[test]
    fn an_input_wins_over_a_monitor_of_the_same_name() {
        let listed = with_monitors(
            vec![input("speakers.monitor")],
            [("speakers".to_owned(), None)],
        );
        assert_eq!(listed, [input("speakers.monitor")]);
    }

    #[test]
    fn a_monitor_name_selects_its_output() {
        assert_eq!(monitored("speakers.monitor"), Some("speakers"));
        assert_eq!(monitored("a.monitor.monitor"), Some("a.monitor"));
        assert_eq!(
            monitored(&monitor_name("Speakers (Realtek)")),
            Some("Speakers (Realtek)")
        );
        assert_eq!(monitored(".monitor"), None);
        assert_eq!(monitored("speakers"), None);
    }
}
