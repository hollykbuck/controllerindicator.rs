// Copyright (C) 2026 hollykbuck
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Named buttons, normalised axes and deadzone handling on top of a raw backend
//! report.
//!
//! The raw report is deliberately XInput shaped. The DualShock backend decodes HID
//! into the same [`RawState`], so everything above this module stays ignorant of
//! where a pad's numbers actually came from.

use std::fmt;

use bitflags::bitflags;

/// Smallest `i16`, as a float. The two ends of the raw range are not symmetric.
const SHORT_MIN: f32 = i16::MIN as f32;
const SHORT_MAX: f32 = i16::MAX as f32;

/// The deadzone Microsoft recommends for XInput: 7849 / 32767.
pub const DEFAULT_DEADZONE: f32 = 7849.0 / 32767.0;

/// Below this a trigger counts as released, which keeps an untouched pad quiet.
const TRIGGER_THRESHOLD: f32 = 0.02;

bitflags! {
    /// A controller button. The values are XInput's `wButtons` bits, extended with
    /// [`Button::TOUCHPAD`] for pads that have a touchpad and no Xbox button to
    /// spare for it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Button: u16 {
        const DPAD_UP       = 0x0001;
        const DPAD_DOWN     = 0x0002;
        const DPAD_LEFT     = 0x0004;
        const DPAD_RIGHT    = 0x0008;
        const START         = 0x0010;
        const BACK          = 0x0020;
        const LEFT_THUMB    = 0x0040;
        const RIGHT_THUMB   = 0x0080;
        const LEFT_SHOULDER = 0x0100;
        const RIGHT_SHOULDER= 0x0200;
        const GUIDE         = 0x0400;
        const TOUCHPAD      = 0x0800;
        const A             = 0x1000;
        const B             = 0x2000;
        const X             = 0x4000;
        const Y             = 0x8000;
    }
}

impl Button {
    /// The label shown in the event stream and drawn on the window.
    pub fn label(self) -> &'static str {
        match self {
            Self::DPAD_UP => "D-pad up",
            Self::DPAD_DOWN => "D-pad down",
            Self::DPAD_LEFT => "D-pad left",
            Self::DPAD_RIGHT => "D-pad right",
            Self::START => "Start",
            Self::BACK => "Back",
            Self::LEFT_THUMB => "L3",
            Self::RIGHT_THUMB => "R3",
            Self::LEFT_SHOULDER => "LB",
            Self::RIGHT_SHOULDER => "RB",
            Self::GUIDE => "Guide",
            Self::TOUCHPAD => "Touchpad",
            Self::A => "A",
            Self::B => "B",
            Self::X => "X",
            Self::Y => "Y",
            _ => "Unknown",
        }
    }
}

/// Every bit in declaration order. Iterating this yields presses in a stable
/// reading order, which is what keeps the event stream deterministic.
pub const ALL_BUTTONS: [Button; 16] = [
    Button::DPAD_UP,
    Button::DPAD_DOWN,
    Button::DPAD_LEFT,
    Button::DPAD_RIGHT,
    Button::START,
    Button::BACK,
    Button::LEFT_THUMB,
    Button::RIGHT_THUMB,
    Button::LEFT_SHOULDER,
    Button::RIGHT_SHOULDER,
    Button::GUIDE,
    Button::TOUCHPAD,
    Button::A,
    Button::B,
    Button::X,
    Button::Y,
];

/// One raw report, in the shape XInput reports a pad.
///
/// This is the whole interchange between a backend and the rest of the program:
/// the DualShock backend decodes HID into exactly this, so a pad read over HID and
/// a pad read over XInput are indistinguishable from here up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawState {
    pub packet_number: u32,
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
}

/// One immutable snapshot of a controller, with the axes already normalised.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GamepadState {
    pub index: u8,
    pub packet_number: u32,
    pub buttons: Button,
    pub left_trigger: f32,
    pub right_trigger: f32,
    pub left_x: f32,
    pub left_y: f32,
    pub right_x: f32,
    pub right_y: f32,
}

