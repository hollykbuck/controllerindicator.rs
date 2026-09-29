// Copyright (C) 2026 hollykbuck
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! A Win32 window that mirrors controller input, drawn with plain GDI.
//!
//! The layout is written once in logical units and mapped to pixels at paint time,
//! so the whole thing scales with the display's DPI without any of the coordinates
//! having to know about it.
//!
//! The window procedure is a plain `extern "system"` function, which means it
//! cannot capture anything. The state hangs off the `HWND` as a user pointer
//! instead: `WM_NCCREATE` stashes it, and [`IndicatorWindow`] owns the `Box` it
//! points at, so the callback can only ever see a live value.

use std::collections::HashMap;
use std::ffi::c_void;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BeginPaint, BitBlt, CLIP_DEFAULT_PRECIS, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateFontW, CreatePen, CreateSolidBrush, DEFAULT_CHARSET,
    DRAW_TEXT_FORMAT, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC,
    DeleteObject, DrawTextW, Ellipse, EndPaint, FillRect, HBRUSH, HDC, HFONT, HGDIOBJ, HPEN,
    InvalidateRect, LineTo, MoveToEx, OUT_DEFAULT_PRECIS, PAINTSTRUCT, ROP_CODE, RoundRect,
    SRCCOPY, ScreenToClient, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    GetDpiForSystem, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CS_DBLCLKS, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, HTCLIENT, IDC_ARROW, IDC_SIZEALL, KillTimer,
    LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, LoadCursorW, PostQuitMessage, RegisterClassW,
    SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNOACTIVATE, SetCursor, SetLayeredWindowAttributes,
    SetProcessDPIAware, SetTimer, SetWindowLongPtrW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST,
    WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_SYSKEYDOWN, WM_TIMER, WNDCLASS_STYLES, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_OVERLAPPEDWINDOW,
    WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, PWSTR, w};

use crate::gamepad::{Button, Gamepad, GamepadState};

const WM_NCHITTEST_CAPTION: isize = 2;
const VK_ESCAPE: WPARAM = WPARAM(0x1B);

const LOGICAL_WIDTH: f32 = 460.0;
const LOGICAL_HEIGHT: f32 = 256.0;
/// Extra room below the controller drawing, added only when the pad has an IMU.
/// The y component is the gap between the pad drawing and the panel.
const MOTION_PANEL: (f32, f32, f32, f32) = (10.0, 10.0, 440.0, 40.0);
const MOTION_PANEL_MARGIN: f32 = 6.0;
const MOTION_DIAL: (f32, f32, f32) = (18.0, 5.0, 30.0);
const MOTION_LINES: [(f32, f32, f32, f32); 2] =
    [(58.0, 4.0, 374.0, 17.0), (58.0, 21.0, 374.0, 17.0)];

/// Which accelerometer axes feed the dial's horizontal and vertical, and with what
/// sign. Measured on a DualShock 4 held flat, then with each edge dropped in turn:
/// Y carries 1 g face up and -1 g face down, so it is the face normal and the two
/// in-plane axes are X and Z. Dropping the left edge pushed X positive, so +X is
/// the pad's left; dropping the top edge pushed Z positive, so +Z is the pad's
/// bottom.
///
/// Both signs are negative because an accelerometer reads specific force, which
/// points up, not gravity, which points down. Negating it is what makes the dot a
/// gravity ball: it falls toward whichever edge is lowest.
const TILT_HORIZONTAL: usize = 0;
const TILT_VERTICAL: usize = 2;
const TILT_HORIZONTAL_SIGN: f32 = -1.0;
const TILT_VERTICAL_SIGN: f32 = 1.0;

/// Logical height of the header row a borderless window can be dragged by.
const TITLE_STRIP: f32 = 24.0;
const TIMER_ID: usize = 1;
const DPI_BASE: f32 = 96.0;
const MIN_SCALE: f32 = 0.5;

// Colours are GDI COLORREF, which is 0x00BBGGRR.
const BG: u32 = 0x1B1B1B;
const PANEL: u32 = 0x242424;
const PANEL_EDGE: u32 = 0x323232;
const SLOT: u32 = 0x2C2C2C;
const SLOT_OFF: u32 = 0x212121;
const IDLE_EDGE: u32 = 0x3E3E3E;
const IDLE_EDGE_OFF: u32 = 0x2A2A2A;
const ACTIVE: u32 = 0x7ED321;
const ACTIVE_EDGE: u32 = 0xB4F03C;
const STICK_BOX_FILL: u32 = 0x232323;
const STICK_CROSS: u32 = 0x3A3A3A;
const STICK_CROSS_LINE: u32 = 0x333333;
const STICK_DOT: u32 = 0x7ED321;
const TRACK: u32 = 0x1A1A1A;
const TRACK_EDGE: u32 = 0x2E2E2E;
const TEXT: u32 = 0xE6E6E6;
const TEXT_DIM: u32 = 0x8E8E8E;
const TEXT_OFF: u32 = 0x6A6A6A;
const GRIP_ON: u32 = 0x505050;
const GRIP_OFF: u32 = 0x3A3A3A;

