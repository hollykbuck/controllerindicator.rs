// Copyright (C) 2026 hollykbuck
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Watch what a game controller is doing, on Windows.
//!
//! A Rust port of the Python `xboxindicator`, which used `ctypes` against the same
//! Win32 APIs. The layering carries over unchanged: [`Gamepad`] talks to a
//! [`GamepadBackend`], and `XInput` and `PS4` both satisfy it, so the listener and
//! the indicator window never had to know which one they were driving.

pub mod gamepad;
pub mod listener;

#[cfg(windows)]
pub mod hid;
#[cfg(windows)]
pub mod ps4;
#[cfg(windows)]
pub mod window;
#[cfg(windows)]
pub mod xinput;

#[cfg(windows)]
pub mod cli;

pub use gamepad::{
    AxisCalibration, Button, Calibration, DEFAULT_DEADZONE, Gamepad, GamepadBackend, GamepadState,
    Motion, RawState,
};
