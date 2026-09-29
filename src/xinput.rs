// Copyright (C) 2026 hollykbuck
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! XInput: the shortest route to an Xbox One, Series or 360 pad.
//!
//! The `windows` crate links XInput straight to `xinput1_4.dll`, which would make
//! that DLL a hard startup requirement. It is missing on Windows 7 and on some
//! stripped installs, so the two older libraries are kept as fallbacks and the
//! entry points are resolved at runtime instead — the same order the Python
//! original used.

use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Input::XboxController::{XINPUT_GAMEPAD, XINPUT_STATE, XINPUT_VIBRATION};
use windows::core::{PCSTR, PCWSTR};

use crate::gamepad::{GamepadBackend, RawState};

/// XInput addresses four slots, 0 through 3.
pub const MAX_USER_INDEX: u8 = 3;

/// Full speed for a motor, which is a `u16`.
const MAX_MOTOR_SPEED: u16 = 0xFFFF;

const ERROR_SUCCESS: u32 = 0;
const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

/// Newest first. Every one of these exports the same two entry points.
const DLL_CANDIDATES: [&str; 3] = ["xinput1_4", "xinput1_3", "xinput9_1_0"];

type XInputGetStateFn = unsafe extern "system" fn(u32, *mut XINPUT_STATE) -> u32;
type XInputSetStateFn = unsafe extern "system" fn(u32, *const XINPUT_VIBRATION) -> u32;

/// A loaded XInput library with its two entry points resolved.
pub struct XInput {
    _module: HMODULE,
    get_state: XInputGetStateFn,
    set_state: XInputSetStateFn,
}

/// Resolve one entry point on a loaded module.
///
/// The target type is left to the caller's binding, which is what keeps the
/// pointer-sized reinterpretation out of a generic function. A missing name comes
/// back as `None` so the caller can report which of the two was absent.
///
/// # Safety
/// `$module` must be a live module handle, and the resolved symbol must match the
/// type the caller binds it to.
macro_rules! entry_point {
    ($module:expr, $name:literal) => {{
        // GetProcAddress takes an ANSI name; every entry point we look up is ASCII.
        let address = unsafe { GetProcAddress($module, PCSTR(concat!($name, "\0").as_ptr())) };
        address.map(|f| unsafe { std::mem::transmute::<_, _>(f) })
    }};
}

impl XInput {
    /// Load whichever XInput library this machine has.
    ///
    /// # Errors
    /// If none of the candidates could be loaded, or the one that loaded did not
    /// export both entry points. Every failure is listed, because "no controller"
    /// and "no XInput at all" need to be told apart.
    pub fn new() -> anyhow::Result<Self> {
        let mut failures = Vec::new();
        for name in DLL_CANDIDATES {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: the string is NUL terminated and outlives the call.
            let module = match unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) } {
                Ok(module) => module,
                Err(err) => {
                    failures.push(format!("{name}: {err}"));
                    continue;
                }
            };
            // SAFETY: `module` came from LoadLibraryW above and is still live.
            let get_state: Option<XInputGetStateFn> = entry_point!(module, "XInputGetState");
            // SAFETY: as above.
            let set_state: Option<XInputSetStateFn> = entry_point!(module, "XInputSetState");
            match (get_state, set_state) {
                (Some(get_state), Some(set_state)) => {
                    return Ok(Self {
                        _module: module,
                        get_state,
                        set_state,
                    });
                }
                (found_get, _) => {
                    // SAFETY: loaded above, dropped exactly once here.
                    unsafe { FreeLibrary(module).ok() };
                    failures.push(format!(
                        "{name}: missing entry point ({})",
                        if found_get.is_none() {
                            "XInputGetState"
                        } else {
                            "XInputSetState"
                        }
                    ));
                }
            }
        }
        anyhow::bail!(
            "unable to load an XInput library (tried {}): {}",
            DLL_CANDIDATES.join(", "),
            failures.join("; ")
        )
    }
}

impl Drop for XInput {
    fn drop(&mut self) {
        // SAFETY: the handle came from LoadLibraryW in `new` and is dropped once.
        unsafe { FreeLibrary(self._module).ok() };
    }
}

impl GamepadBackend for XInput {
    fn max_index(&self) -> u8 {
        MAX_USER_INDEX
    }