// A box in logical units: x, y, width, height.
type Box4 = (f32, f32, f32, f32);

/// A piece of text to draw: what it says, where it goes, how big and in what colour.
struct Label<'a> {
    text: &'a str,
    at: Box4,
    px: i32,
    color: u32,
    align: DRAW_TEXT_FORMAT,
}

impl<'a> Label<'a> {
    fn new(text: &'a str, at: Box4, px: i32, color: u32) -> Self {
        Self {
            text,
            at,
            px,
            color,
            align: DT_LEFT,
        }
    }

    fn centered(mut self) -> Self {
        self.align = DT_CENTER;
        self
    }
}

/// This module's own `HINSTANCE`, needed to register the class and create the window.
fn module_handle() -> HINSTANCE {
    // SAFETY: a null name asks for the current module, which lives as long as the
    // process does.
    HINSTANCE(unsafe { GetModuleHandleW(None) }.map_or(std::ptr::null_mut(), |module| module.0))
}

const STICK_BOX: Box4 = (24.0, 68.0, 90.0, 90.0);
const RIGHT_STICK_BOX: Box4 = (352.0, 68.0, 90.0, 90.0);
const DPAD_BOX: Box4 = (151.0, 68.0, 90.0, 90.0);
const DPAD_CELLS: [(Button, Box4); 4] = [
    (Button::DPAD_UP, (181.0, 68.0, 30.0, 30.0)),
    (Button::DPAD_DOWN, (181.0, 128.0, 30.0, 30.0)),
    (Button::DPAD_LEFT, (151.0, 98.0, 30.0, 30.0)),
    (Button::DPAD_RIGHT, (211.0, 98.0, 30.0, 30.0)),
];
const FACE_BUTTONS: [(Button, (f32, f32)); 4] = [
    (Button::Y, (302.0, 84.0)),
    (Button::X, (273.0, 115.0)),
    (Button::B, (331.0, 115.0)),
    (Button::A, (302.0, 146.0)),
];
const SHOULDERS: [(Button, Box4); 2] = [
    (Button::LEFT_SHOULDER, (24.0, 32.0, 84.0, 22.0)),
    (Button::RIGHT_SHOULDER, (352.0, 32.0, 84.0, 22.0)),
];
const THUMBS: [(Button, Box4); 2] = [
    (Button::LEFT_THUMB, STICK_BOX),
    (Button::RIGHT_THUMB, RIGHT_STICK_BOX),
];
const THUMB_LABELS: [(Button, &str); 2] = [(Button::LEFT_THUMB, "L3"), (Button::RIGHT_THUMB, "R3")];
const TRIGGERS: [(f32, f32, f32, f32); 2] = [(24.0, 178.0, 90.0, 16.0), (352.0, 178.0, 90.0, 16.0)];
const SYSTEM_BUTTONS: [(Button, Box4); 3] = [
    (Button::GUIDE, (206.0, 200.0, 48.0, 24.0)),
    (Button::BACK, (254.0, 200.0, 84.0, 24.0)),
    (Button::START, (346.0, 200.0, 96.0, 24.0)),
];
const SYSTEM_LABELS: [(Button, &str); 3] = [
    (Button::GUIDE, "Guide"),
    (Button::BACK, "Back"),
    (Button::START, "Start"),
];
const SYSTEM_LABEL_BOXES: [(Button, Box4); 3] = [
    (Button::GUIDE, (194.0, 205.0, 48.0, 14.0)),
    (Button::BACK, (254.0, 205.0, 84.0, 14.0)),
    (Button::START, (346.0, 205.0, 96.0, 14.0)),
];

/// A fill colour, an edge colour and a pen width, in logical units' worth of
/// pixels once scaled.
type Colors = (u32, u32, f32);

/// A cached GDI object plus the handle it replaced.
struct Cached<T> {
    objects: HashMap<u32, T>,
}

impl<T> Default for Cached<T> {
    fn default() -> Self {
        Self {
            objects: HashMap::new(),
        }
    }
}

impl<T> Cached<T>
where
    T: Copy + Into<HGDIOBJ>,
{
    fn get_or_insert(&mut self, key: u32, make: impl FnOnce() -> T) -> T {
        *self.objects.entry(key).or_insert_with(make)
    }
}

impl Cached<HBRUSH> {
    fn brush(&mut self, color: u32) -> HBRUSH {
        self.get_or_insert(color, || {
            // SAFETY: a plain colour value, no resource involved.
            unsafe { CreateSolidBrush(COLORREF(color)) }
        })
    }
}

impl Cached<HPEN> {
    /// Pens are keyed by colour and width, so a scaled edge stays a distinct object.
    fn pen(&mut self, color: u32, width: i32) -> HPEN {
        let key = (color << 16) ^ width as u32;
        self.get_or_insert(key, || {
            // SAFETY: a plain style, width and colour.
            unsafe {
                CreatePen(
                    windows::Win32::Graphics::Gdi::PS_SOLID,
                    width.max(1),
                    COLORREF(color),
                )
            }
        })
    }
}

