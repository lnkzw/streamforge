//! Follows the `get_health` readings of a run: when a loss counter grows or
//! a state changes, that becomes an event with its time, so every loss the
//! verdict counts can be placed on the timeline (and next to the host's
//! load at that moment).

use std::fmt;

use serde::Serialize;

use crate::reports::Health;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    /// Seconds since the outputs started.
    pub t_s: f64,
    /// The counter or state, e.g. `recording.frames_dropped` or
    /// `captures[Screen].state`.
    pub what: String,
    /// For counters: how much it grew since the previous reading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<u64>,
    /// The counter's total or the new state.
    pub now: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

/// Stream drops before the first encoded video frame: the stream drops
/// earlier audio by design so that both start together.
const STREAM_START: &str = "before the stream's first video frame, by design";

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {} {}", crate::clock(self.t_s), self.what, self.now)?;
        if let Some(delta) = self.delta {
            write!(f, " (+{delta})")?;
        }
        if let Some(note) = self.note {
            write!(f, ": {note}")?;
        }
        Ok(())
    }
}

/// The readings of one run, from the first one after the outputs started.
#[derive(Debug, Default)]
pub struct Watch {
    pub first: Option<Health>,
    pub last: Option<Health>,
    pub events: Vec<Event>,
    /// The deepest output queues seen in any reading.
    pub max_queue_depth: QueueDepths,
    pub readings: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct QueueDepths {
    pub recording: u64,
    pub streaming: u64,
}

impl Watch {
    /// Take one reading, `t_s` seconds after the outputs started; returns
    /// the events it found.
    pub fn observe(&mut self, t_s: f64, health: Health) -> &[Event] {
        let start = self.events.len();
        if let Some(last) = &self.last {
            let before = loss_counters(last);
            let stream_started = last
                .streaming
                .as_ref()
                .is_some_and(|stream| stream.frames_encoded > 0);
            for (what, now) in loss_counters(&health) {
                let previous = before
                    .iter()
                    .find(|(name, _)| *name == what)
                    .map_or(0, |(_, value)| *value);
                if now > previous {
                    let note =
                        (what.starts_with("streaming.") && !stream_started).then_some(STREAM_START);
                    self.events.push(Event {
                        t_s,
                        what,
                        delta: Some(now - previous),
                        now: now.to_string(),
                        note,
                    });
                }
            }
            let before = states(last);
            for (what, now) in states(&health) {
                let previous = before.iter().find(|(name, _)| *name == what);
                if previous.is_none_or(|(_, value)| *value != now) {
                    self.events.push(Event {
                        t_s,
                        what,
                        delta: None,
                        now,
                        note: None,
                    });
                }
            }
        }
        let depth = |output: &Option<crate::reports::OutputHealth>| {
            output.as_ref().map_or(0, |output| output.queue_depth)
        };
        self.max_queue_depth.recording =
            self.max_queue_depth.recording.max(depth(&health.recording));
        self.max_queue_depth.streaming =
            self.max_queue_depth.streaming.max(depth(&health.streaming));
        self.readings += 1;
        if self.first.is_none() {
            self.first = Some(health.clone());
        }
        self.last = Some(health);
        &self.events[start..]
    }

    /// How much `counter` (a name from [`loss_counters`]) grew from the
    /// first reading to the last.
    pub fn growth(&self, counter: &str) -> u64 {
        let value = |health: &Option<Health>| {
            health.as_ref().map_or(0, |health| {
                loss_counters(health)
                    .into_iter()
                    .find(|(name, _)| name == counter)
                    .map_or(0, |(_, value)| value)
            })
        };
        value(&self.last).saturating_sub(value(&self.first))
    }

