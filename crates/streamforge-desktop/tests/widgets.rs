//! Behaviour of design-system components under simulated input
//! (egui_kittest). No rendering is involved, so these run everywhere.

use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use streamforge_desktop::ui::widgets::{self, FADER_STEP_DB};

const HEIGHT: f32 = 180.0;
const RANGE: std::ops::RangeInclusive<f32> = -60.0..=12.0;

fn fader(start: f32) -> Harness<'static, f32> {
    let mut harness = Harness::new_ui_state(
        |ui, gain: &mut f32| {
            widgets::fader(ui, HEIGHT, gain, RANGE, "mic");
        },
        start,
    );
    harness.run();
    harness
}

/// Where the thumb of a fader at `gain` is centred, in a fader at `rect`.
fn thumb_y(rect: egui::Rect, gain: f32) -> f32 {
    let track = HEIGHT - 12.0;
    let bottom = rect.center().y + track / 2.0;
    bottom - (gain - RANGE.start()) / (RANGE.end() - RANGE.start()) * track
}

/// Press at `from`, move to `to`, release there.
fn drag<State>(harness: &mut Harness<'static, State>, from: egui::Pos2, to: egui::Pos2) {
    harness.hover_at(from);
    harness.run();
    harness.drag_at(from);
    harness.run();
    harness.hover_at(to);
    harness.run();
    harness.drop_at(to);
    harness.run();
}

#[test]
fn a_click_changes_nothing() {
    let mut harness = fader(-6.0);
    harness.get_by_label("mic").click();
    harness.run();
    assert_eq!(*harness.state(), -6.0);
}

#[test]
fn a_drag_moves_from_where_the_thumb_was_grabbed() {
    let mut harness = fader(-6.0);
    let rect = harness.get_by_label("mic").rect();
    // Grab 4 px above the thumb's centre and move up 42 px: the thumb
    // follows by 42 px, not by 46.
    let grab = egui::pos2(rect.center().x, thumb_y(rect, -6.0) - 4.0);
    drag(&mut harness, grab, grab - egui::vec2(0.0, 42.0));
    let expected = -6.0 + 42.0 / (HEIGHT - 12.0) * (RANGE.end() - RANGE.start());
    let gain = *harness.state();
    assert!((gain - expected).abs() < 0.11, "{gain} != {expected}");
}

#[test]
fn a_drag_near_unity_snaps_to_it() {
    let mut harness = fader(-12.0);
    let rect = harness.get_by_label("mic").rect();
    let grab = egui::pos2(rect.center().x, thumb_y(rect, -12.0));
    // To about +0.3 dB: inside the detent.
    let target = thumb_y(rect, 0.3);
    drag(&mut harness, grab, egui::pos2(grab.x, target));
    assert_eq!(*harness.state(), 0.0);
}

#[test]
fn arrow_keys_step_the_focused_fader() {
    let mut harness = fader(-6.0);
    harness.get_by_label("mic").focus();
    harness.run();
    harness.key_press(egui::Key::ArrowUp);
    harness.key_press(egui::Key::ArrowUp);
    harness.run();
    assert_eq!(*harness.state(), -6.0 + 2.0 * FADER_STEP_DB);
    harness.key_press(egui::Key::ArrowDown);
    harness.run();
    assert_eq!(*harness.state(), -6.0 + FADER_STEP_DB);
}

#[test]
fn arrow_keys_keep_the_focus_on_the_fader() {
    // A mute switch under the fader, as in a console strip: egui would
    // move the focus down to it.
    let mut harness = Harness::new_ui_state(
        |ui, (gain, mute): &mut (f32, bool)| {
            widgets::fader(ui, HEIGHT, gain, RANGE, "mic");
            widgets::toggle(ui, mute, "Mute mic");
        },
        (-6.0, false),
    );
    harness.run();
    harness.get_by_label("mic").focus();
    harness.run();
    harness.key_press(egui::Key::ArrowDown);
    harness.run();
    harness.key_press(egui::Key::ArrowDown);
    harness.run();
    assert_eq!(harness.state().0, -6.0 - 2.0 * FADER_STEP_DB);
    assert!(harness.get_by_label("mic").is_focused());
}

#[test]
fn a_value_outside_the_range_stays_until_the_fader_is_moved() {
    // The engine accepts -96 dB; the fader shows it at its bottom end
    // without changing it.
    let mut harness = fader(-96.0);
    harness.run();
    assert_eq!(*harness.state(), -96.0);
    let rect = harness.get_by_label("mic").rect();
    let bottom = thumb_y(rect, -60.0);
    assert!(bottom <= rect.bottom(), "thumb inside the fader");
    harness.get_by_label("mic").focus();
    harness.run();
    harness.key_press(egui::Key::ArrowUp);
    harness.run();
    // The first step starts from the clamped end of the range.
    assert_eq!(*harness.state(), -60.0);
}

#[test]
fn a_drag_commits_its_value_when_the_caller_rebuilds_it_each_frame() {
    // As the mixer does: the gain comes from the model every frame, and the
    // edit is sent on the frame the drag ends.
    let mut harness = Harness::new_ui_state(
        |ui, (model, sent): &mut (f32, Option<f32>)| {
            let mut gain = *model;
            let response = widgets::fader(ui, HEIGHT, &mut gain, RANGE, "mic");
            if response.drag_stopped() {
                *sent = Some(gain);
            }
        },
        (-12.0, None),
    );
    harness.run();
    let rect = harness.get_by_label("mic").rect();
    let grab = egui::pos2(rect.center().x, thumb_y(rect, -12.0));
    drag(&mut harness, grab, egui::pos2(grab.x, thumb_y(rect, -24.0)));
    let sent = harness.state().1.expect("sent on release");
    assert!((sent + 24.0).abs() < 0.11, "{sent}");
}