/// Everything the window procedure needs, hung off the `HWND`.
struct WindowState<'a> {
    gamepad: &'a Gamepad<'a>,
    title: Vec<u16>,
    borderless: bool,
    click_through: bool,
    motion_panel: bool,

    hwnd: HWND,
    state: Option<GamepadState>,
    connected: bool,
    scale: f32,
    offset: (f32, f32),

    brushes: Cached<HBRUSH>,
    pens: Cached<HPEN>,
    font: Option<HFONT>,
    font_px: i32,

    memory_dc: Option<HDC>,
    memory_bitmap: Option<windows::Win32::Graphics::Gdi::HBITMAP>,
    memory_old_bitmap: Option<HGDIOBJ>,
    buffer_size: (i32, i32),
}

impl<'a> WindowState<'a> {
    /// The drawing height, which grows by one panel when there is an IMU.
    fn height(&self) -> f32 {
        if !self.motion_panel {
            return LOGICAL_HEIGHT;
        }
        LOGICAL_HEIGHT + MOTION_PANEL.1 + MOTION_PANEL.3 + MOTION_PANEL_MARGIN
    }

    /// The height the scale is fitted to, so the panel never squashes the pad.
    fn content_height(&self) -> f32 {
        if self.motion_panel {
            LOGICAL_HEIGHT
        } else {
            self.height()
        }
    }

    fn poll(&mut self) {
        match self.gamepad.poll() {
            Ok(Some(state)) => {
                self.connected = true;
                self.state = Some(state);
            }
            Ok(None) => self.connected = false,
            Err(err) => {
                eprintln!("error: {err}");
                self.connected = false;
            }
        }
    }

    /// Map a logical box to the pixel rect GDI expects: left, top, right, bottom.
    fn map(&self, x: f32, y: f32, w: f32, h: f32) -> (i32, i32, i32, i32) {
        let left = x * self.scale + self.offset.0;
        let top = y * self.scale + self.offset.1;
        (
            left.round() as i32,
            top.round() as i32,
            (left + w * self.scale).round() as i32,
            (top + h * self.scale).round() as i32,
        )
    }

    fn title_bottom(&self) -> f32 {
        TITLE_STRIP * self.scale + self.offset.1
    }

    fn button_colors(&self, button: Button, fill: u32, edge: u32) -> Colors {
        if !self.connected || self.state.is_none() {
            return (SLOT_OFF, IDLE_EDGE_OFF, 1.0);
        }
        if self.state.is_some_and(|s| s.buttons.contains(button)) {
            return (ACTIVE, ACTIVE_EDGE, self.scale.max(1.0));
        }
        (fill, edge, 1.0)
    }

    // ---- GDI primitives ----

    fn line(&mut self, dc: HDC, from: (f32, f32), to: (f32, f32), color: u32, width: f32) {
        let (ax, ay, _, _) = self.map(from.0, from.1, 0.0, 0.0);
        let (bx, by, _, _) = self.map(to.0, to.1, 0.0, 0.0);
        let pen = self.pens.pen(color, (width * self.scale) as i32);
        // SAFETY: `dc` is the buffer DC and both objects are owned by this state.
        unsafe {
            let old = SelectObject(dc, pen.into());
            let _ = MoveToEx(dc, ax, ay, None);
            let _ = LineTo(dc, bx, by);
            let _ = SelectObject(dc, old);
        }
    }

    fn rounded(&mut self, dc: HDC, (x, y, w, h): Box4, colors: Colors, radius: Option<i32>) {
        let (left, top, right, bottom) = self.map(x, y, w, h);
        let (fill, edge, pen_width) = colors;
        let corner = radius.unwrap_or_else(|| (6.0 * self.scale).round().max(1.0) as i32);
        let brush = self.brushes.brush(fill);
        let pen = self.pens.pen(edge, (pen_width * self.scale) as i32);
        // SAFETY: `dc` is the buffer DC; both objects are restored before return.
        unsafe {
            let old_brush = SelectObject(dc, brush.into());
            let old_pen = SelectObject(dc, pen.into());
            let _ = RoundRect(dc, left, top, right, bottom, corner, corner);
            SelectObject(dc, old_brush);
            SelectObject(dc, old_pen);
        }
    }

    fn circle(&mut self, dc: HDC, cx: f32, cy: f32, r: f32, colors: Colors) {
        let (left, top, right, bottom) = self.map(cx - r, cy - r, r * 2.0, r * 2.0);
        let (fill, edge, pen_width) = colors;
        let brush = self.brushes.brush(fill);
        let pen = self.pens.pen(edge, (pen_width * self.scale) as i32);
        // SAFETY: as `rounded`.
        unsafe {
            let old_brush = SelectObject(dc, brush.into());
            let old_pen = SelectObject(dc, pen.into());
            let _ = Ellipse(dc, left, top, right, bottom);
            SelectObject(dc, old_brush);
            SelectObject(dc, old_pen);
        }
    }

