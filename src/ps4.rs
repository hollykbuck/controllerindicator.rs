// Copyright (C) 2026 hollykbuck
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! DualShock 4 report decoding, read straight from Win32 HID and reported in the
//! shape of an XInput pad.
//!
//! The report layouts and framing rules follow SDL's `SDL_hidapi_ps4.c`, which
//! itself credits Valve for the Bluetooth work. Only what an input indicator needs
//! is here: buttons, sticks, triggers, rumble and hotplug.
//!
//! The report decoder is a pure function of its bytes, so every rule in it is
//! covered by unit tests in this file. Only [`PS4`] itself, at the bottom, touches
//! hardware.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::gamepad::{AxisCalibration, Calibration, GamepadBackend, Motion, RawState};
use crate::hid::{HidDevice, HidDeviceInfo, MAX_REPORT_SIZE, enumerate_hid_devices};

pub const SONY_VENDOR_ID: u16 = 0x054C;
pub const DS4_PRODUCT_IDS: [u16; 3] = [0x05C4, 0x09CC, 0x0BA0];

/// Report ids that carry button and stick state.
const USB_STATE: u8 = 0x01;
const BT_STATE_FIRST: u8 = 0x11;
const BT_STATE_LAST: u8 = 0x19;

const USB_SIMPLE_SIZE: usize = 10;
const USB_EXTENDED_SIZE: usize = 64;
/// The bit at byte 31 that marks a report as coming from the wireless dongle
/// rather than a pad.
const DONGLE_PRESENT_OFFSET: usize = 31;
const DONGLE_PRESENT_MASK: u8 = 0x04;

const BT_PACKET_SIZE: usize = 78;
const BT_HID_PRESENT: u8 = 0x80;
const BT_MAGIC: u8 = 0xC0;
const BT_REPORT_INTERVAL: u8 = 0x04;
const BT_CRC_HEADER: u8 = 0xA2;
const STATE_CRC_HEADER: u8 = 0xA1;

const BT_EFFECTS: u8 = 0x11;
const USB_EFFECTS: u8 = 0x05;
const USB_EFFECTS_SIZE: usize = 32;

// Byte offsets inside the state packet; the USB and Bluetooth layouts share them.
const LX: usize = 0;
const LY: usize = 1;
const RX: usize = 2;
const RY: usize = 3;
const FACE: usize = 4;
const SHOULDERS: usize = 5;
const SYSTEM: usize = 6;
const LEFT_TRIGGER: usize = 7;
const RIGHT_TRIGGER: usize = 8;
const TIMESTAMP: usize = 9;
const GYRO_X: usize = 12;
const GYRO_Y: usize = 14;
const GYRO_Z: usize = 16;
const ACCEL_X: usize = 18;
const ACCEL_Y: usize = 20;
const ACCEL_Z: usize = 22;

// Offsets of the high and low readings of each accelerometer axis inside the
// factory calibration report. Each axis owns a four byte pair, not a two byte stride.
const ACCEL_PLUS: usize = 23;
const ACCEL_MINUS: usize = 25;

const EFFECT_RUMBLE: u8 = 0x01;
const EFFECT_LED: u8 = 0x02;

// Factory calibration lives in a feature report, not in the input stream.
const GYRO_CALIBRATION_USB: u8 = 0x02;
const GYRO_CALIBRATION_BT: u8 = 0x05;
const CALIBRATION_LENGTH: usize = 64;
const CALIBRATION_MIN_LENGTH: usize = 35;
const CALIBRATION_ATTEMPTS: usize = 5;
const CALIBRATION_RETRY_DELAY: Duration = Duration::from_millis(2);

// The pad's nominal sensitivities, as implied by Sony's report layout. Only
// third-party pads report different ones, and those are not enumerated here.
const GYRO_NUMERATOR: f32 = 1.0;
const GYRO_DENOMINATOR: f32 = 16.0;
const ACCEL_NUMERATOR: f32 = 1.0;
const ACCEL_DENOMINATOR: f32 = 8192.0;

/// Sanity limits on a calibration report, matching SDL's.
const MAX_PLAUSIBLE_BIAS: f32 = 1024.0;
const MAX_PLAUSIBLE_SHIFT: f32 = 0.5;

/// True when `CONTROLLERINDICATOR_HID_DEBUG=1`, which dumps every raw report.
fn debug_enabled() -> bool {
    std::env::var_os("CONTROLLERINDICATOR_HID_DEBUG").is_some_and(|value| value == "1")
}

/// A Bluetooth pad that stops reporting is only treated as gone after this long.
const BLUETOOTH_QUIET_SECONDS: f64 = 0.5;
const BLUETOOTH_LOST_SECONDS: f64 = 3.0;
/// Consecutive bad checksums tolerated before a report is thrown away.
const CRC_FAILURES_ALLOWED: u32 = 3;

/// The player colours the DualShock uses to tell pads apart, from hid-sony.c.
const PLAYER_COLORS: [[u8; 3]; 7] = [
    [0x00, 0x00, 0x40], // blue
    [0x40, 0x00, 0x00], // red
    [0x00, 0x40, 0x00], // green
    [0x20, 0x00, 0x20], // pink
    [0x02, 0x01, 0x00], // orange
    [0x00, 0x01, 0x01], // teal
    [0x01, 0x01, 0x01], // white
];

/// A DualShock's name, falling back to a guess when the pad will not say.
pub fn product_name(product_id: u16) -> &'static str {
    match product_id {
        0x05C4 => "DualShock 4",
        0x09CC => "DualShock 4 v3",
        0x0BA0 => "DualShock 4 Edge",
        _ => "DualShock 4",
    }
}

/// A decoded report: XInput-shaped buttons and sticks, plus the raw IMU counts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decoded {
    pub state: RawState,
    pub raw_gyro: [i16; 3],
    pub raw_accel: [i16; 3],
    pub timestamp: u16,
    /// False only for a Bluetooth report that arrived with a bad CRC, which the
    /// caller may tolerate for a few packets in a row.
    pub checksum_ok: bool,
}

/// The face buttons, mapped by position rather than by symbol.
///
/// The pad's symbols are mapped to the Xbox layout they correspond to positionally,
/// so X is the square, A the cross, B the circle and Y the triangle. These are the
/// raw `wButtons` bits, spelled numerically because `bitflags` cannot be combined
/// in a const.
const FACE_BUTTONS: [(u8, u16); 4] = [
    (0x10, 0x4000), // square -> X
    (0x20, 0x1000), // cross  -> A
    (0x40, 0x2000), // circle -> B
    (0x80, 0x8000), // triangle -> Y
];

