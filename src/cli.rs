// Copyright (C) 2026 hollykbuck
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Command line entry point: watch a controller and report its input.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Parser};

use crate::gamepad::{DEFAULT_DEADZONE, Gamepad, GamepadBackend};

#[derive(Parser, Debug)]
#[command(
    name = "controllerindicator",
    about = "Listen to controller input: Xbox One/Series and 360 pads over XInput, \
             DualShock 4 pads straight from HID."
)]
#[command(group(
    ArgGroup::new("mode")
        .multiple(false)
        .args(["list", "once", "rumble", "motion", "window"]),
))]
pub struct Args {
    /// Controller slot to watch (default: 0)
    #[arg(short, long, value_name = "N", default_value_t = 0)]
    pub index: u8,

    /// Where to read input from: xinput or ps4 (default: xinput)
    #[arg(short, long, value_name = "BACKEND", default_value = "xinput")]
    pub backend: String,

    /// List connected slots and exit
    #[arg(long)]
    pub list: bool,

    /// Print one state snapshot and exit
    #[arg(long)]
    pub once: bool,

    /// Test the rumble motors for SECONDS (default: 1.0)
    #[arg(long, value_name = "SECONDS", num_args = 0..=1, default_missing_value = "1.0")]
    pub rumble: Option<f32>,

    /// Stream raw gyroscope and accelerometer samples (ps4 only)
    #[arg(long)]
    pub motion: bool,

    /// Open a window that draws the controller state
    #[arg(short = 'w', long)]
    pub window: bool,

    /// Keep the window above other windows
    #[arg(long, requires = "window")]
    pub topmost: bool,

    /// No title bar; drag the top strip
    #[arg(long, requires = "window")]
    pub borderless: bool,

    /// Ignore mouse clicks
    #[arg(long, requires = "window")]
    pub click_through: bool,

    /// Window opacity 0.0-1.0 (default: 1.0)
    #[arg(long, value_name = "A", default_value_t = 1.0, requires = "window")]
    pub opacity: f32,

    /// Stick deadzone, 0.0-1.0
    #[arg(long, value_name = "R", default_value_t = DEFAULT_DEADZONE)]
    pub deadzone: f32,

    /// Flip the horizontal stick axis
    #[arg(long)]
    pub invert_x: bool,

    /// Flip the vertical stick axis (for pads whose driver reports up as +)
    #[arg(long)]
    pub invert_y: bool,

    /// Poll rate (default: 120)
    #[arg(long = "poll-hz", value_name = "HZ", default_value_t = 120.0)]
    pub poll_hz: f32,
}

/// Which backend to read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    XInput,
    Ps4,
}

impl BackendKind {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "xinput" => Ok(Self::XInput),
            "ps4" => Ok(Self::Ps4),
            other => bail!("unknown backend {other:?}: expected xinput or ps4"),
        }
    }
}

/// Exit code for "nothing was found", matching the Python original.
const EXIT_NOTHING: i32 = 1;

/// The gap between polls. A rate below 1 Hz is clamped rather than dividing by zero.
fn poll_interval(hz: f32) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(hz.max(1.0)))
}

/// Run the program. `Ok(0)` means success; an `Err` is reported and exits 1.
pub fn run() -> Result<i32> {
    let args = Args::parse();
    let kind = BackendKind::parse(&args.backend)?;

    if args.list {
        return list_slots(kind);
    }

    // The window needs the gamepad to borrow, so it takes the backend for its
    // whole life; every other mode only holds it long enough to read.
    if args.window {
        return run_window(&args, kind);
    }

    match kind {
        BackendKind::XInput => {
            let backend = crate::xinput::XInput::new().context("XInput is unavailable")?;
            run_console(&args, &backend)
        }
        BackendKind::Ps4 => {
            let backend = crate::ps4::PS4::new().context("Win32 HID is unavailable")?;
            run_console(&args, &backend)
        }
    }
}

fn list_slots(kind: BackendKind) -> Result<i32> {
    let slots = match kind {
        BackendKind::XInput => {
            let backend = crate::xinput::XInput::new()?;
            backend.connected_indices()?
        }
        BackendKind::Ps4 => {
            let backend = crate::ps4::PS4::new()?;
            // The HID backend still has to open the device and wait for the first
            // report, so an immediate query would come back empty.
            settle_slots(&backend, Duration::from_secs(1))
        }
    };
    let names: Vec<String> = slots.iter().map(u8::to_string).collect();
    let joined = if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    };
    println!("connected slots: {joined}");
    Ok(if slots.is_empty() { EXIT_NOTHING } else { 0 })
}