    fn get_state(&self, index: u8) -> anyhow::Result<Option<RawState>> {
        let mut state = XINPUT_STATE::default();
        // SAFETY: `state` is a correctly sized, aligned out-parameter.
        let result = unsafe { (self.get_state)(u32::from(index), &raw mut state) };
        if result == ERROR_DEVICE_NOT_CONNECTED {
            return Ok(None);
        }
        if result != ERROR_SUCCESS {
            anyhow::bail!("XInputGetState({index}) failed with code {result}");
        }
        Ok(Some(from_xinput(&state)))
    }

    fn set_vibration(&self, index: u8, left: f32, right: f32) -> anyhow::Result<bool> {
        let vibration = XINPUT_VIBRATION {
            wLeftMotorSpeed: motor_speed(left),
            wRightMotorSpeed: motor_speed(right),
        };
        // SAFETY: `vibration` is borrowed for the call only, which is all an
        // in-parameter needs.
        let result = unsafe { (self.set_state)(u32::from(index), &raw const vibration) };
        Ok(result == ERROR_SUCCESS)
    }

    fn connected_indices(&self) -> anyhow::Result<Vec<u8>> {
        let mut found = Vec::new();
        for index in 0..=MAX_USER_INDEX {
            if self.get_state(index)?.is_some() {
                found.push(index);
            }
        }
        Ok(found)
    }
}

/// Flatten the Win32 report into our own shape.
fn from_xinput(state: &XINPUT_STATE) -> RawState {
    let gamepad: &XINPUT_GAMEPAD = &state.Gamepad;
    RawState {
        packet_number: state.dwPacketNumber,
        buttons: gamepad.wButtons.0,
        left_trigger: gamepad.bLeftTrigger,
        right_trigger: gamepad.bRightTrigger,
        left_x: gamepad.sThumbLX,
        left_y: gamepad.sThumbLY,
        right_x: gamepad.sThumbRX,
        right_y: gamepad.sThumbRY,
    }
}

/// 0.0-1.0 onto the full motor range, rounded the way the Python original did.
fn motor_speed(amount: f32) -> u16 {
    (amount.clamp(0.0, 1.0) * f32::from(MAX_MOTOR_SPEED)).round() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motor_speed_spans_the_whole_range() {
        assert_eq!(motor_speed(0.0), 0);
        assert_eq!(motor_speed(1.0), MAX_MOTOR_SPEED);
        assert_eq!(motor_speed(0.5), 32768);
    }

    #[test]
    fn motor_speed_clamps_out_of_range_input() {
        assert_eq!(motor_speed(-3.0), 0);
        assert_eq!(motor_speed(4.0), MAX_MOTOR_SPEED);
        assert_eq!(motor_speed(f32::NAN), 0);
    }

    #[test]
    fn from_xinput_copies_every_field() {
        use windows::Win32::UI::Input::XboxController::XINPUT_GAMEPAD_BUTTON_FLAGS;

        let state = XINPUT_STATE {
            dwPacketNumber: 42,
            Gamepad: XINPUT_GAMEPAD {
                wButtons: XINPUT_GAMEPAD_BUTTON_FLAGS(0x1234),
                bLeftTrigger: 10,
                bRightTrigger: 20,
                sThumbLX: -100,
                sThumbLY: 200,
                sThumbRX: 300,
                sThumbRY: -400,
            },
        };
        let raw = from_xinput(&state);
        assert_eq!(raw.packet_number, 42);
        assert_eq!(raw.buttons, 0x1234);
        assert_eq!(raw.left_trigger, 10);
        assert_eq!(raw.right_trigger, 20);
        assert_eq!(raw.left_x, -100);
        assert_eq!(raw.left_y, 200);
        assert_eq!(raw.right_x, 300);
        assert_eq!(raw.right_y, -400);
    }

    #[test]
    fn max_index_is_three() {
        assert_eq!(MAX_USER_INDEX, 3);
    }

    #[test]
    fn the_struct_layout_matches_what_win32_expects() {
        // A mismatched layout would read garbage, so pin the sizes down.
        use std::mem::size_of;
        assert_eq!(size_of::<XINPUT_GAMEPAD>(), 12);
        assert_eq!(size_of::<XINPUT_STATE>(), 16);
        assert_eq!(size_of::<XINPUT_VIBRATION>(), 4);
    }
}