    /// The loss counters whose growth the verdict judges, with that growth;
    /// the outputs' own drops are judged from their stop reports instead.
    pub fn pipeline_losses(&self) -> Vec<(String, u64)> {
        self.last
            .as_ref()
            .map(loss_counters)
            .unwrap_or_default()
            .into_iter()
            .filter(|(name, _)| !name.starts_with("recording.") && !name.starts_with("streaming."))
            .map(|(name, _)| {
                let grown = self.growth(&name);
                (name, grown)
            })
            .collect()
    }
}

/// Counters that grow when media is lost: ticks the compositor skipped,
/// blocks the mixer skipped, audio an input did not deliver in time
/// (underruns, filled with silence) or delivered beyond its buffer
/// (overruns, discarded), and what the outputs dropped.
pub fn loss_counters(health: &Health) -> Vec<(String, u64)> {
    let mut counters = Vec::new();
    if let Some(render) = &health.render {
        counters.push(("render.skipped_ticks".to_owned(), render.skipped_ticks));
    }
    if let Some(mix) = &health.mix {
        counters.push(("mix.skipped_blocks".to_owned(), mix.skipped_blocks));
        for input in &mix.inputs {
            let name = &input.name;
            counters.push((
                format!("mix.inputs[{name}].underrun_frames"),
                input.underrun_frames,
            ));
            counters.push((
                format!("mix.inputs[{name}].overrun_frames"),
                input.overrun_frames.unwrap_or(0),
            ));
        }
    }
    for (output, health) in [
        ("recording", &health.recording),
        ("streaming", &health.streaming),
    ] {
        if let Some(health) = health {
            counters.push((format!("{output}.frames_dropped"), health.frames_dropped));
            counters.push((format!("{output}.blocks_dropped"), health.blocks_dropped));
        }
    }
    counters
}

/// States whose change is an event: each output's state, a stream's
/// connects and reconnect attempts, and each capture's state.
fn states(health: &Health) -> Vec<(String, String)> {
    let mut states = Vec::new();
    for (output, health) in [
        ("recording", &health.recording),
        ("streaming", &health.streaming),
    ] {
        let Some(health) = health else { continue };
        states.push((format!("{output}.state"), health.state.clone()));
        states.push((format!("{output}.running"), health.running.to_string()));
        if let Some(connects) = health.connects {
            states.push((format!("{output}.connects"), connects.to_string()));
        }
        if let Some(attempts) = health.reconnect_attempts {
            states.push((format!("{output}.reconnect_attempts"), attempts.to_string()));
        }
    }
    for capture in &health.captures {
        let state = match &capture.reason {
            Some(reason) => format!("{} ({reason})", capture.state),
            None => capture.state.clone(),
        };
        states.push((format!("captures[{}].state", capture.name), state));
    }
    states
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reports::{Capture, Mix, MixInput, OutputHealth, Render};

    fn reading(skipped: u64, underruns: u64, dropped: u64, capture: &str) -> Health {
        Health {
            render: Some(Render {
                skipped_ticks: skipped,
                ..Render::default()
            }),
            mix: Some(Mix {
                inputs: vec![MixInput {
                    name: "Desk".to_owned(),
                    underrun_frames: underruns,
                    ..MixInput::default()
                }],
                ..Mix::default()
            }),
            recording: Some(OutputHealth {
                state: "running".to_owned(),
                running: true,
                frames_dropped: dropped,
                queue_depth: dropped + 1,
                ..OutputHealth::default()
            }),
            streaming: None,
            captures: vec![Capture {
                kind: "screen".to_owned(),
                name: "Screen".to_owned(),
                state: capture.to_owned(),
                ..Capture::default()
            }],
        }
    }

    #[test]
    fn grown_counters_and_changed_states_become_events() {
        let mut watch = Watch::default();
        // Counts before the first reading are not events.
        assert!(watch.observe(0.25, reading(2, 0, 0, "running")).is_empty());
        assert!(watch.observe(0.5, reading(2, 0, 0, "running")).is_empty());
        let found = watch.observe(0.75, reading(3, 480, 2, "running")).to_vec();
        let names: Vec<_> = found.iter().map(|event| event.what.as_str()).collect();
        assert_eq!(
            names,
            [
                "render.skipped_ticks",
                "mix.inputs[Desk].underrun_frames",
                "recording.frames_dropped"
            ]
        );
        assert_eq!(found[1].delta, Some(480));
        assert_eq!(found[2].now, "2");
        let ended = watch.observe(1.0, reading(3, 480, 2, "ended")).to_vec();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].what, "captures[Screen].state");
        assert_eq!(ended[0].delta, None);
        assert_eq!(watch.readings, 4);
        assert_eq!(watch.events.len(), 4);
        assert_eq!(watch.max_queue_depth.recording, 3);
    }

    #[test]
    fn growth_counts_from_the_first_reading() {
        let mut watch = Watch::default();
        watch.observe(0.25, reading(2, 10, 1, "running"));
        watch.observe(9.0, reading(5, 10, 4, "running"));
        assert_eq!(watch.growth("render.skipped_ticks"), 3);
        assert_eq!(watch.growth("mix.inputs[Desk].underrun_frames"), 0);
        assert_eq!(watch.growth("absent"), 0);
        // The outputs' drops are judged from their reports.
        assert_eq!(
            watch.pipeline_losses(),
            [
                ("render.skipped_ticks".to_owned(), 3),
                ("mix.skipped_blocks".to_owned(), 0),
                ("mix.inputs[Desk].underrun_frames".to_owned(), 0),
                ("mix.inputs[Desk].overrun_frames".to_owned(), 0),
            ]
        );
    }

    #[test]
    fn stream_drops_before_its_first_frame_are_marked_as_by_design() {
        let stream = |frames_encoded, blocks_dropped| Health {
            streaming: Some(OutputHealth {
                state: "connected".to_owned(),
                running: true,
                frames_encoded,
                blocks_dropped,
                ..OutputHealth::default()
            }),
            ..Health::default()
        };
        let mut watch = Watch::default();
        watch.observe(0.0, stream(0, 0));
        let start = watch.observe(0.25, stream(15, 2)).to_vec();
        assert_eq!(start[0].note, Some(STREAM_START));
        assert_eq!(
            start[0].to_string(),
            "[00:00] streaming.blocks_dropped 2 (+2): before the stream's first video frame, by design"
        );
        let later = watch.observe(61.0, stream(3600, 3)).to_vec();
        assert_eq!(later[0].note, None);
        assert_eq!(
            later[0].to_string(),
            "[01:01] streaming.blocks_dropped 3 (+1)"
        );
    }
}