/// Poll for a non-empty slot list until the timeout, then return whatever the last
/// poll saw.
fn settle_slots(backend: &dyn GamepadBackend, timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    loop {
        let slots = backend.connected_indices().unwrap_or_default();
        if !slots.is_empty() || Instant::now() >= deadline {
            return slots;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_console(args: &Args, backend: &dyn GamepadBackend) -> Result<i32> {
    let gamepad = Gamepad::new(
        args.index,
        args.deadzone,
        args.invert_x,
        args.invert_y,
        backend,
    )?;
    let interval = poll_interval(args.poll_hz);

    if args.motion {
        return motion_stream(args, &gamepad);
    }
    if let Some(seconds) = args.rumble {
        return rumble_test(args, &gamepad, seconds);
    }
    if args.once {
        return once(args, &gamepad);
    }

    if gamepad.poll()?.is_none() {
        println!("waiting for a controller in slot {}...", args.index);
    }
    println!("press ctrl+c to quit");

    // stdout has to be flushed per line or a piped stream shows nothing until the
    // buffer fills, which for an event stream is most of its life.
    let mut out = std::io::stdout();
    crate::listener::watch(&gamepad, interval, |event| {
        let _ = writeln!(out, "{event}");
        let _ = out.flush();
    });
    Ok(0)
}

fn once(args: &Args, gamepad: &Gamepad<'_>) -> Result<i32> {
    settle(gamepad, Duration::from_secs(1));
    match gamepad.poll()? {
        Some(state) => {
            println!("{state}");
            Ok(0)
        }
        None => {
            eprintln!("error: no controller in slot {}", args.index);
            Ok(EXIT_NOTHING)
        }
    }
}

fn rumble_test(args: &Args, gamepad: &Gamepad<'_>, seconds: f32) -> Result<i32> {
    settle(gamepad, Duration::from_secs(1));
    if gamepad.poll()?.is_none() {
        eprintln!("error: no controller in slot {}", args.index);
        return Ok(EXIT_NOTHING);
    }
    println!("rumbling slot {} for {seconds:.1}s", args.index);
    gamepad.set_vibration(0.6, 0.9)?;
    std::thread::sleep(Duration::from_secs_f32(seconds.max(0.0)));
    let stopped = gamepad.stop_vibration()?;
    println!(
        "{}",
        if stopped {
            "stopped"
        } else {
            "warning: could not stop the motors"
        }
    );
    Ok(0)
}
fn motion_stream(args: &Args, gamepad: &Gamepad<'_>) -> Result<i32> {
    if !gamepad.has_motion() {
        eprintln!("error: motion sensors need the ps4 backend");
        return Ok(EXIT_NOTHING);
    }
    settle(gamepad, Duration::from_secs(1));
    if gamepad.poll()?.is_none() {
        eprintln!("error: no controller in slot {}", args.index);
        return Ok(EXIT_NOTHING);
    }

    let backend = gamepad.backend();
    if let Some(calibration) = backend.calibration(gamepad.index()) {
        if calibration.hardware {
            println!("factory calibration applied; ctrl+c to quit");
            for (axis, name) in calibration.gyro.iter().zip(["pitch", "yaw", "roll"]) {
                println!(
                    "  gyro {:<5} bias {:8.1}  {:.6} deg/s per count",
                    name, axis.bias, axis.scale
                );
            }
            for (axis, name) in calibration
                .accel
                .iter()
                .zip(["accel x", "accel y", "accel z"])
            {
                println!(
                    "  {:<10} bias {:8.1}  {:.6} g per count",
                    name, axis.bias, axis.scale
                );
            }
        } else {
            println!("no usable calibration report, values are raw counts; ctrl+c to quit");
        }
    }

    let interval = poll_interval(args.poll_hz);
    let mut shown: Option<u16> = None;
    let mut out = std::io::stdout();
    loop {
        if let Some(sample) = backend.motion(gamepad.index())
            && shown != Some(sample.timestamp)
        {
            shown = Some(sample.timestamp);
            let _ = writeln!(out, "{sample}");
            let _ = out.flush();
        }
        std::thread::sleep(interval);
    }
}

fn run_window(args: &Args, kind: BackendKind) -> Result<i32> {
    let backend: Box<dyn GamepadBackend> = match kind {
        BackendKind::XInput => {
            Box::new(crate::xinput::XInput::new().context("XInput is unavailable")?)
        }
        BackendKind::Ps4 => Box::new(crate::ps4::PS4::new().context("Win32 HID is unavailable")?),
    };
    let gamepad = Gamepad::new(
        args.index,
        args.deadzone,
        args.invert_x,
        args.invert_y,
        backend.as_ref(),
    )?;
    let title = format!(
        "Controller Input - {}",
        backend
            .name(args.index)
            .unwrap_or_else(|| format!("slot {}", args.index))
    );
    crate::window::IndicatorWindow::new(
        &gamepad,
        &title,
        args.topmost,
        args.borderless,
        args.click_through,
        args.opacity,
        args.poll_hz,
    )?
    .run()
}

/// Wait for a pad to show up, then hand back whatever the last poll saw.
///
/// XInput sees a pad the moment it is plugged in, but the HID backend still has to
/// open the device and wait for the first report to arrive.
fn settle(gamepad: &Gamepad<'_>, timeout: Duration) -> Option<crate::GamepadState> {
    let deadline = Instant::now() + timeout;
    loop {
        let state = gamepad.poll().ok().flatten();
        if state.is_some() || Instant::now() >= deadline {
            return state;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_definition_is_valid() {
        Args::command().debug_assert();
    }

    #[test]
    fn defaults_match_the_documented_ones() {
        let args = Args::parse_from(["controllerindicator"]);
        assert_eq!(args.index, 0);
        assert_eq!(args.backend, "xinput");
        assert_eq!(args.deadzone, DEFAULT_DEADZONE);
        assert_eq!(args.poll_hz, 120.0);
        assert_eq!(args.opacity, 1.0);
        assert!(!args.invert_x);
        assert!(!args.invert_y);
        assert!(args.rumble.is_none());
    }

    #[test]
    fn every_flag_parses() {
        let args = Args::parse_from([
            "controllerindicator",
            "-i",
            "2",
            "-b",
            "ps4",
            "--deadzone",
            "0.1",
            "--invert-x",
            "--invert-y",
            "--poll-hz",
            "60",
        ]);
        assert_eq!(args.index, 2);
        assert_eq!(args.backend, "ps4");
        assert!((args.deadzone - 0.1).abs() < 1e-6);
        assert!(args.invert_x && args.invert_y);
        assert!((args.poll_hz - 60.0).abs() < 1e-6);
    }

    #[test]
    fn window_options_are_only_meaningful_with_the_window() {
        assert!(
            Args::try_parse_from(["controllerindicator", "--topmost"]).is_err(),
            "--topmost alone should be rejected"
        );
        assert!(Args::try_parse_from(["controllerindicator", "-w", "--topmost"]).is_ok());
    }

    #[test]
    fn two_modes_at_once_are_rejected() {
        assert!(Args::try_parse_from(["controllerindicator", "--list", "--once"]).is_err());
        assert!(Args::try_parse_from(["controllerindicator", "--list", "-w"]).is_err());
        assert!(Args::try_parse_from(["controllerindicator", "--motion", "--rumble"]).is_err());
    }

    #[test]
    fn a_mode_on_its_own_is_fine() {
        assert!(Args::try_parse_from(["controllerindicator", "--list"]).is_ok());
        assert!(Args::try_parse_from(["controllerindicator", "-w"]).is_ok());
        assert!(Args::try_parse_from(["controllerindicator", "--rumble"]).is_ok());
    }

    #[test]
    fn rumble_defaults_to_one_second_when_bare() {
        let args = Args::parse_from(["controllerindicator", "--rumble"]);
        assert_eq!(args.rumble, Some(1.0));
        let args = Args::parse_from(["controllerindicator", "--rumble", "2"]);
        assert_eq!(args.rumble, Some(2.0));
    }

    #[test]
    fn backend_names_round_trip() {
        assert_eq!(BackendKind::parse("xinput").unwrap(), BackendKind::XInput);
        assert_eq!(BackendKind::parse("ps4").unwrap(), BackendKind::Ps4);
        assert!(BackendKind::parse("nope").is_err());
    }
}