    fn font_handle(&mut self, px: i32) -> HFONT {
        if self.font.is_none() || self.font_px != px {
            if let Some(font) = self.font.take() {
                // SAFETY: created by CreateFontW below, deleted once.
                unsafe {
                    let _ = DeleteObject(font.into());
                }
            }
            // SAFETY: a plain font description; a negative height means "character
            // height in pixels", which is what the layout wants.
            self.font = Some(unsafe {
                CreateFontW(
                    -px,
                    0,
                    0,
                    0,
                    400,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    ANTIALIASED_QUALITY,
                    0,
                    w!("Segoe UI"),
                )
            });
            self.font_px = px;
        }
        self.font.expect("a font was just created")
    }

    fn text(&mut self, dc: HDC, label: Label<'_>) {
        let (x, y, w, h) = label.at;
        let (left, top, right, bottom) = self.map(x, y, w, h);
        let font = self.font_handle(label.px);
        let mut wide: Vec<u16> = label.text.encode_utf16().collect();
        wide.push(0);
        let mut rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        let format = label.align | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX;
        // SAFETY: the string is NUL terminated and `rect` outlives the call.
        unsafe {
            let old = SelectObject(dc, font.into());
            let _ = SetTextColor(dc, COLORREF(label.color));
            let _ = DrawTextW(dc, &mut wide, &raw mut rect, format);
            let _ = SelectObject(dc, old);
        }
    }

    // ---- the drawing ----