impl Default for GamepadState {
    fn default() -> Self {
        Self {
            index: 0,
            packet_number: 0,
            buttons: Button::empty(),
            left_trigger: 0.0,
            right_trigger: 0.0,
            left_x: 0.0,
            left_y: 0.0,
            right_x: 0.0,
            right_y: 0.0,
        }
    }
}

impl GamepadState {
    /// True when every one of `wanted` is held.
    pub fn is_pressed(&self, wanted: Button) -> bool {
        self.buttons.contains(wanted)
    }

    /// The held buttons, in [`ALL_BUTTONS`] order.
    pub fn pressed_buttons(&self) -> impl Iterator<Item = Button> + '_ {
        ALL_BUTTONS
            .into_iter()
            .filter(move |button| self.buttons.contains(*button))
    }

    pub fn left_stick(&self) -> (f32, f32) {
        (self.left_x, self.left_y)
    }

    pub fn right_stick(&self) -> (f32, f32) {
        (self.right_x, self.right_y)
    }

    /// Rescale a raw report into a snapshot.
    ///
    /// `invert_x` / `invert_y` exist for the few pads whose drivers report an axis
    /// the other way up from the rest.
    pub fn from_raw(
        index: u8,
        raw: RawState,
        deadzone: f32,
        invert_x: bool,
        invert_y: bool,
    ) -> Self {
        let (left_x, left_y) = stick(raw.left_x, raw.left_y, deadzone, invert_x, invert_y);
        let (right_x, right_y) = stick(raw.right_x, raw.right_y, deadzone, invert_x, invert_y);
        Self {
            index,
            packet_number: raw.packet_number,
            buttons: Button::from_bits_truncate(raw.buttons),
            left_trigger: trigger(raw.left_trigger),
            right_trigger: trigger(raw.right_trigger),
            left_x,
            left_y,
            right_x,
            right_y,
        }
    }
}

impl fmt::Display for GamepadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self
            .pressed_buttons()
            .map(|button| button.label())
            .collect::<Vec<_>>()
            .join(" ");
        let names = if names.is_empty() { "-" } else { &names };
        let (lx, ly) = self.left_stick();
        let (rx, ry) = self.right_stick();
        write!(
            f,
            "[{}] {:<24} LT {:.2} RT {:.2}  L({:+.2},{:+.2}) R({:+.2},{:+.2})",
            self.index, names, self.left_trigger, self.right_trigger, lx, ly, rx, ry
        )
    }
}

/// Scale a raw stick reading to -1.0..1.0, keeping each end of the range.
///
/// The two ends are not symmetric, so each is divided by its own limit. Negation
/// happens in `f32` rather than `i16` on purpose: pushing a stick fully up or fully
/// down yields `i16::MIN`, and negating that overflows.
fn normalize(raw: f32) -> f32 {
    if raw < 0.0 {
        raw / -SHORT_MIN
    } else {
        raw / SHORT_MAX
    }
}

/// Rescale one axis so the deadzone remaps to exactly 0.0 and 1.0 stays 1.0.
fn with_deadzone(value: f32, deadzone: f32) -> f32 {
    if value.abs() <= deadzone {
        return 0.0;
    }
    let sign = if value < 0.0 { -1.0 } else { 1.0 };
    let scaled = sign * (value.abs() - deadzone) / (1.0 - deadzone);
    scaled.clamp(-1.0, 1.0)
}

/// Convert a raw thumb pair to a -1.0..1.0 pair with up being positive.
///
/// XInput reports the vertical axis as negative when pushed up, so the sign is
/// flipped here for every pad regardless of what its driver does.
fn stick(x: i16, y: i16, deadzone: f32, invert_x: bool, invert_y: bool) -> (f32, f32) {
    let x = f32::from(x);
    let y = f32::from(y);
    let x = if invert_x { -x } else { x };
    let y = if invert_y { -y } else { y };
    (
        with_deadzone(normalize(x), deadzone),
        with_deadzone(normalize(-y), deadzone),
    )
}

fn trigger(raw: u8) -> f32 {
    let value = raw as f32 / 255.0;
    if value < TRIGGER_THRESHOLD {
        0.0
    } else {
        value
    }
}

/// One axis's bias and scale, already in physical units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisCalibration {
    pub bias: f32,
    pub scale: f32,
}

