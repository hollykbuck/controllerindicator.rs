# controllerindicator

Watch what a game controller is doing, on Windows. Prints button presses, stick and
trigger movement, and a live event stream; or draws it all in a small always-handy
window.

Works with Xbox One, Series and 360 pads through XInput, and with DualShock 4 pads
read straight from HID. No gamepad libraries — everything goes through the Win32
APIs via the `windows` crate.

A Rust port of the Python `xboxindicator`, which did the same thing with `ctypes`.
The layering, the output format and the window layout are carried over unchanged.

```
cargo run --release -- -w
```

## Build

Requires Rust 1.82 or newer and Windows.

```
cargo build --release
cargo run -- -h
```

The binary lands in `target\release\controllerindicator.exe`.

## Usage

```
cargo run --                       # stream events to the terminal
cargo run -- -w                    # open the indicator window
cargo run -- --list                # which slots hold a controller
cargo run -- --once                # one snapshot, then exit
cargo run -- --rumble 2            # test the motors for two seconds
cargo run -- -b ps4 --motion       # stream gyroscope and accelerometer samples
```

Useful flags:

| Flag | Effect |
| --- | --- |
| `-i N` | which controller slot to watch |
| `-b xinput` / `-b ps4` | where to read input from (default `xinput`) |
| `--deadzone R` | stick deadzone, 0.0-1.0 |
| `--invert-x` / `--invert-y` | flip an axis for pads whose driver reports it the other way up |
| `--poll-hz HZ` | poll rate |
| `--topmost`, `--borderless`, `--click-through`, `--opacity` | window behaviour |

Without a mode flag it streams one line per event:

```
[0] connected
[0] A down
[0] left_x -0.42
[0] LT 0.63
[0] A up
```

## DualShock 4

Sony never shipped a Windows driver for the DualShock 4, so it is invisible to
XInput. Rather than require you to install a translation driver, this reads the pad
over raw Win32 HID and reports it in the same shape as an XInput pad, so every mode
above works unchanged:

```
cargo run -- -b ps4 -w
```

DS4 v1/v2 (USB 05c4), v3 (09cc) and Edge (0ba0) are recognised, over both USB and
Bluetooth. Multiple pads plugged in at once each get their own slot index.

Notes on the face buttons: the pad's symbols are mapped to the Xbox layout they
correspond to positionally, so **X** is the square, **A** is the cross, **B** is the
circle and **Y** is the triangle. The touchpad reports as its own `Touchpad` button.
Share is `Back` and Options is `Start`.

### Motion sensors

DualShock 4 pads have a gyroscope and accelerometer, and both are surfaced. The
window grows a panel with a gravity dial and live axis values when a pad is present:

```
accel  +0.02  +9.75  -0.02 g
gyro   +0.0   +12.3  +88.1 deg/s
```

`--motion` prints the same thing as a stream, plus the calibration the pad's factory
report gave us:

```
factory calibration applied; ctrl+c to quit
  gyro pitch      bias      1.0  0.061100 deg/s per count
  gyro yaw   bias      0.0  0.060773 deg/s per count
  gyro roll  bias      0.0  0.060722 deg/s per count
  accel x    bias   -297.5  0.001210 g per count
  accel y    bias    -42.0  0.001215 g per count
  accel z    bias   -512.0  0.001227 g per count
```

Calibration comes from the pad's own feature report, read once when the device is
opened. If a pad does not supply a usable one the values fall back to raw counts and
say so.

The gravity dial leans toward whichever edge is lowest. An accelerometer measures
specific force, which points *up*, so the display negates it. A pad lying flat reads
about 1.0 g along its face normal and 0.0 in the plane of its face.

## Architecture

```
main.rs       process entry point
cli.rs        clap definitions and the mode dispatch
gamepad.rs    buttons, normalised axes, deadzone; the GamepadBackend trait
xinput.rs     XInputGetState / XInputSetState
hid.rs        Win32 HID enumeration and overlapped IO
ps4.rs        DualShock 4 report parsing, calibration and the reader thread
listener.rs   turns a stream of polled states into discrete events
window.rs     a Win32 window that renders state with plain GDI
```

`Gamepad` talks to a `GamepadBackend`, which is `get_state`, `set_vibration`,
`connected_indices` and `close`. `XInput` and `PS4` both satisfy it, which is why the
listener and the window needed no changes when DS4 support was added. A backend that
also reports `supports_motion` gets the panel automatically, so a pad without an IMU
simply leaves it out and the window stays the size it was.

### Notes from the port

Four things cost real time, and each is commented where it is implemented:

- **Feature reports do not go through `DeviceIoControl`.** The documented route is
  `IOCTL_HID_GET_FEATURE`, and the current HID stack rejects every encoding of it
  with `ERROR_INVALID_FUNCTION`. `HidD_GetFeature` works, and that is what
  `hid.rs` uses. The DS4's gyro calibration is only reachable this way.
- **A `ReadFile` on a HID handle returns several reports at once**, so the report
  length has to come from `HidP_GetCaps` and the buffer has to be split. `ps4.rs`
  does both.
- **The SDL gyroscope scale is 180/pi too small.** Report parsing follows SDL's
  `SDL_hidapi_ps4.c`, which credits Valve for the Bluetooth work. Its gyroscope
  branch finishes with an extra `pi / 180`, which leaves degrees per second 57.3x
  too small — a full-range sweep would saturate at 35 deg/s. The factory report says
  the sweep ran at 540 deg/s and read back about 8839 counts, so one count is
  0.061 deg/s, and a full int16 comes out at ~2000 deg/s, which is the DS4's stated
  +/-2048 deg/s range. `ps4.rs` drops that factor.
- **Asking a SetupAPI function how much room it needs is answered with an error.**
  `SetupDiGetDeviceInterfaceDetailW` called with a null buffer always fails with
  `ERROR_INSUFFICIENT_BUFFER`; the size it writes back is the real answer. Rust's
  `Result` makes it tempting to `.ok()?` on that call, which silently drops every
  device on the machine and makes the program report "no controller" while the
  hardware is sitting right there.
- **Overlapped IO needs storage that does not move.** The kernel writes through the
  `OVERLAPPED` and the read buffer until the transfer completes, so both live in a
  `Box` for as long as the handle is open.

## Tests

```
cargo test
```

The parts that can be tested without a controller are: axis normalisation and
deadzone, the event differ, HID path decoding, DualShock report framing, button
mapping and calibration arithmetic. Those are covered by unit tests next to the code
they exercise. Anything that needs a pad plugged in — reading a real report, the
window's appearance — has to be checked by hand.

## Licence

GPL-3.0-or-later, following the Python original. Every source file carries an
`SPDX-License-Identifier` header.

```
Copyright (C) 2026 hollykbuck
SPDX-License-Identifier: GPL-3.0-or-later
```

The DualShock 4 protocol work follows SDL's `SDL_hidapi_ps4.c`, which is zlib
licensed and itself credits Valve for the Bluetooth support.