/// The hat, which is four bits where several can be set at once. Positions 8 to 15
/// are the "hat centred" cases and press nothing.
const HAT_BUTTONS: [u16; 16] = [
    0x0001, // up
    0x0009, // up + right
    0x0008, // right
    0x000A, // right + down
    0x0002, // down
    0x0006, // down + left
    0x0004, // left
    0x0005, // left + up
    0, 0, 0, 0, 0, 0, 0, 0,
];

const SHOULDER_BUTTONS: [(u8, u16); 6] = [
    (0x01, 0x0100), // L1  -> LB
    (0x02, 0x0200), // R1  -> RB
    (0x10, 0x0020), // share  -> Back
    (0x20, 0x0010), // options -> Start
    (0x40, 0x0040), // L3
    (0x80, 0x0080), // R3
];

const SYSTEM_BUTTONS: [(u8, u16); 2] = [
    (0x01, 0x0400), // PS button -> Guide
    (0x02, 0x0800), // touchpad
];

/// Map a 0..255 stick position onto the -32768..32767 range XInput reports.
fn axis(raw: u8) -> i16 {
    (i32::from(raw) * 257 - 32768) as i16
}

fn motor(amount: f32) -> u8 {
    (amount.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Little-endian `i16` at `offset`, if the packet reaches that far.
fn sample(data: &[u8], offset: usize) -> Option<i16> {
    let pair = data.get(offset..offset + 2)?;
    Some(i16::from_le_bytes([pair[0], pair[1]]))
}

fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    let pair = data.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([pair[0], pair[1]]))
}

/// The Bluetooth CRC: seeded with the header, then run over all but the last four
/// bytes, which is where the checksum itself lives.
fn crc32(data: &[u8], header: u8, size: usize) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&[header]);
    hasher.update(&data[..size - 4]);
    hasher.finalize()
}

fn crc_ok(data: &[u8], header: u8, size: usize) -> bool {
    // The DualShock stores a plain 32 bit CRC over the first size-4 bytes, seeded
    // with the header, with the checksum itself in the last four.
    let Some(tail) = data.get(size - 4..size) else {
        return false;
    };
    let stored = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]);
    stored == crc32(data, header, size)
}

/// Strip a report's framing, handing back the state packet and its CRC verdict.
///
/// USB reports start right after the report id. Bluetooth reports carry two extra
/// header bytes, and only report ids 0x11 to 0x19 when the HID payload is present.
fn state_payload(data: &[u8]) -> Option<(&[u8], bool)> {
    let report_id = *data.first()?;
    if report_id == USB_STATE {
        // A pad on the dongle sends the full 64 byte report; one on USB may send a
        // short 10 byte one, which is the same state without the motion sensors.
        if data.len() != USB_SIMPLE_SIZE
            && (data.len() < USB_EXTENDED_SIZE
                || data[DONGLE_PRESENT_OFFSET] & DONGLE_PRESENT_MASK != 0)
        {
            return None;
        }
        return Some((&data[1..], true));
    }
    if (BT_STATE_FIRST..=BT_STATE_LAST).contains(&report_id) {
        if data.len() < BT_PACKET_SIZE || data[1] & BT_HID_PRESENT == 0 {
            return None;
        }
        return Some((&data[3..], crc_ok(data, STATE_CRC_HEADER, BT_PACKET_SIZE)));
    }
    None
}

fn button_bits(face: u8, shoulders: u8, system: u8) -> u16 {
    let mut bits = HAT_BUTTONS[(face & 0x0F) as usize];
    for (mask, button) in FACE_BUTTONS {
        if face & mask != 0 {
            bits |= button;
        }
    }
    for (mask, button) in SHOULDER_BUTTONS {
        if shoulders & mask != 0 {
            bits |= button;
        }
    }
    for (mask, button) in SYSTEM_BUTTONS {
        if system & mask != 0 {
            bits |= button;
        }
    }
    bits
}

/// Turn a raw HID report into XInput-shaped state plus its IMU counts.
///
/// `None` means the report should be ignored entirely.
pub fn decode_state(data: &[u8]) -> Option<Decoded> {
    let (data, checksum_ok) = state_payload(data)?;
    if data.len() <= RIGHT_TRIGGER {
        return None;
    }

    let shoulders = data[SHOULDERS];
    let mut left = data[LEFT_TRIGGER];
    let mut right = data[RIGHT_TRIGGER];
    // Some fight sticks only ever set the digital trigger bits, never the analog ones.
    if shoulders & 0x04 != 0 && left == 0 {
        left = 255;
    }
    if shoulders & 0x08 != 0 && right == 0 {
        right = 255;
    }

    let raw_gyro = [
        sample(data, GYRO_X).unwrap_or(0),
        sample(data, GYRO_Y).unwrap_or(0),
        sample(data, GYRO_Z).unwrap_or(0),
    ];
    let raw_accel = [
        sample(data, ACCEL_X).unwrap_or(0),
        sample(data, ACCEL_Y).unwrap_or(0),
        sample(data, ACCEL_Z).unwrap_or(0),
    ];

    Some(Decoded {
        state: RawState {
            packet_number: 0,
            buttons: button_bits(data[FACE], shoulders, data[SYSTEM]),
            left_trigger: left,
            right_trigger: right,
            left_x: axis(data[LX]),
            left_y: axis(data[LY]),
            right_x: axis(data[RX]),
            right_y: axis(data[RY]),
        },
        raw_gyro,
        raw_accel,
        timestamp: u16_at(data, TIMESTAMP).unwrap_or(0),
        checksum_ok,
    })
}

/// Read the IMU out of a report and put it through the pad's calibration.
///
/// Without a calibration the axes come back as the raw counts, so a caller can
/// always fall back to the raw fields and get the same numbers.
pub fn decode_motion(data: &[u8], calibration: &Calibration) -> Option<Motion> {
    let decoded = decode_state(data)?;
    Some(Motion {
        gyro: calibration.gyro_per_second(decoded.raw_gyro),
        accel: calibration.accel_g(decoded.raw_accel),
        raw_gyro: decoded.raw_gyro,
        raw_accel: decoded.raw_accel,
        timestamp: decoded.timestamp,
        hardware_calibration: calibration.hardware,
    })
}