/// Two opacity sliders side by side (0 to 100, step 1), so a key that
/// moved the focus would reach the second one.
fn sliders(start: f32) -> Harness<'static, [f32; 2]> {
    let mut harness = Harness::new_ui_state(
        |ui, values: &mut [f32; 2]| {
            ui.horizontal(|ui| {
                widgets::slider(ui, &mut values[0], 0.0..=100.0, 1.0, 200.0, "first");
                widgets::slider(ui, &mut values[1], 0.0..=100.0, 1.0, 200.0, "second");
            });
        },
        [start, start],
    );
    harness.run();
    harness
}

#[test]
fn a_click_on_the_slider_sets_the_value_under_the_pointer() {
    let mut harness = sliders(10.0);
    let rect = harness.get_by_label("first").rect();
    // The track spans the width less the thumb (12 px); three quarters
    // along it is 75.
    let x = rect.left() + 6.0 + 0.75 * (rect.width() - 12.0);
    harness.hover_at(egui::pos2(x, rect.center().y));
    harness.run();
    harness.drag_at(egui::pos2(x, rect.center().y));
    harness.run();
    harness.drop_at(egui::pos2(x, rect.center().y));
    harness.run();
    assert_eq!(harness.state()[0], 75.0);
    assert_eq!(harness.state()[1], 10.0);
}

#[test]
fn arrow_keys_step_the_focused_slider_and_keep_its_focus() {
    let mut harness = sliders(10.0);
    harness.get_by_label("first").focus();
    harness.run();
    harness.key_press(egui::Key::ArrowRight);
    harness.run();
    harness.key_press(egui::Key::ArrowRight);
    harness.run();
    // Had the first press moved the focus, the second would step the
    // second slider.
    assert_eq!(*harness.state(), [12.0, 10.0]);
    harness.key_press(egui::Key::ArrowLeft);
    harness.run();
    assert_eq!(*harness.state(), [11.0, 10.0]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoder {
    Auto,
    Software,
}

#[test]
fn a_segment_click_changes_the_choice() {
    let mut harness = Harness::new_ui_state(
        |ui, (encoder, changes): &mut (Encoder, usize)| {
            let choices = [(Encoder::Auto, "Auto"), (Encoder::Software, "Software")];
            if widgets::segmented(ui, encoder, &choices) {
                *changes += 1;
            }
        },
        (Encoder::Auto, 0),
    );
    harness.run();
    // The current segment again changes nothing.
    harness.get_by_label("Auto").click();
    harness.run();
    assert_eq!(*harness.state(), (Encoder::Auto, 0));
    harness.get_by_label("Software").click();
    harness.run();
    assert_eq!(*harness.state(), (Encoder::Software, 1));
}

#[test]
fn a_dropdown_choice_becomes_current_and_closes_the_list() {
    let mut harness = Harness::new_ui_state(
        |ui, device: &mut Option<&'static str>| {
            let text = device.unwrap_or("System default");
            widgets::dropdown(ui, text, "Audio output", 200.0, |ui| {
                widgets::choice(ui, device, None, "System default");
                widgets::choice(ui, device, Some("Speakers"), "Speakers");
            });
        },
        None,
    );
    harness.run();
    assert!(harness.query_by_label("Speakers").is_none(), "closed");
    harness.get_by_label("Audio output").click();
    harness.run();
    harness.get_by_label("Speakers").click();
    harness.run();
    assert_eq!(*harness.state(), Some("Speakers"));
    assert!(harness.query_by_label("System default").is_none(), "closed");
}

#[test]
fn a_dropdown_choice_by_keyboard_closes_the_list_too() {
    let mut harness = Harness::new_ui_state(
        |ui, device: &mut Option<&'static str>| {
            let text = device.unwrap_or("System default");
            widgets::dropdown(ui, text, "Audio output", 200.0, |ui| {
                widgets::choice(ui, device, None, "System default");
                widgets::choice(ui, device, Some("Speakers"), "Speakers");
            });
        },
        None,
    );
    harness.run();
    harness.get_by_label("Audio output").click();
    harness.run();
    harness.get_by_label("Speakers").focus();
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert_eq!(*harness.state(), Some("Speakers"));
    assert!(harness.query_by_label("System default").is_none(), "closed");
}

#[test]
fn a_disabled_menu_item_or_button_says_so_to_its_tooltip() {
    // `on_disabled_hover_text` shows only for a response that reports
    // disabled, as when the `Ui` is.
    let mut harness = Harness::new_ui_state(
        |ui, enabled: &mut [bool; 2]| {
            let item = ui.add(widgets::MenuItem::new("Delete…").enabled(false));
            let button =
                ui.add(widgets::Button::new(widgets::ButtonKind::Secondary, "Put").enabled(false));
            *enabled = [item.enabled(), button.enabled()];
        },
        [true, true],
    );
    harness.run();
    assert_eq!(*harness.state(), [false, false]);
}
