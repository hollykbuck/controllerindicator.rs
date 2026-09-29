// Copyright (C) 2026 hollykbuck
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Turn a stream of polled states into discrete input events.

use std::fmt;
use std::thread::sleep;
use std::time::Duration;

use crate::gamepad::{ALL_BUTTONS, Button, Gamepad, GamepadState};

/// An axis has to move at least this far to be worth an event.
///
/// Without it a resting stick would chatter all day over the last bit of noise.
pub const AXIS_EPSILON: f32 = 0.01;

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Connected,
    Disconnected,
    Button,
    Axis,
    Trigger,
}

/// A single thing that happened on a controller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputEvent {
    pub index: u8,
    pub kind: EventKind,
    /// The button label, the axis name, or `LT`/`RT` for a trigger.
    pub name: &'static str,
    pub value: f32,
    pub previous: f32,
}

impl InputEvent {
    pub fn connected(index: u8) -> Self {
        Self {
            index,
            kind: EventKind::Connected,
            name: "",
            value: 0.0,
            previous: 0.0,
        }
    }

    pub fn disconnected(index: u8) -> Self {
        Self {
            index,
            kind: EventKind::Disconnected,
            name: "",
            value: 0.0,
            previous: 0.0,
        }
    }

    pub fn button(index: u8, button: Button, pressed: bool) -> Self {
        Self {
            index,
            kind: EventKind::Button,
            name: button.label(),
            value: f64::from(pressed) as f32,
            previous: f64::from(!pressed) as f32,
        }
    }

    pub fn axis(index: u8, name: &'static str, after: f32, before: f32) -> Self {
        Self {
            index,
            kind: EventKind::Axis,
            name,
            value: after,
            previous: before,
        }
    }

    pub fn trigger(index: u8, name: &'static str, after: f32, before: f32) -> Self {
        Self {
            index,
            kind: EventKind::Trigger,
            name,
            value: after,
            previous: before,
        }
    }
}

impl fmt::Display for InputEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            EventKind::Connected => write!(f, "[{}] connected", self.index),
            EventKind::Disconnected => write!(f, "[{}] disconnected", self.index),
            EventKind::Button => write!(
                f,
                "[{}] {} {}",
                self.index,
                self.name,
                if self.value != 0.0 { "down" } else { "up" }
            ),
            EventKind::Trigger => write!(f, "[{}] {} {:.2}", self.index, self.name, self.value),
            EventKind::Axis => write!(f, "[{}] {} {:+.2}", self.index, self.name, self.value),
        }
    }
}

/// The events that turn `previous` into `current`.
///
/// A `previous` of `None` means the pad has just appeared, so a connect comes first
/// and everything is then diffed against a neutral state.
pub fn diff(previous: Option<GamepadState>, current: GamepadState) -> Vec<InputEvent> {
    let mut events = Vec::new();
    // A `None` baseline means the pad has just appeared: connect, then diff against
    // a neutral state so everything currently held is reported as a fresh press.
    let baseline = match previous {
        Some(state) => state,
        None => {
            events.push(InputEvent::connected(current.index));
            GamepadState {
                index: current.index,
                ..GamepadState::default()
            }
        }
    };

    let pressed = current.buttons & !baseline.buttons;
    let released = baseline.buttons & !current.buttons;
    for button in ALL_BUTTONS {
        if pressed.contains(button) {
            events.push(InputEvent::button(current.index, button, true));
        }
    }
    for button in ALL_BUTTONS {
        if released.contains(button) {
            events.push(InputEvent::button(current.index, button, false));
        }
    }

    // Triggers before axes, matching the order the Python original emitted them in.
    for (after, before, name) in [
        (current.left_trigger, baseline.left_trigger, "LT"),
        (current.right_trigger, baseline.right_trigger, "RT"),
    ] {
        if (after - before).abs() > AXIS_EPSILON {
            events.push(InputEvent::trigger(current.index, name, after, before));
        }
    }

    for (after, before, name) in [
        (current.left_x, baseline.left_x, "left_x"),
        (current.left_y, baseline.left_y, "left_y"),
        (current.right_x, baseline.right_x, "right_x"),
        (current.right_y, baseline.right_y, "right_y"),
    ] {
        if (after - before).abs() > AXIS_EPSILON {
            events.push(InputEvent::axis(current.index, name, after, before));
        }
    }

    events
}

/// A state that is connected and idle, for tests and for the first frame of a stream.
pub fn idle(index: u8) -> GamepadState {
    GamepadState {
        index,
        ..GamepadState::default()
    }
}