/// Fetch the factory calibration packet, retrying while it comes back empty.
///
/// A pad sometimes answers with all zeros right after connecting. Over Bluetooth
/// the USB report has to be read first: that is what switches the pad into the
/// extended mode that carries the sensors.
fn read_calibration_report(device: &mut HidDevice, bluetooth: bool) -> Option<Vec<u8>> {
    for _ in 0..CALIBRATION_ATTEMPTS {
        let data = device.get_feature_report(GYRO_CALIBRATION_USB, CALIBRATION_LENGTH);
        let data = match data {
            Some(data) if data.len() >= CALIBRATION_MIN_LENGTH => data,
            _ => return None,
        };
        let data = if bluetooth {
            match device.get_feature_report(GYRO_CALIBRATION_BT, CALIBRATION_LENGTH) {
                Some(data) if data.len() >= CALIBRATION_MIN_LENGTH => data,
                _ => return None,
            }
        } else {
            data
        };
        if data[1..CALIBRATION_MIN_LENGTH]
            .iter()
            .any(|&byte| byte != 0)
        {
            return Some(data);
        }
        std::thread::sleep(CALIBRATION_RETRY_DELAY);
    }
    None
}

/// Reject a report whose numbers are obviously wrong.
///
/// These are the limits SDL uses: a bias past 1024 counts, or a sensitivity more
/// than half again away from one, means the report is not worth trusting.
fn plausible(axes: &[(f32, f32)]) -> bool {
    axes.iter().all(|&(bias, scale)| {
        bias.abs() <= MAX_PLAUSIBLE_BIAS && (1.0 - scale).abs() <= MAX_PLAUSIBLE_SHIFT
    })
}

/// Turn the pad's calibration report into per-axis bias and scale.
///
/// The arithmetic follows SDL's `SDL_hidapi_ps4.c`, which credits Valve, with one
/// correction: SDL finishes the gyroscope by multiplying in `pi / 180`, which leaves
/// its degrees-per-second figure 180/pi too small. The factory report says the sweep
/// ran at 540 deg/s and read back about 8839 counts, so one count is 0.061 deg/s, and
/// a full int16 comes out at ~2000 deg/s, which is the DualShock's stated +/-2048
/// deg/s range. SDL's extra factor would cap a sweep at 35 deg/s. The accelerometer
/// branch is used as SDL has it, and reads a clean 1 g with the pad flat on a table.
///
/// The plausibility check runs on the pad's own numbers, before units are applied.
pub fn calibration_from_report(data: &[u8], bluetooth: bool) -> Calibration {
    let uncalibrated = Calibration::UNCALIBRATED;
    if data.len() < CALIBRATION_MIN_LENGTH {
        return uncalibrated;
    }

    let biases = [
        sample(data, 1).unwrap_or(0),
        sample(data, 3).unwrap_or(0),
        sample(data, 5).unwrap_or(0),
    ];
    // Over Bluetooth the three axes arrive grouped by sign, not by axis.
    let (plus, minus) = if bluetooth {
        (
            [
                sample(data, 7).unwrap_or(0),
                sample(data, 9).unwrap_or(0),
                sample(data, 11).unwrap_or(0),
            ],
            [
                sample(data, 13).unwrap_or(0),
                sample(data, 15).unwrap_or(0),
                sample(data, 17).unwrap_or(0),
            ],
        )
    } else {
        (
            [
                sample(data, 7).unwrap_or(0),
                sample(data, 11).unwrap_or(0),
                sample(data, 15).unwrap_or(0),
            ],
            [
                sample(data, 9).unwrap_or(0),
                sample(data, 13).unwrap_or(0),
                sample(data, 17).unwrap_or(0),
            ],
        )
    };

    let speed = (f32::from(sample(data, 19).unwrap_or(0))
        + f32::from(sample(data, 21).unwrap_or(0)))
        * GYRO_DENOMINATOR
        / GYRO_NUMERATOR;

    let mut raw_axes: Vec<(f32, f32)> = Vec::new();
    let mut gyro = [AxisCalibration {
        bias: 0.0,
        scale: 1.0,
    }; 3];
    for axis in 0..3 {
        let bias = biases[axis];
        let spread = (i32::from(plus[axis]) - i32::from(bias)).abs()
            + (i32::from(minus[axis]) - i32::from(bias)).abs();
        if spread == 0 {
            return uncalibrated;
        }
        let sensitivity = speed / spread as f32;
        raw_axes.push((f32::from(bias), sensitivity));
        // SDL multiplies the numerator/denominator back out and then adds a
        // pi / 180 on top; see the docstring for why the second part is dropped.
        gyro[axis] = AxisCalibration {
            bias: f32::from(bias),
            scale: sensitivity * GYRO_NUMERATOR / GYRO_DENOMINATOR,
        };
    }

    let mut accel = [AxisCalibration {
        bias: 0.0,
        scale: 1.0,
    }; 3];
    for (axis, slot) in accel.iter_mut().enumerate() {
        let high = i32::from(sample(data, ACCEL_PLUS + axis * 4).unwrap_or(0));
        let low = i32::from(sample(data, ACCEL_MINUS + axis * 4).unwrap_or(0));
        let spread = high - low;
        if spread == 0 {
            return uncalibrated;
        }
        let sensitivity = 2.0 * ACCEL_DENOMINATOR / ACCEL_NUMERATOR / spread as f32;
        // Zero g sits halfway between the two readings the factory sweep took.
        let bias = (high - spread / 2) as f32;
        raw_axes.push((bias, sensitivity));
        *slot = AxisCalibration {
            bias,
            scale: sensitivity * ACCEL_NUMERATOR / ACCEL_DENOMINATOR,
        };
    }

    if !plausible(&raw_axes) {
        return uncalibrated;
    }
    Calibration {
        gyro,
        accel,
        hardware: true,
    }
}

/// Read calibration off an open device.
pub fn read_calibration(device: &mut HidDevice, bluetooth: bool) -> Calibration {
    match read_calibration_report(device, bluetooth) {
        Some(data) => calibration_from_report(&data, bluetooth),
        None => Calibration::UNCALIBRATED,
    }
}

fn effects_payload(left: u8, right: u8, color: [u8; 3]) -> [u8; 7] {
    let [red, green, blue] = color;
    [right, left, red, green, blue, 0x00, 0x00]
}

/// Assemble an output report; rumble and the light bar share one packet.
pub fn build_effects(bluetooth: bool, mask: u8, payload: &[u8]) -> Vec<u8> {
    if !bluetooth {
        let mut data = vec![0u8; USB_EFFECTS_SIZE];
        data[0] = USB_EFFECTS;
        data[1] = mask;
        data[4..4 + payload.len()].copy_from_slice(payload);
        return data;
    }
    let mut data = vec![0u8; BT_PACKET_SIZE];
    data[0] = BT_EFFECTS;
    data[1] = BT_MAGIC | BT_REPORT_INTERVAL;
    data[3] = mask;
    data[6..6 + payload.len()].copy_from_slice(payload);
    let checksum = crc32(&data, BT_CRC_HEADER, BT_PACKET_SIZE);
    data[BT_PACKET_SIZE - 4..].copy_from_slice(&checksum.to_le_bytes());
    data
}