impl AxisCalibration {
    pub fn apply(&self, raw: i16) -> f32 {
        (f32::from(raw) - self.bias) * self.scale
    }
}

/// Per-axis IMU calibration, turning raw counts into deg/s and g.
///
/// `hardware` says whether the pad actually supplied a usable calibration report.
/// When it is false the axes are identity, so values stay comparable with the raw
/// counts rather than quietly meaning something wrong.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    pub gyro: [AxisCalibration; 3],
    pub accel: [AxisCalibration; 3],
    pub hardware: bool,
}

impl Calibration {
    /// Counts pass straight through: what a pad with no usable report gets.
    pub const UNCALIBRATED: Self = Self {
        gyro: [
            AxisCalibration {
                bias: 0.0,
                scale: 1.0,
            },
            AxisCalibration {
                bias: 0.0,
                scale: 1.0,
            },
            AxisCalibration {
                bias: 0.0,
                scale: 1.0,
            },
        ],
        accel: [
            AxisCalibration {
                bias: 0.0,
                scale: 1.0,
            },
            AxisCalibration {
                bias: 0.0,
                scale: 1.0,
            },
            AxisCalibration {
                bias: 0.0,
                scale: 1.0,
            },
        ],
        hardware: false,
    };

    pub fn gyro_per_second(&self, raw: [i16; 3]) -> [f32; 3] {
        [
            self.gyro[0].apply(raw[0]),
            self.gyro[1].apply(raw[1]),
            self.gyro[2].apply(raw[2]),
        ]
    }

    pub fn accel_g(&self, raw: [i16; 3]) -> [f32; 3] {
        [
            self.accel[0].apply(raw[0]),
            self.accel[1].apply(raw[1]),
            self.accel[2].apply(raw[2]),
        ]
    }
}

/// An IMU sample, if the pad has one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    /// Degrees per second, after the pad's own calibration.
    pub gyro: [f32; 3],
    /// g, after the pad's own calibration.
    pub accel: [f32; 3],
    /// Untouched little-endian counts, for when there is no usable calibration.
    pub raw_gyro: [i16; 3],
    pub raw_accel: [i16; 3],
    /// 5.33 microsecond ticks since the pad booted, wrapping at 16 bits. Enough to
    /// spot a stalled or restarted report stream.
    pub timestamp: u16,
    /// Whether the pad actually supplied a calibration report.
    pub hardware_calibration: bool,
}

impl fmt::Display for Motion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "t {:5}  gyro {:+7.2} {:+7.2} {:+7.2} deg/s  [{:>6} {:>6} {:>6}]  \
             accel {:+6.3} {:+6.3} {:+6.3} g  [{:>6} {:>6} {:>6}]",
            self.timestamp,
            self.gyro[0],
            self.gyro[1],
            self.gyro[2],
            self.raw_gyro[0],
            self.raw_gyro[1],
            self.raw_gyro[2],
            self.accel[0],
            self.accel[1],
            self.accel[2],
            self.raw_accel[0],
            self.raw_accel[1],
            self.raw_accel[2],
        )
    }
}

/// Where a [`Gamepad`] gets its state from.
///
/// `XInput` and the raw-HID DualShock driver both satisfy this, so anything built on
/// top of `Gamepad` — the listener and the indicator window — stays unchanged.
pub trait GamepadBackend {
    /// The highest index [`GamepadBackend::get_state`] can answer for.
    fn max_index(&self) -> u8;

    /// The current state at `index`, or `None` while nothing is connected.
    fn get_state(&self, index: u8) -> anyhow::Result<Option<RawState>>;

    /// Drive the rumble motors, 0.0-1.0 each. `false` if that pad is not there.
    fn set_vibration(&self, index: u8, left: f32, right: f32) -> anyhow::Result<bool>;

    /// Every slot that currently holds a controller.
    fn connected_indices(&self) -> anyhow::Result<Vec<u8>>;

    /// Whether this backend's pads can carry an IMU at all.
    ///
    /// The Python original got this from `isinstance(backend, MotionSource)`; here
    /// it has to be a method, because the window asks before the pad is plugged in
    /// and a sample may not have arrived yet. Only backends that really can supply
    /// one should override it — the window sizes itself from this.
    fn supports_motion(&self) -> bool {
        false
    }