/// Poll `gamepad` forever, handing each event to `on_event` as it happens.
///
/// Returning from `on_event` does nothing special; to stop, close the controller or
/// drop the process. The Python original raised `KeyboardInterrupt` out of the
/// generator, which here is the same as the process being interrupted.
pub fn watch<F>(gamepad: &Gamepad<'_>, interval: Duration, mut on_event: F)
where
    F: FnMut(InputEvent),
{
    let mut previous: Option<GamepadState> = None;
    loop {
        match gamepad.poll() {
            Ok(Some(current)) => {
                for event in diff(previous, current) {
                    on_event(event);
                }
                previous = Some(current);
            }
            Ok(None) => {
                if let Some(state) = previous {
                    on_event(InputEvent::disconnected(state.index));
                    previous = None;
                }
            }
            Err(err) => {
                eprintln!("error: {err}");
                return;
            }
        }
        sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamepad::RawState;

    fn state(index: u8, raw: RawState) -> GamepadState {
        GamepadState::from_raw(index, raw, 0.0, false, false)
    }

    fn names(events: &[InputEvent]) -> Vec<String> {
        events.iter().map(|e| e.to_string()).collect()
    }

    #[test]
    fn first_read_connects_and_reports_nothing_else() {
        let events = diff(None, idle(0));
        assert_eq!(names(&events), ["[0] connected"]);
    }

    #[test]
    fn a_press_then_a_release_yields_down_then_up() {
        let down = state(
            0,
            RawState {
                buttons: Button::A.bits(),
                ..Default::default()
            },
        );
        let up = state(0, RawState::default());

        let events = diff(Some(idle(0)), down);
        assert_eq!(names(&events), ["[0] A down"]);

        let events = diff(Some(down), up);
        assert_eq!(names(&events), ["[0] A up"]);
    }

    #[test]
    fn presses_come_before_releases() {
        let before = state(
            0,
            RawState {
                buttons: Button::A.bits(),
                left_x: 16384,
                ..Default::default()
            },
        );
        let after = state(
            0,
            RawState {
                buttons: Button::B.bits(),
                left_x: 0,
                ..Default::default()
            },
        );
        let events = diff(Some(before), after);
        assert_eq!(
            names(&events),
            ["[0] B down", "[0] A up", "[0] left_x +0.00"]
        );
    }

    #[test]
    fn a_held_button_produces_nothing() {
        let held = state(
            0,
            RawState {
                buttons: Button::A.bits(),
                ..Default::default()
            },
        );
        assert!(diff(Some(held), held).is_empty());
    }

    #[test]
    fn several_buttons_at_once_come_in_reading_order() {
        let after = state(
            0,
            RawState {
                buttons: Button::Y.bits() | Button::DPAD_UP.bits() | Button::A.bits(),
                ..Default::default()
            },
        );
        let events = diff(Some(idle(0)), after);
        assert_eq!(
            names(&events),
            ["[0] D-pad up down", "[0] A down", "[0] Y down"]
        );
    }

    #[test]
    fn triggers_are_reported_before_axes() {
        let after = state(
            0,
            RawState {
                left_trigger: 255,
                right_x: 16384,
                ..Default::default()
            },
        );
        let events = diff(Some(idle(0)), after);
        assert_eq!(names(&events), ["[0] LT 1.00", "[0] right_x +0.50"]);
    }

    #[test]
    fn a_movement_under_the_epsilon_is_ignored() {
        let before = state(
            0,
            RawState {
                left_x: 16384,
                ..Default::default()
            },
        );
        // One count is about 3e-5, far below the 0.01 threshold.
        let after = state(
            0,
            RawState {
                left_x: 16385,
                ..Default::default()
            },
        );
        assert!(diff(Some(before), after).is_empty());
    }

    #[test]
    fn events_carry_the_previous_value() {
        let before = state(
            0,
            RawState {
                left_x: 0,
                ..Default::default()
            },
        );
        let after = state(
            0,
            RawState {
                left_x: 16384,
                ..Default::default()
            },
        );
        let events = diff(Some(before), after);
        let event = events.first().expect("an axis event");
        assert_eq!(event.previous, 0.0);
        // 16384 sits a hair over half of 32767, so this is not exactly 0.5.
        assert!((event.value - 0.50001).abs() < 1e-4, "got {}", event.value);
    }

    #[test]
    fn a_reconnect_connects_again() {
        let held = state(
            0,
            RawState {
                buttons: Button::A.bits(),
                ..Default::default()
            },
        );
        let events = diff(None, held);
        assert_eq!(names(&events), ["[0] connected", "[0] A down"]);
    }

    #[test]
    fn an_axis_keeps_its_sign() {
        let after = state(
            0,
            RawState {
                left_x: -16384,
                ..Default::default()
            },
        );
        let events = diff(Some(idle(0)), after);
        assert_eq!(names(&events), ["[0] left_x -0.50"]);
    }
}