/// An empty Bluetooth output report, used to wake a pad that went quiet.
///
/// The checksum is deliberately left unset: the packet only needs to nudge the
/// Bluetooth stack, and a valid one would switch the pad into enhanced mode.
pub fn build_tickle() -> Vec<u8> {
    let mut data = vec![0u8; BT_PACKET_SIZE];
    data[0] = BT_EFFECTS;
    data[1] = BT_MAGIC;
    data
}

/// Cut a raw read into single reports.
///
/// Windows fills the read buffer with as many whole reports as it can, so a 128 byte
/// read off a DualShock is usually two 64 byte reports.
fn split_reports(data: &[u8], report_length: usize) -> Vec<&[u8]> {
    if report_length == 0 || data.len() < report_length {
        return vec![data];
    }
    data.chunks(report_length).collect()
}

/// One DualShock: the open handle plus the last state decoded from it.
struct Slot {
    info: HidDeviceInfo,
    device: Option<HidDevice>,
    state: Option<RawState>,
    motion: Option<Motion>,
    calibration: Calibration,
    last_packet: Option<Instant>,
    packet_number: u32,
    crc_failures: u32,
    broken: bool,
}

impl Slot {
    fn new(info: HidDeviceInfo) -> Self {
        Self {
            info,
            device: None,
            state: None,
            motion: None,
            calibration: Calibration::UNCALIBRATED,
            last_packet: None,
            packet_number: 0,
            crc_failures: 0,
            broken: false,
        }
    }

    fn bluetooth(&self) -> bool {
        self.info.is_bluetooth
    }

    fn name(&self) -> String {
        if self.info.product_string.is_empty() {
            product_name(self.info.product_id).to_string()
        } else {
            self.info.product_string.clone()
        }
    }

    fn open(&mut self) -> Result<()> {
        if self.device.is_some() {
            return Ok(());
        }
        let mut device = HidDevice::new(&self.info)?;
        // Reading the feature report blocks, so it happens once here rather than on
        // the reader thread's hot path.
        self.calibration = read_calibration(&mut device, self.bluetooth());
        self.device = Some(device);
        self.broken = false;
        Ok(())
    }

    /// Take the newest report off the wire, or `None` if nothing is waiting.
    fn receive(&mut self) -> Option<RawState> {
        let report_length = self
            .device
            .as_ref()
            .map_or(0, |device| device.input_report_length);

        let data = {
            let device = self.device.as_mut()?;
            // A failed read means the device went away, which is not the same as a
            // device that simply has nothing to say yet.
            match device.read(MAX_REPORT_SIZE, 0) {
                Ok(data) => data,
                Err(_) => {
                    self.broken = true;
                    return None;
                }
            }
        };
        self.broken = false;
        let data = data.filter(|data| !data.is_empty())?;

        let mut newest = None;
        for report in split_reports(&data, report_length) {
            if debug_enabled() {
                let hex: Vec<String> = report.iter().map(|b| format!("{b:02x}")).collect();
                println!("[{}] {:>3} {}", self.name(), report.len(), hex.join(" "));
            }
            let Some(decoded) = decode_state(report) else {
                continue;
            };
            if decoded.checksum_ok {
                self.crc_failures = 0;
            } else {
                self.crc_failures += 1;
                if self.crc_failures > CRC_FAILURES_ALLOWED {
                    continue;
                }
            }
            self.motion = Some(Motion {
                gyro: self.calibration.gyro_per_second(decoded.raw_gyro),
                accel: self.calibration.accel_g(decoded.raw_accel),
                raw_gyro: decoded.raw_gyro,
                raw_accel: decoded.raw_accel,
                timestamp: decoded.timestamp,
                hardware_calibration: self.calibration.hardware,
            });
            newest = Some(decoded.state);
        }

        let state = newest?;
        self.packet_number = self.packet_number.wrapping_add(1);
        self.state = Some(RawState {
            packet_number: self.packet_number,
            ..state
        });
        self.last_packet = Some(Instant::now());
        self.state
    }

    /// False once the handle has failed or a pad has gone quiet for too long.
    ///
    /// A USB pad reports continuously, so only a failed read means it is gone. A
    /// Bluetooth pad can fall silent, so it also has a deadline.
    fn alive(&self) -> bool {
        if self.state.is_none() || self.broken {
            return false;
        }
        if !self.bluetooth() {
            return true;
        }
        self.last_packet
            .is_some_and(|last| last.elapsed().as_secs_f64() < BLUETOOTH_LOST_SECONDS)
    }

    fn send(&mut self, packet: &[u8]) -> bool {
        match self.device.as_mut() {
            Some(device) => device.write(packet).is_ok(),
            None => false,
        }
    }

    fn tickle(&mut self) {
        if self.device.as_ref().is_some_and(HidDevice::is_writable) {
            self.send(&build_tickle());
        }
    }

    fn close(&mut self) {
        if let Some(mut device) = self.device.take() {
            device.close();
        }
        self.state = None;
        self.motion = None;
    }
}

/// Reads every attached DualShock over HID and reports XInput-shaped state.
///
/// One background thread rescans on a timer and drains each pad with non-blocking
/// reads, so hotplug, Bluetooth sleep and calls that would otherwise block never
/// stall the caller.
pub struct PS4 {
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct Inner {
    slots: Vec<Slot>,
    scan_interval: Duration,
    poll_interval: Duration,
    /// When the next device enumeration is due. Enumeration opens every HID device
    /// on the machine, so it cannot run at the poll rate.
    next_scan: Option<Instant>,
}

impl PS4 {
    /// Open every attached DualShock and start the reader thread.
    ///
    /// # Errors
    /// If Win32 HID itself is unavailable, as opposed to merely having no pads.
    pub fn new() -> Result<Self> {
        Self::with_intervals(Duration::from_secs(2), Duration::from_millis(2))
    }