    fn draw(&mut self, dc: HDC, width: i32, height: i32) {
        // Fit to whichever axis is tighter, so the pad never gets squashed.
        self.scale = MIN_SCALE
            .max((width as f32 / LOGICAL_WIDTH).min(height as f32 / self.content_height()));
        self.offset = ((width as f32 - LOGICAL_WIDTH * self.scale) / 2.0, 0.0);

        // SAFETY: `dc` is the buffer DC and the brush belongs to this state.
        unsafe {
            let _ = SetBkMode(dc, TRANSPARENT);
        }
        self.fill(dc, (0.0, 0.0, width as f32, height as f32), BG);

        let live = self.connected && self.state.is_some();
        if self.borderless {
            self.grip(dc);
        }

        let title = String::from_utf16_lossy(&self.title);
        let title_color = if live { TEXT } else { TEXT_OFF };
        self.text(
            dc,
            Label::new(&title, (10.0, 4.0, 260.0, 16.0), 13, title_color),
        );
        let status = if live {
            "connected"
        } else {
            "waiting for controller"
        };
        let status_color = if live { ACTIVE } else { TEXT_DIM };
        self.text(
            dc,
            Label::new(status, (300.0, 4.0, 148.0, 16.0), 13, status_color).centered(),
        );

        let edge = if live { IDLE_EDGE } else { IDLE_EDGE_OFF };
        let fill = if live { SLOT } else { SLOT_OFF };

        for (button, (x, y, w, h)) in SHOULDERS {
            let colors = self.button_colors(button, fill, edge);
            self.rounded(dc, (x, y, w, h), colors, None);
        }
        for (button, (x, y, w, h)) in THUMBS {
            let colors = self.button_colors(button, fill, edge);
            self.rounded(dc, (x, y, w, h), colors, None);
        }
        self.rounded(dc, DPAD_BOX, (fill, edge, 1.0), None);
        for (button, (x, y, w, h)) in DPAD_CELLS {
            let colors = self.button_colors(button, fill, edge);
            self.rounded(dc, (x, y, w, h), colors, None);
        }
        for (button, (cx, cy)) in FACE_BUTTONS {
            let colors = self.button_colors(button, fill, edge);
            self.circle(dc, cx, cy, 15.0, colors);
        }
        for (button, (x, y, w, h)) in SYSTEM_BUTTONS {
            let colors = self.button_colors(button, fill, edge);
            if button == Button::GUIDE {
                self.circle(dc, x, y + h / 2.0, h / 2.0, colors);
            } else {
                self.rounded(dc, (x, y, w, h), colors, None);
            }
        }

        let triggers = self.state;
        self.trigger(dc, 0, triggers.as_ref());
        self.trigger(dc, 1, triggers.as_ref());
        self.stick(dc, STICK_BOX, self.stick_position(true), live);
        self.stick(dc, RIGHT_STICK_BOX, self.stick_position(false), live);

        for (button, (x, y, w, h)) in THUMBS {
            let label = THUMB_LABELS
                .iter()
                .find(|(b, _)| *b == button)
                .map(|(_, text)| *text)
                .unwrap_or("");
            let held = self.state.is_some_and(|s| s.buttons.contains(button));
            let color = if held { ACTIVE } else { TEXT_DIM };
            self.text(
                dc,
                Label::new(label, (x + 4.0, y + h - 16.0, w - 8.0, 12.0), 9, color).centered(),
            );
        }
        for (button, (x, y, w, h)) in SYSTEM_LABEL_BOXES {
            let label = SYSTEM_LABELS
                .iter()
                .find(|(b, _)| *b == button)
                .map(|(_, text)| *text)
                .unwrap_or("");
            let held = self.state.is_some_and(|s| s.buttons.contains(button));
            let color = if held { ACTIVE } else { TEXT_DIM };
            self.text(dc, Label::new(label, (x, y, w, h), 9, color).centered());
        }

        let pressed = if live {
            self.state
                .map(|state| {
                    state
                        .pressed_buttons()
                        .map(|b| b.label())
                        .collect::<Vec<_>>()
                        .join("  ")
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        let pressed = if pressed.is_empty() {
            "no buttons".to_string()
        } else {
            pressed
        };
        let pressed_color = if live { TEXT } else { TEXT_OFF };
        self.text(
            dc,
            Label::new(
                &pressed,
                (10.0, LOGICAL_HEIGHT - 26.0, LOGICAL_WIDTH - 20.0, 18.0),
                11,
                pressed_color,
            )
            .centered(),
        );

        if self.motion_panel {
            self.draw_motion(dc);
        }
    }

    fn stick_position(&self, left: bool) -> (f32, f32) {
        match (self.connected, self.state) {
            (true, Some(state)) if left => (state.left_x, state.left_y),
            (true, Some(state)) => (state.right_x, state.right_y),
            _ => (0.0, 0.0),
        }
    }

    fn fill(&mut self, dc: HDC, (x, y, w, h): Box4, color: u32) {
        let (left, top, right, bottom) = self.map(x, y, w, h);
        let brush = self.brushes.brush(color);
        // SAFETY: the rect is on the stack and the brush belongs to this state.
        unsafe {
            let rect = RECT {
                left,
                top,
                right,
                bottom,
            };
            let _ = FillRect(dc, &raw const rect, brush);
        }
    }

    fn trigger(&mut self, dc: HDC, which: usize, state: Option<&GamepadState>) {
        let (x, y, w, h) = TRIGGERS[which];
        let value = state.map_or(0.0, |state| {
            if which == 0 {
                state.left_trigger
            } else {
                state.right_trigger
            }
        });
        let label = if which == 0 { "LT" } else { "RT" };
        self.rounded(dc, (x, y, w, h), (TRACK, TRACK_EDGE, 1.0), None);
        if value > 0.0 {
            self.rounded(
                dc,
                (x, y, (w * value).max(4.0), h),
                (ACTIVE, ACTIVE_EDGE, 1.0),
                None,
            );
        }
        let color = if value > 0.02 { TEXT } else { TEXT_DIM };
        self.text(
            dc,
            Label::new(label, (x + 4.0, y + h / 2.0 - 7.0, w - 8.0, 14.0), 9, color).centered(),
        );
    }

    fn stick(&mut self, dc: HDC, (x, y, w, h): Box4, (px, py): (f32, f32), live: bool) {
        let cx = x + w / 2.0;
        let cy = y + h / 2.0;
        self.rounded(dc, (x, y, w, h), (STICK_BOX_FILL, STICK_CROSS, 1.0), None);
        self.line(dc, (cx, y + 6.0), (cx, y + h - 6.0), STICK_CROSS_LINE, 1.0);
        self.line(dc, (x + 6.0, cy), (x + w - 6.0, cy), STICK_CROSS_LINE, 1.0);
        if live {
            self.circle(
                dc,
                cx + px * (w / 2.0 - 10.0),
                cy - py * (h / 2.0 - 10.0),
                8.0,
                (STICK_DOT, STICK_DOT, 1.0),
            );
        }
    }

    /// Six dots in the header showing a borderless window can be dragged by it.
    fn grip(&mut self, dc: HDC) {
        let color = if self.connected { GRIP_ON } else { GRIP_OFF };
        let start_x = LOGICAL_WIDTH / 2.0 - 11.0;
        for row in 0..2 {
            for column in 0..3 {
                self.rounded(
                    dc,
                    (
                        start_x + column as f32 * 8.0,
                        8.0 + row as f32 * 5.0,
                        5.0,
                        3.0,
                    ),
                    (color, color, 1.0),
                    Some(1),
                );
            }
        }
    }

    /// A gravity dial plus the raw axis values, or a note when there is no IMU.
    fn draw_motion(&mut self, dc: HDC) {
        let (panel_x, panel_y, panel_w, panel_h) = MOTION_PANEL;
        let top = LOGICAL_HEIGHT + panel_y;
        self.rounded(
            dc,
            (panel_x, top, panel_w, panel_h),
            (PANEL, PANEL_EDGE, 1.0),
            None,
        );
        let (line_x, _, line_w, _) = MOTION_LINES[0];
        let sample = if self.connected {
            self.state.and(self.gamepad.motion())
        } else {
            None
        };

        let Some(sample) = sample else {
            self.tilt_dial(dc, 0.0, 0.0, false);
            let (_, text_y, _, text_h) = MOTION_LINES[1];
            self.text(
                dc,
                Label::new(
                    "waiting for motion data...",
                    (line_x, top + text_y, line_w, text_h),
                    10,
                    TEXT_OFF,
                ),
            );
            return;
        };

        self.tilt_dial(
            dc,
            sample.accel[TILT_HORIZONTAL] * TILT_HORIZONTAL_SIGN,
            sample.accel[TILT_VERTICAL] * TILT_VERTICAL_SIGN,
            true,
        );
        let accel = format!(
            "accel  {:+6.2} {:+6.2} {:+6.2} g",
            sample.accel[0], sample.accel[1], sample.accel[2]
        );
        let gyro = format!(
            "gyro   {:+7.1} {:+7.1} {:+7.1} deg/s",
            sample.gyro[0], sample.gyro[1], sample.gyro[2]
        );
        for ((_, text_y, _, text_h), line) in MOTION_LINES.iter().zip([accel, gyro]) {
            self.text(
                dc,
                Label::new(&line, (line_x, top + *text_y, line_w, *text_h), 11, TEXT),
            );
        }
    }

    /// A small pad face with a dot where gravity pulls, clamped to the rim.
    ///
    /// Both inputs are the two axes that lie in the plane of the pad's face, already
    /// negated so the dot leans toward the lower edge. On a DualShock the face
    /// normal is Y, so Y is deliberately not one of them.
    fn tilt_dial(&mut self, dc: HDC, horizontal: f32, vertical: f32, live: bool) {
        let (x, y, size) = MOTION_DIAL;
        let y = y + LOGICAL_HEIGHT + MOTION_PANEL.1;
        self.rounded(
            dc,
            (x, y, size, size),
            (STICK_BOX_FILL, STICK_CROSS, 1.0),
            None,
        );
        self.line(
            dc,
            (x + size / 2.0, y + 4.0),
            (x + size / 2.0, y + size - 4.0),
            STICK_CROSS_LINE,
            1.0,
        );
        self.line(
            dc,
            (x + 4.0, y + size / 2.0),
            (x + size - 4.0, y + size / 2.0),
            STICK_CROSS_LINE,
            1.0,
        );
        if !live {
            return;
        }
        let reach = size / 2.0 - 6.0;
        self.circle(
            dc,
            x + size / 2.0 + horizontal.clamp(-1.0, 1.0) * reach,
            y + size / 2.0 - vertical.clamp(-1.0, 1.0) * reach,
            4.0,
            (STICK_DOT, STICK_DOT, 1.0),
        );
    }

    // ---- double buffering ----

    fn buffer(&mut self, dc: HDC, width: i32, height: i32) -> Option<HDC> {
        if width <= 0 || height <= 0 {
            return None;
        }
        if let Some(memory) = self.memory_dc
            && self.buffer_size == (width, height)
        {
            return Some(memory);
        }
        self.release_buffer();
        // SAFETY: `dc` is a live device context for the duration of the call.
        let (memory, bitmap) = unsafe {
            let memory = CreateCompatibleDC(Some(dc));
            let bitmap = CreateCompatibleBitmap(dc, width, height);
            if memory.0.is_null() || bitmap.0.is_null() {
                (None, None)
            } else {
                (Some(memory), Some(bitmap))
            }
        };
        let (memory, bitmap) = match (memory, bitmap) {
            (Some(memory), Some(bitmap)) => (memory, bitmap),
            _ => {
                self.release_buffer();
                return None;
            }
        };
        // SAFETY: the bitmap is ours to select, and the old object is kept so it
        // can be put back before the DC is deleted.
        self.memory_old_bitmap = Some(unsafe { SelectObject(memory, bitmap.into()) });
        self.memory_dc = Some(memory);
        self.memory_bitmap = Some(bitmap);
        self.buffer_size = (width, height);
        Some(memory)
    }

    fn release_buffer(&mut self) {
        if let Some(memory) = self.memory_dc.take() {
            if let Some(old) = self.memory_old_bitmap.take() {
                // SAFETY: `old` is what the DC had selected before our bitmap.
                unsafe {
                    let _ = SelectObject(memory, old);
                }
            }
            // SAFETY: created by CreateCompatibleDC, deleted once.
            unsafe {
                let _ = DeleteDC(memory);
            }
        }
        if let Some(bitmap) = self.memory_bitmap.take() {
            // SAFETY: no DC holds it after the select above was undone.
            unsafe {
                let _ = DeleteObject(bitmap.into());
            }
        }
        self.buffer_size = (0, 0);
    }

    fn paint(&mut self, hwnd: HWND) {
        // SAFETY: `hwnd` is this window's, and the struct outlives Begin/EndPaint.
        unsafe {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &raw mut paint);
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &raw mut client);
            let width = client.right - client.left;
            let height = client.bottom - client.top;

            if let Some(memory) = self.buffer(dc, width, height) {
                self.draw(memory, width, height);
                BitBlt(
                    dc,
                    0,
                    0,
                    width,
                    height,
                    Some(memory),
                    0,
                    0,
                    ROP_CODE(SRCCOPY.0),
                )
                .ok();
            }
            let _ = EndPaint(hwnd, &raw const paint);
        }
    }
}

impl Drop for WindowState<'_> {
    fn drop(&mut self) {
        self.release_buffer();
        for brush in self.brushes.objects.values() {
            // SAFETY: created by CreateSolidBrush and owned here.
            unsafe {
                let _ = DeleteObject((*brush).into());
            }
        }
        for pen in self.pens.objects.values() {
            // SAFETY: created by CreatePen and owned here.
            unsafe {
                let _ = DeleteObject((*pen).into());
            }
        }
        if let Some(font) = self.font.take() {
            // SAFETY: created by CreateFontW and owned here.
            unsafe {
                let _ = DeleteObject(font.into());
            }
        }
    }
}