    /// The newest IMU sample, or `None` when this pad has no motion sensor.
    fn motion(&self, _index: u8) -> Option<Motion> {
        None
    }

    /// The calibration in force for a pad, or `None` when the backend has none to
    /// give. The CLI prints this before streaming motion samples.
    fn calibration(&self, _index: u8) -> Option<Calibration> {
        None
    }

    /// The pad's product name, when the backend knows it. Used for the window title.
    fn name(&self, _index: u8) -> Option<String> {
        None
    }

    /// Release anything the backend holds open.
    fn close(&self) {}
}

/// Polls a single controller slot.
///
/// Unlike the Python original this does not own its backend: the CLI opens one and
/// hands it in, so both backends can be opened and dropped in the same place.
pub struct Gamepad<'a> {
    index: u8,
    deadzone: f32,
    invert_x: bool,
    invert_y: bool,
    backend: &'a dyn GamepadBackend,
}

impl<'a> Gamepad<'a> {
    /// # Errors
    /// If `index` is not a slot this backend can answer for.
    pub fn new(
        index: u8,
        deadzone: f32,
        invert_x: bool,
        invert_y: bool,
        backend: &'a dyn GamepadBackend,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            index <= backend.max_index(),
            "index must be 0..={}, got {index}",
            backend.max_index()
        );
        Ok(Self {
            index,
            deadzone,
            invert_x,
            invert_y,
            backend,
        })
    }

    pub fn index(&self) -> u8 {
        self.index
    }

    pub fn backend(&self) -> &'a dyn GamepadBackend {
        self.backend
    }

    /// Whether this pad has an IMU at all, whether or not it is plugged in.
    pub fn has_motion(&self) -> bool {
        self.backend.supports_motion()
    }

    /// The current state, or `None` while the pad is disconnected.
    pub fn poll(&self) -> anyhow::Result<Option<GamepadState>> {
        Ok(self.backend.get_state(self.index)?.map(|raw| {
            GamepadState::from_raw(self.index, raw, self.deadzone, self.invert_x, self.invert_y)
        }))
    }

    /// The newest IMU sample, or `None` when the pad has no motion sensor.
    pub fn motion(&self) -> Option<Motion> {
        self.backend.motion(self.index)
    }

    pub fn set_vibration(&self, left: f32, right: f32) -> anyhow::Result<bool> {
        self.backend.set_vibration(self.index, left, right)
    }

    pub fn stop_vibration(&self) -> anyhow::Result<bool> {
        self.set_vibration(0.0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DZ: f32 = DEFAULT_DEADZONE;

    #[test]
    fn normalize_keeps_both_ends_exact() {
        assert_eq!(normalize(f32::from(i16::MIN)), -1.0);
        assert_eq!(normalize(f32::from(i16::MAX)), 1.0);
        assert_eq!(normalize(0.0), 0.0);
    }

    #[test]
    fn a_fully_pushed_stick_never_overflows() {
        // i16::MIN is what a fully pushed axis reads as, and negating it in i16
        // would overflow. Both directions have to come out as clean full scale.
        let (x, y) = stick(i16::MIN, i16::MIN, 0.0, false, false);
        assert_eq!((x, y), (-1.0, 1.0));
        let (x, y) = stick(i16::MIN, i16::MIN, 0.0, true, true);
        assert_eq!((x, y), (1.0, -1.0));
    }

    #[test]
    fn deadzone_maps_to_zero_and_keeps_full_scale() {
        assert_eq!(with_deadzone(0.0, DZ), 0.0);
        assert_eq!(with_deadzone(DZ, DZ), 0.0);
        assert_eq!(with_deadzone(-DZ, DZ), 0.0);
        // Anything just outside the deadzone is nudged, not clamped to zero.
        assert!(with_deadzone(DZ * 1.01, DZ) > 0.0);
        assert!(with_deadzone(-DZ * 1.01, DZ) < 0.0);
        assert_eq!(with_deadzone(1.0, DZ), 1.0);
        assert_eq!(with_deadzone(-1.0, DZ), -1.0);
    }

    #[test]
    fn deadzone_rescale_is_continuous_at_the_edge() {
        // The remap has to send the deadzone edge to 0 and the full-scale end to 1
        // without a step in between.
        let edge = with_deadzone(DZ, DZ);
        let just_past = with_deadzone(DZ + 1.0 / 32767.0, DZ);
        assert!(
            just_past > 0.0 && just_past < 0.01,
            "step at the edge: {just_past}"
        );
        assert!(edge == 0.0);
    }

    #[test]
    fn deadzone_never_exceeds_full_scale() {
        assert_eq!(with_deadzone(2.0, DZ), 1.0);
        assert_eq!(with_deadzone(-2.0, DZ), -1.0);
    }

    #[test]
    fn up_is_positive() {
        // XInput reports pushing up as a negative raw value.
        let (x, y) = stick(0, i16::MIN, DZ, false, false);
        assert!(x == 0.0);
        assert_eq!(y, 1.0);
    }

    #[test]
    fn invert_flips_the_axis() {
        // x is fully left, y fully down; flipping both puts them the other way.
        let (x, y) = stick(i16::MIN, i16::MAX, DZ, true, true);
        assert_eq!(x, 1.0);
        assert_eq!(y, 1.0);
    }

    #[test]
    fn inverting_a_centred_axis_changes_nothing() {
        let (x, y) = stick(0, 0, DZ, true, true);
        assert_eq!((x, y), (0.0, 0.0));
    }

    #[test]
    fn invert_x_does_not_disturb_y() {
        let (x, y) = stick(i16::MIN, i16::MIN, DZ, true, false);
        assert_eq!(x, 1.0);
        assert_eq!(y, 1.0);
    }

    #[test]
    fn trigger_threshold_keeps_an_untouched_pad_quiet() {
        assert_eq!(trigger(0), 0.0);
        // The threshold is 0.02, which lands between 5 and 6 out of 255.
        assert_eq!(trigger(5), 0.0);
        assert_eq!(trigger(6), 6.0 / 255.0);
        assert_eq!(trigger(255), 1.0);
    }

    #[test]
    fn from_raw_maps_every_field() {
        let raw = RawState {
            packet_number: 7,
            buttons: Button::A.bits() | Button::DPAD_UP.bits(),
            left_trigger: 255,
            right_trigger: 0,
            left_x: i16::MAX,
            left_y: i16::MIN,
            right_x: 0,
            right_y: 0,
        };
        let state = GamepadState::from_raw(1, raw, DZ, false, false);
        assert_eq!(state.index, 1);
        assert_eq!(state.packet_number, 7);
        assert!(state.is_pressed(Button::A));
        assert!(state.is_pressed(Button::DPAD_UP));
        assert!(!state.is_pressed(Button::B));
        assert_eq!(state.left_trigger, 1.0);
        assert_eq!(state.right_trigger, 0.0);
        assert_eq!(state.left_x, 1.0);
        assert_eq!(state.left_y, 1.0);
    }

    #[test]
    fn every_wire_bit_is_a_known_button() {
        // All 16 bits of wButtons are spoken for, so nothing is silently dropped.
        for bit in 0..16 {
            let bits = 1u16 << bit;
            assert!(
                Button::from_bits(bits).is_some(),
                "0x{bits:04x} would be dropped on the way in"
            );
        }
    }

    #[test]
    fn pressed_buttons_are_in_declaration_order() {
        let raw = RawState {
            buttons: Button::Y.bits() | Button::DPAD_UP.bits() | Button::A.bits(),
            ..RawState::default()
        };
        let state = GamepadState::from_raw(0, raw, DZ, false, false);
        let labels: Vec<_> = state.pressed_buttons().map(|b| b.label()).collect();
        assert_eq!(labels, ["D-pad up", "A", "Y"]);
    }

    #[test]
    fn summary_falls_back_to_a_dash() {
        let state = GamepadState::default();
        assert!(state.to_string().contains("[0] -"));
    }

    #[test]
    fn every_button_has_a_distinct_label() {
        let mut labels: Vec<&str> = ALL_BUTTONS.iter().map(|b| b.label()).collect();
        labels.sort_unstable();
        let before = labels.len();
        labels.dedup();
        assert_eq!(before, labels.len(), "two buttons share a label");
    }
}