    /// [`PS4::new`] with explicit rescan and poll rates.
    ///
    /// # Errors
    /// As [`PS4::new`].
    pub fn with_intervals(scan_interval: Duration, poll_interval: Duration) -> Result<Self> {
        let inner = Arc::new(Mutex::new(Inner {
            slots: Vec::new(),
            scan_interval,
            poll_interval,
            next_scan: None,
        }));
        rescan(&inner, true).map_err(|err| anyhow::anyhow!("Win32 HID is unavailable: {err}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let worker = Arc::clone(&inner);
        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("controllerindicator-ps4".into())
            .spawn(move || {
                let poll_interval = worker
                    .lock()
                    .map(|inner| inner.poll_interval)
                    .unwrap_or(Duration::from_millis(2));
                while !worker_stop.load(Ordering::Relaxed) {
                    if let Err(err) = rescan(&worker, false) {
                        eprintln!("DualShock rescan failed: {err}");
                    }
                    drain(&worker);
                    std::thread::sleep(poll_interval);
                }
            })
            .context("could not start the DualShock reader thread")?;

        Ok(Self {
            inner,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for PS4 {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        for slot in &mut inner.slots {
            slot.close();
        }
    }
}

impl GamepadBackend for PS4 {
    fn max_index(&self) -> u8 {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner.slots.len().saturating_sub(1) as u8
    }

    fn get_state(&self, index: u8) -> Result<Option<RawState>> {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        Ok(inner
            .slots
            .get(index as usize)
            .filter(|slot| slot.alive())
            .and_then(|slot| slot.state))
    }

    fn set_vibration(&self, index: u8, left: f32, right: f32) -> Result<bool> {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let Some(slot) = inner.slots.get_mut(index as usize) else {
            return Ok(false);
        };
        let bluetooth = slot.bluetooth();
        let color = PLAYER_COLORS[index as usize % PLAYER_COLORS.len()];
        let payload = effects_payload(motor(left), motor(right), color);
        Ok(slot.send(&build_effects(
            bluetooth,
            EFFECT_RUMBLE | EFFECT_LED,
            &payload,
        )))
    }

    fn connected_indices(&self) -> Result<Vec<u8>> {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        Ok(inner
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.alive())
            .map(|(index, _)| index as u8)
            .collect())
    }

    fn supports_motion(&self) -> bool {
        true
    }

    fn motion(&self, index: u8) -> Option<Motion> {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let slot = inner.slots.get(index as usize)?;
        if !slot.alive() {
            return None;
        }
        slot.motion
    }

    fn calibration(&self, index: u8) -> Option<Calibration> {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        Some(inner.slots.get(index as usize)?.calibration)
    }

    fn name(&self, index: u8) -> Option<String> {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        Some(inner.slots.get(index as usize)?.name())
    }
}

/// Bring the slot list in line with what is plugged in right now, and open anything
/// new. Runs on the reader thread, on a timer.
///
/// `force` skips the timer, which is what the constructor needs so the first slot
/// list is real rather than empty.
fn rescan(inner: &Arc<Mutex<Inner>>, force: bool) -> Result<()> {
    {
        let mut guard = inner.lock().unwrap_or_else(|err| err.into_inner());
        if !force {
            let now = Instant::now();
            if guard.next_scan.is_some_and(|next| now < next) {
                return Ok(());
            }
            guard.next_scan = Some(now + guard.scan_interval);
        }
    }

    let found = enumerate_hid_devices(Some(SONY_VENDOR_ID), Some(&DS4_PRODUCT_IDS))?;

    let mut guard = inner.lock().unwrap_or_else(|err| err.into_inner());
    // Dropping a slot closes its handle through `Slot::drop`'s `HidDevice`.
    guard
        .slots
        .retain(|slot| found.iter().any(|info| info.path == slot.info.path));
    for info in found {
        if !guard.slots.iter().any(|slot| slot.info.path == info.path) {
            guard.slots.push(Slot::new(info));
        }
    }

    // Opening stays under the lock: `HidDevice::new` blocks on the calibration
    // report, and letting the lock go would show a caller a slot with no state.
    for slot in &mut guard.slots {
        if let Err(err) = slot.open() {
            // A pad we cannot open stays in the list but stays dead, so the next
            // scan can try again once whatever is holding it lets go.
            eprintln!("{}: {err}", slot.name());
        }
    }
    Ok(())
}

/// Drain every pad once, tidying up after any that have gone away.
fn drain(inner: &Arc<Mutex<Inner>>) {
    let mut guard = inner.lock().unwrap_or_else(|err| err.into_inner());
    for slot in &mut guard.slots {
        slot.receive();
        if slot.broken {
            // Drop the handle so the next rescan can pick the pad back up.
            slot.close();
        } else if slot.bluetooth()
            && slot.device.is_some()
            && slot
                .last_packet
                .is_none_or(|last| last.elapsed().as_secs_f64() > BLUETOOTH_QUIET_SECONDS)
        {
            slot.tickle();
        }
    }
}

/// Build a USB state report for tests: the report id, then the payload.
///
/// The pad's report is 64 bytes including the id, and byte 31 being clear means it
/// is plugged into the dongle rather than straight into USB.
#[cfg(test)]
fn usb_state_report(payload: &[u8]) -> Vec<u8> {
    let mut report = vec![0u8; USB_EXTENDED_SIZE];
    report[0] = USB_STATE;
    let end = 1 + payload.len().min(USB_EXTENDED_SIZE - 1);
    report[1..end].copy_from_slice(&payload[..end - 1]);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamepad::Button;

    /// A 63 byte state payload with a centred hat and the sensors zeroed.
    ///
    /// A hat value of 0 genuinely means "pointing up" in this protocol; 8 is the
    /// centred value, so the base payload has to set it.
    fn payload() -> Vec<u8> {
        let mut payload = vec![0u8; USB_EXTENDED_SIZE - 1];
        payload[FACE] = 0x08;
        payload
    }

    fn usb(report_payload: &[u8]) -> Vec<u8> {
        usb_state_report(report_payload)
    }

    #[test]
    fn a_centred_usb_report_reads_as_neutral() {
        let decoded = decode_state(&usb(&payload())).expect("a state report");
        assert_eq!(decoded.state.buttons, 0);
        assert_eq!(decoded.state.left_trigger, 0);
        assert_eq!(decoded.state.left_x, -32768);
        assert!(decoded.checksum_ok, "USB reports carry no checksum");
    }

    #[test]
    fn an_empty_report_is_ignored() {
        assert!(decode_state(&[]).is_none());
    }

    #[test]
    fn a_hat_of_zero_really_is_up() {
        // Worth pinning down: an all-zero face byte is a hat pointing up, not a
        // centred hat. Centred is 8.
        let state = vec![0u8; USB_EXTENDED_SIZE - 1];
        let decoded = decode_state(&usb(&state)).expect("state");
        assert_eq!(decoded.state.buttons, Button::DPAD_UP.bits());
    }

    #[test]
    fn a_short_usb_report_still_decodes() {
        // A pad on plain USB may send the 10 byte form, which has no sensors.
        let mut short = vec![0u8; USB_SIMPLE_SIZE];
        short[0] = USB_STATE;
        short[1 + FACE] = 0x08;
        short[1 + LY] = 255; // stick up
        let decoded = decode_state(&short).expect("a short state report");
        assert_eq!(decoded.state.left_y, 32767);
    }

    #[test]
    fn a_dongle_report_is_ignored() {
        // The bit at byte 31 is what marks a report as coming from the wireless
        // dongle rather than a pad, and those are not pads.
        let mut report = vec![0u8; USB_EXTENDED_SIZE];
        report[0] = USB_STATE;
        report[DONGLE_PRESENT_OFFSET] = DONGLE_PRESENT_MASK;
        assert!(decode_state(&report).is_none());
    }

    #[test]
    fn a_report_id_outside_the_range_is_ignored() {
        for id in [0x00, 0x02, 0x10, 0x1A, 0xFF] {
            let mut report = vec![0u8; USB_EXTENDED_SIZE];
            report[0] = id;
            assert!(decode_state(&report).is_none(), "report id {id:#04x}");
        }
    }

    #[test]
    fn face_buttons_map_onto_the_xbox_layout_by_position() {
        let mut state = payload();
        state[FACE] = 0x10; // square
        let decoded = decode_state(&usb(&state)).expect("state");
        assert!(decoded.state.buttons & Button::X.bits() != 0, "square is X");

        state[FACE] = 0x20; // cross
        let decoded = decode_state(&usb(&state)).expect("state");
        assert!(decoded.state.buttons & Button::A.bits() != 0, "cross is A");

        state[FACE] = 0x40; // circle
        let decoded = decode_state(&usb(&state)).expect("state");
        assert!(decoded.state.buttons & Button::B.bits() != 0, "circle is B");

        state[FACE] = 0x80; // triangle
        let decoded = decode_state(&usb(&state)).expect("state");
        assert!(
            decoded.state.buttons & Button::Y.bits() != 0,
            "triangle is Y"
        );
    }

    #[test]
    fn the_touchpad_is_its_own_button() {
        let mut state = payload();
        state[SYSTEM] = 0x02;
        let decoded = decode_state(&usb(&state)).expect("state");
        assert!(decoded.state.buttons & Button::TOUCHPAD.bits() != 0);
        assert_eq!(decoded.state.buttons & Button::GUIDE.bits(), 0);
    }

    #[test]
    fn share_and_options_are_back_and_start() {
        let mut state = payload();
        state[SHOULDERS] = 0x10;
        let decoded = decode_state(&usb(&state)).expect("state");
        assert!(decoded.state.buttons & Button::BACK.bits() != 0);
        assert!(decoded.state.buttons & Button::START.bits() == 0);

        state[SHOULDERS] = 0x20;
        let decoded = decode_state(&usb(&state)).expect("state");
        assert!(decoded.state.buttons & Button::START.bits() != 0);
    }

    #[test]
    fn the_hat_reports_diagonals() {
        for (hat, expected) in [
            (0x0u8, Button::DPAD_UP),
            (0x1, Button::DPAD_UP | Button::DPAD_RIGHT),
            (0x5, Button::DPAD_DOWN | Button::DPAD_LEFT),
            (0x7, Button::DPAD_UP | Button::DPAD_LEFT),
        ] {
            let mut state = payload();
            state[FACE] = hat;
            let decoded = decode_state(&usb(&state)).expect("state");
            assert_eq!(decoded.state.buttons, expected.bits(), "hat {hat:#x}");
        }
    }

    #[test]
    fn an_unused_hat_position_presses_nothing() {
        for hat in 0x8u8..=0xF {
            let mut state = payload();
            state[FACE] = hat;
            let decoded = decode_state(&usb(&state)).expect("state");
            assert_eq!(decoded.state.buttons, 0, "hat {hat:#x}");
        }
    }

    #[test]
    fn the_stick_range_spans_the_whole_axis() {
        let mut state = payload();
        state[LX] = 0;
        assert_eq!(decode_state(&usb(&state)).unwrap().state.left_x, -32768);
        state[LX] = 255;
        assert_eq!(decode_state(&usb(&state)).unwrap().state.left_x, 32767);
        state[LX] = 128;
        assert_eq!(decode_state(&usb(&state)).unwrap().state.left_x, 128);
    }

    #[test]
    fn a_digital_only_trigger_still_reads_as_pulled() {
        // Some fight sticks set bit 0x04/0x08 and leave the analog value at zero.
        let mut state = payload();
        state[SHOULDERS] = 0x04;
        assert_eq!(decode_state(&usb(&state)).unwrap().state.left_trigger, 255);
        state[SHOULDERS] = 0x08;
        assert_eq!(decode_state(&usb(&state)).unwrap().state.right_trigger, 255);
    }

    #[test]
    fn an_analog_trigger_wins_over_the_digital_bit() {
        let mut state = payload();
        state[SHOULDERS] = 0x04;
        state[LEFT_TRIGGER] = 100;
        assert_eq!(decode_state(&usb(&state)).unwrap().state.left_trigger, 100);
    }

    #[test]
    fn the_imu_counts_are_read_little_endian() {
        let mut state = payload();
        state[GYRO_X..GYRO_X + 2].copy_from_slice(&(-100i16).to_le_bytes());
        state[ACCEL_Z..ACCEL_Z + 2].copy_from_slice(&8192i16.to_le_bytes());
        let decoded = decode_state(&usb(&state)).expect("state");
        assert_eq!(decoded.raw_gyro[0], -100);
        assert_eq!(decoded.raw_accel[2], 8192);
    }

    #[test]
    fn the_timestamp_is_read() {
        let mut state = payload();
        state[TIMESTAMP..TIMESTAMP + 2].copy_from_slice(&1234u16.to_le_bytes());
        assert_eq!(decode_state(&usb(&state)).unwrap().timestamp, 1234);
    }

    #[test]
    fn uncalibrated_motion_is_the_raw_counts() {
        let mut state = payload();
        state[ACCEL_X..ACCEL_X + 2].copy_from_slice(&4096i16.to_le_bytes());
        let sample = decode_motion(&usb(&state), &Calibration::UNCALIBRATED).expect("motion");
        assert_eq!(sample.accel[0], 4096.0);
        assert_eq!(sample.raw_accel[0], 4096);
        assert!(!sample.hardware_calibration);
    }

    #[test]
    fn calibration_scales_the_counts() {
        let calibration = Calibration {
            hardware: true,
            gyro: [AxisCalibration {
                bias: 0.0,
                scale: 0.061,
            }; 3],
            accel: [AxisCalibration {
                bias: 0.0,
                scale: 1.0 / 8192.0,
            }; 3],
        };
        let mut state = payload();
        state[GYRO_X..GYRO_X + 2].copy_from_slice(&1000i16.to_le_bytes());
        state[ACCEL_X..ACCEL_X + 2].copy_from_slice(&8192i16.to_le_bytes());
        let sample = decode_motion(&usb(&state), &calibration).expect("motion");
        assert!(
            (sample.gyro[0] - 61.0).abs() < 0.01,
            "got {}",
            sample.gyro[0]
        );
        assert!((sample.accel[0] - 1.0).abs() < 1e-6);
        assert!(sample.hardware_calibration);
    }

    #[test]
    fn a_short_report_has_no_motion() {
        let mut short = vec![0u8; USB_SIMPLE_SIZE];
        short[0] = USB_STATE;
        short[1 + FACE] = 0x08;
        assert!(decode_state(&short).is_some());
        // The 10 byte form stops before the sensors, so the counts come back zero.
        let sample = decode_motion(&short, &Calibration::UNCALIBRATED).expect("motion");
        assert_eq!(sample.raw_accel, [0, 0, 0]);
    }

    // ---- calibration ----

    /// A calibration report laid out the way the pad sends it over USB.
    fn calibration_report() -> Vec<u8> {
        let mut data = vec![0u8; CALIBRATION_LENGTH];
        // Biases at 1, 3, 5.
        for (offset, value) in [(1usize, 0i16), (3, 0), (5, 0)] {
            data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        // High readings at 7, 11, 15; low at 9, 13, 17.
        for (offset, value) in [(7usize, 8000i16), (11, 8000), (15, 8000)] {
            data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        for (offset, value) in [(9usize, -8000i16), (13, -8000), (17, -8000)] {
            data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        // The sweep speed: two halves of 540 deg/s, as SDL splits it.
        data[19..21].copy_from_slice(&270i16.to_le_bytes());
        data[21..23].copy_from_slice(&270i16.to_le_bytes());
        // Accelerometer high/low pairs, four bytes apart.
        for (offset, value) in [
            (ACCEL_PLUS, 8000i16),
            (ACCEL_PLUS + 4, 8000),
            (ACCEL_PLUS + 8, 8000),
        ] {
            data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        for (offset, value) in [
            (ACCEL_MINUS, -8000i16),
            (ACCEL_MINUS + 4, -8000),
            (ACCEL_MINUS + 8, -8000),
        ] {
            data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        data
    }

    #[test]
    fn a_good_calibration_report_is_accepted() {
        let calibration = calibration_from_report(&calibration_report(), false);
        assert!(calibration.hardware, "report should be usable");
    }

    #[test]
    fn the_gyro_scale_comes_out_in_degrees_per_second() {
        let calibration = calibration_from_report(&calibration_report(), false);
        // A full-scale sweep was 540 deg/s over a spread of 16000 counts.
        let full = calibration.gyro[0].scale * 32767.0;
        assert!(
            (full - 32767.0 * 540.0 / 16000.0).abs() < 1.0,
            "full scale came out at {full} deg/s"
        );
        // SDL's extra pi/180 would have capped this at about 35 deg/s.
        assert!(full > 1000.0, "full scale should be in the thousands");
    }

    #[test]
    fn the_accelerometer_scale_is_counts_over_8192() {
        let calibration = calibration_from_report(&calibration_report(), false);
        // The arithmetic is
        //   sensitivity = 2 * 8192 / spread
        //   scale       = sensitivity / 8192
        // which is counts per 8192, i.e. plain g. The report sweeps +8000 to -8000
        // counts, so the spread is 16000 and one count is about 0.000123.
        //
        // The Python original multiplied this by STANDARD_GRAVITY, which turned g
        // into m/s^2. Measured on a pad lying flat that read +9.75 where g wants
        // 1.0, and it made the gravity dial peg to the rim at any real tilt.
        let expected = 2.0 * 8192.0 / 16000.0 / 8192.0;
        assert!(
            (calibration.accel[0].scale - expected).abs() < 1e-9,
            "got {}, wanted {expected}",
            calibration.accel[0].scale
        );
    }

    #[test]
    fn a_flat_pad_reads_one_g_on_its_face_normal() {
        // The whole reason the scale is plain g: with the bias taken out, a pad
        // flat on a table has to read about 1.0 on the axis along its face normal
        // and about 0.0 on the two in the plane of the pad.
        let mut report = calibration_report();
        // Biases taken from a real DS4 v1 factory report.
        for (offset, value) in [
            (ACCEL_PLUS, 7681i16),
            (ACCEL_PLUS + 4, 8136),
            (ACCEL_PLUS + 8, 7680),
        ] {
            report[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        for (offset, value) in [
            (ACCEL_MINUS, -8275i16),
            (ACCEL_MINUS + 4, -8220),
            (ACCEL_MINUS + 8, -8304),
        ] {
            report[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        let calibration = calibration_from_report(&report, false);
        assert!(calibration.hardware, "report should be usable");

        // A pad lying flat: 1 g along the face normal (Y), 0 g in plane (X and Z).
        let flat = [0.0f32, 1.0, 0.0];
        for (axis, &wanted) in flat.iter().enumerate() {
            let value = calibration.accel[axis].apply((wanted * 8192.0) as i16);
            assert!(
                (value - wanted).abs() < 0.05,
                "axis {axis} read {value} g, wanted {wanted} g"
            );
        }
    }

    #[test]
    fn the_accelerometer_bias_is_the_midpoint() {
        let calibration = calibration_from_report(&calibration_report(), false);
        // High 8000, low -8000, so the midpoint and the zero-g point are 0.
        assert_eq!(calibration.accel[0].bias, 0.0);
    }

    #[test]
    fn an_all_zero_report_is_rejected() {
        let data = vec![0u8; CALIBRATION_LENGTH];
        assert!(!calibration_from_report(&data, false).hardware);
    }

    #[test]
    fn an_implausible_bias_is_rejected() {
        let mut data = calibration_report();
        // A bias far past the 1024 count limit.
        data[1..3].copy_from_slice(&(-30_000i16).to_le_bytes());
        assert!(!calibration_from_report(&data, false).hardware);
    }

    #[test]
    fn a_flat_accelerometer_axis_is_rejected() {
        let mut data = calibration_report();
        // High equal to low means a zero spread, which is not a real measurement.
        data[ACCEL_PLUS..ACCEL_PLUS + 2].copy_from_slice(&(-8000i16).to_le_bytes());
        assert!(!calibration_from_report(&data, false).hardware);
    }

    #[test]
    fn a_truncated_report_is_rejected() {
        let data = vec![0u8; CALIBRATION_MIN_LENGTH - 1];
        assert!(!calibration_from_report(&data, false).hardware);
    }

    #[test]
    fn bluetooth_reads_its_axes_grouped_by_sign() {
        let data = calibration_report();
        let usb_calibration = calibration_from_report(&data, false);
        assert!(usb_calibration.hardware);

        // Over Bluetooth the same three axes sit at 7/9/11 and 13/15/17, so a
        // report laid out that way must also be accepted.
        let mut bt = vec![0u8; CALIBRATION_LENGTH];
        for (offset, value) in [
            (1usize, 0i16),
            (3, 0),
            (5, 0),
            (7, 8000),
            (9, -8000),
            (11, 8000),
            (13, -8000),
            (15, 8000),
            (17, -8000),
        ] {
            bt[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        bt[19..21].copy_from_slice(&270i16.to_le_bytes());
        bt[21..23].copy_from_slice(&270i16.to_le_bytes());
        for (offset, value) in [
            (ACCEL_PLUS, 8000i16),
            (ACCEL_PLUS + 4, 8000),
            (ACCEL_PLUS + 8, 8000),
        ] {
            bt[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        for (offset, value) in [
            (ACCEL_MINUS, -8000i16),
            (ACCEL_MINUS + 4, -8000),
            (ACCEL_MINUS + 8, -8000),
        ] {
            bt[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        let bt_calibration = calibration_from_report(&bt, true);
        assert!(bt_calibration.hardware);
    }

    // ---- framing ----

    #[test]
    fn a_bluetooth_packet_without_the_hid_flag_is_ignored() {
        let mut report = vec![0u8; BT_PACKET_SIZE];
        report[0] = 0x11;
        report[1] = 0x00; // no HID payload
        assert!(decode_state(&report).is_none());
    }

    #[test]
    fn a_truncated_bluetooth_packet_is_ignored() {
        let mut report = vec![0u8; BT_PACKET_SIZE - 1];
        report[0] = 0x11;
        report[1] = BT_HID_PRESENT;
        assert!(decode_state(&report).is_none());
    }

    #[test]
    fn a_good_bluetooth_checksum_passes() {
        let mut report = vec![0u8; BT_PACKET_SIZE];
        report[0] = 0x11;
        report[1] = BT_HID_PRESENT;
        let checksum = crc32(&report, STATE_CRC_HEADER, BT_PACKET_SIZE);
        report[BT_PACKET_SIZE - 4..].copy_from_slice(&checksum.to_le_bytes());
        let decoded = decode_state(&report).expect("a bluetooth state report");
        assert!(decoded.checksum_ok);
    }

    #[test]
    fn a_corrupt_bluetooth_checksum_is_flagged() {
        let mut report = vec![0u8; BT_PACKET_SIZE];
        report[0] = 0x11;
        report[1] = BT_HID_PRESENT;
        let checksum = crc32(&report, STATE_CRC_HEADER, BT_PACKET_SIZE);
        report[BT_PACKET_SIZE - 4..].copy_from_slice(&checksum.to_le_bytes());
        // Corrupt a payload byte without touching the checksum.
        report[10] ^= 0xFF;
        let decoded = decode_state(&report).expect("a bluetooth state report");
        assert!(!decoded.checksum_ok);
    }
    #[test]
    fn a_buffer_holding_two_reports_is_split() {
        let single = usb(&payload());
        let mut both = single.clone();
        both.extend_from_slice(&single);
        let parts = split_reports(&both, single.len());
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], &single[..]);
        assert_eq!(parts[1], &single[..]);
    }

    #[test]
    fn an_unknown_report_length_leaves_the_buffer_alone() {
        let single = usb(&payload());
        assert_eq!(split_reports(&single, 0).len(), 1);
        assert_eq!(split_reports(&single, single.len() * 2).len(), 1);
    }

    // ---- output reports ----

    #[test]
    fn a_usb_effects_report_is_the_right_size() {
        let payload = effects_payload(128, 255, [0x01, 0x02, 0x03]);
        let report = build_effects(false, EFFECT_RUMBLE | EFFECT_LED, &payload);
        assert_eq!(report.len(), USB_EFFECTS_SIZE);
        assert_eq!(report[0], USB_EFFECTS);
        assert_eq!(report[1], EFFECT_RUMBLE | EFFECT_LED);
        assert_eq!(&report[4..11], &payload[..]);
    }

    #[test]
    fn a_bluetooth_effects_report_carries_a_valid_checksum() {
        let payload = effects_payload(128, 255, [0x01, 0x02, 0x03]);
        let report = build_effects(true, EFFECT_RUMBLE | EFFECT_LED, &payload);
        assert_eq!(report.len(), BT_PACKET_SIZE);
        assert_eq!(report[0], BT_EFFECTS);
        assert_eq!(report[1], BT_MAGIC | BT_REPORT_INTERVAL);
        assert!(crc_ok(&report, BT_CRC_HEADER, BT_PACKET_SIZE));
    }

    #[test]
    fn the_tickle_packet_leaves_its_checksum_unset() {
        let report = build_tickle();
        assert_eq!(report.len(), BT_PACKET_SIZE);
        assert_eq!(report[0], BT_EFFECTS);
        assert_eq!(report[1], BT_MAGIC);
        assert!(
            !crc_ok(&report, BT_CRC_HEADER, BT_PACKET_SIZE),
            "a valid checksum would switch the pad into enhanced mode"
        );
    }

    #[test]
    fn motor_clamps_and_rounds() {
        assert_eq!(motor(0.0), 0);
        assert_eq!(motor(1.0), 255);
        assert_eq!(motor(2.0), 255);
        assert_eq!(motor(-1.0), 0);
    }

    #[test]
    fn product_names_cover_every_supported_pad() {
        assert_eq!(product_name(0x05C4), "DualShock 4");
        assert_eq!(product_name(0x09CC), "DualShock 4 v3");
        assert_eq!(product_name(0x0BA0), "DualShock 4 Edge");
        // An unknown pad still gets a usable name rather than nothing.
        assert_eq!(product_name(0x1234), "DualShock 4");
    }
}