/// Turn a screen point packed into `lParam` into a hit-test result.
fn hit_test(state: &WindowState<'_>, hwnd: HWND, lparam: LPARAM) -> isize {
    let mut point = POINT {
        x: (lparam.0 & 0xFFFF) as i32,
        y: ((lparam.0 >> 16) & 0xFFFF) as i32,
    };
    // SAFETY: `point` is a live POINT and `hwnd` is a live window.
    let inside = unsafe { ScreenToClient(hwnd, &raw mut point).as_bool() }
        && point.y as f32 <= state.title_bottom();
    if inside {
        WM_NCHITTEST_CAPTION
    } else {
        HTCLIENT as isize
    }
}

/// The window procedure. Everything it needs hangs off `GWLP_USERDATA`.
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: WM_NCCREATE carries the CREATESTRUCTW holding the pointer handed to
    // CreateWindowExW, and it is the first message this window ever sees.
    if message == WM_NCCREATE {
        // SAFETY: the OS guarantees lParam is a CREATESTRUCTW here.
        let create = unsafe {
            &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW)
        };
        if !create.lpCreateParams.is_null() {
            // SAFETY: GWLP_USERDATA is this window's own user data slot, and the
            // pointer came straight from our own CreateWindowExW call.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
        }
    }

    // SAFETY: only ever set to a pointer this crate owns, in WM_NCCREATE above.
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState<'_>;

    if message == WM_NCCREATE {
        return LRESULT(1);
    }

    if raw.is_null() {
        // SAFETY: the handle and message came from the OS.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: the pointer came from a `Box` owned by `IndicatorWindow`, which
    // outlives the window; see the module docs.
    let state = unsafe { &mut *raw };

    match message {
        WM_DESTROY => {
            // SAFETY: `hwnd` is live and the timer was set on it.
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // SAFETY: clearing our own user data slot before the handle dies.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            LRESULT(0)
        }
        WM_TIMER => {
            state.poll();
            // SAFETY: `hwnd` is live; a null rect means the whole client area.
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            state.paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_KEYDOWN | WM_SYSKEYDOWN if wparam == VK_ESCAPE => {
            // SAFETY: `hwnd` is live.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            // SAFETY: `hwnd` is live.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_NCHITTEST if state.borderless => LRESULT(hit_test(state, hwnd, lparam)),
        WM_SETCURSOR if state.borderless && !state.click_through => {
            if hit_test(state, hwnd, lparam) == WM_NCHITTEST_CAPTION {
                // SAFETY: a stock cursor id, loaded on demand by the OS.
                let cursor = unsafe { LoadCursorW(None, PWSTR(IDC_SIZEALL.0 as *mut u16)) }
                    .unwrap_or_default();
                unsafe {
                    let _ = SetCursor(Some(cursor));
                }
                return LRESULT(1);
            }
            LRESULT(0)
        }
        // SAFETY: the handle and message came from the OS.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// Owns a window class and message loop that renders controller state.
pub struct IndicatorWindow<'a> {
    state: Box<WindowState<'a>>,
    class_name: Vec<u16>,
    opacity: f32,
    poll_hz: f32,
    topmost: bool,
    hwnd: Option<HWND>,
}

impl<'a> IndicatorWindow<'a> {
    /// Build a window. Nothing is shown until [`IndicatorWindow::run`].
    ///
    /// # Errors
    /// If the window class or window cannot be created.
    pub fn new(
        gamepad: &'a Gamepad<'a>,
        title: &str,
        topmost: bool,
        borderless: bool,
        click_through: bool,
        opacity: f32,
        poll_hz: f32,
    ) -> Result<Self> {
        // A per-instance class name keeps two windows from fighting over one class.
        let class_name: Vec<u16> = format!(
            "ControllerIndicatorWindow_{}",
            std::process::id() as usize * 1000 + title.len()
        )
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

        let hwnd = HWND(std::ptr::null_mut());
        let state = Box::new(WindowState {
            gamepad,
            title: title.encode_utf16().chain(std::iter::once(0)).collect(),
            borderless,
            click_through,
            motion_panel: gamepad.has_motion(),
            hwnd,
            state: None,
            connected: false,
            scale: 1.0,
            offset: (0.0, 0.0),
            brushes: Cached::default(),
            pens: Cached::default(),
            font: None,
            font_px: 0,
            memory_dc: None,
            memory_bitmap: None,
            memory_old_bitmap: None,
            buffer_size: (0, 0),
        });

        Ok(Self {
            state,
            class_name,
            opacity,
            poll_hz,
            topmost,
            hwnd: None,
        })
    }

    /// Show the window and run the message loop until it is closed.
    ///
    /// # Errors
    /// If the window cannot be created.
    pub fn run(&mut self) -> Result<i32> {
        self.create()?;
        // SAFETY: the message is a plain struct on the stack.
        unsafe {
            let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            while GetMessageW(&raw mut message, None, 0, 0).as_bool() {
                // SAFETY: `message` was filled in by GetMessageW just above.
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
        Ok(0)
    }

    /// Register the class and create the window.
    ///
    /// # Errors
    /// If either Win32 call fails.
    pub fn create(&mut self) -> Result<HWND> {
        // Per-monitor v2 is Windows 10 1703 and later; the older context still
        // stops the window being rescaled by the system.
        // SAFETY: both calls only affect this process, and either may fail on old
        // Windows, which is why the fallback exists.
        unsafe {
            if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
                let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
                let _ = SetProcessDPIAware();
            }
        }
        // SAFETY: a plain DPI query; 96 is the documented fallback.
        let dpi = unsafe { GetDpiForSystem() }.max(1) as f32;
        let scale = MIN_SCALE.max(dpi / DPI_BASE);

        let style = if self.state.borderless {
            WINDOW_STYLE(WS_POPUP.0)
        } else {
            WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0)
        };
        let mut ex_style = WINDOW_EX_STYLE(0);
        if self.topmost {
            ex_style |= WS_EX_TOPMOST;
        }
        if self.state.click_through {
            ex_style |= WS_EX_LAYERED | WS_EX_TRANSPARENT;
        } else if self.opacity < 1.0 {
            ex_style |= WS_EX_LAYERED;
        }
        if self.state.borderless {
            ex_style |= WS_EX_TOOLWINDOW;
        }

        let mut frame = RECT {
            left: 0,
            top: 0,
            right: (LOGICAL_WIDTH * scale).round() as i32,
            bottom: (self.state.height() * scale).round() as i32,
        };
        // SAFETY: `frame` is a live RECT on the stack.
        unsafe { AdjustWindowRectEx(&raw mut frame, style, false, ex_style) }.ok();
        let width = frame.right - frame.left;
        let height = frame.bottom - frame.top;

        // SAFETY: plain system metrics, no arguments to get wrong.
        let (screen_w, screen_h) =
            unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let x = (screen_w - width) / 2;
        let y = (screen_h - height) / 2;

        self.register_class();

        let title: Vec<u16> = self.state.title.clone();
        // SAFETY: the class and title pointers are NUL terminated and outlive the
        // call; the user pointer is stashed by WM_NCCREATE and owned by `self`.
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                PCWSTR(self.class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                style | WINDOW_STYLE(WS_VISIBLE.0),
                x,
                y,
                width,
                height,
                None,
                None,
                Some(module_handle()),
                Some(&raw mut *self.state as *const c_void),
            )
        }
        .context("could not create the indicator window")?;

        self.hwnd = Some(hwnd);
        self.state.hwnd = hwnd;

        if ex_style.contains(WS_EX_LAYERED) {
            // SAFETY: `hwnd` was just created and is live.
            unsafe {
                SetLayeredWindowAttributes(
                    hwnd,
                    COLORREF(0),
                    (self.opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
                    LAYERED_WINDOW_ATTRIBUTES_FLAGS(LWA_ALPHA.0),
                )
                .ok();
            }
        }
        // SAFETY: `hwnd` is live; a null timer function means WM_TIMER dispatch.
        unsafe {
            let _ = SetTimer(
                Some(hwnd),
                TIMER_ID,
                (1000.0 / self.poll_hz.max(1.0)) as u32,
                None,
            );
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        self.state.poll();
        Ok(hwnd)
    }

    fn register_class(&self) {
        // SAFETY: the cursor is a stock resource and the class is plain data.
        let (cursor, instance) = unsafe {
            (
                LoadCursorW(None, PWSTR(IDC_ARROW.0 as *mut u16)).unwrap_or_default(),
                module_handle(),
            )
        };
        let class = WNDCLASSW {
            style: WNDCLASS_STYLES(CS_DBLCLKS.0),
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: windows::Win32::UI::WindowsAndMessaging::HICON(std::ptr::null_mut()),
            hCursor: cursor,
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(self.class_name.as_ptr()),
        };
        // SAFETY: the struct and both strings outlive the call.
        unsafe { RegisterClassW(&raw const class) };
    }
}

impl Drop for IndicatorWindow<'_> {
    fn drop(&mut self) {
        // Destroy the window before the state it points at is dropped.
        if let Some(hwnd) = self.hwnd.take() {
            // SAFETY: the window is still live; the state outlives it because
            // this Drop runs before the Box is released.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}
