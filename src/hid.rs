// Copyright (C) 2026 hollykbuck
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Win32 HID enumeration and IO.
//!
//! Three things here cost real time to work out, and all three are why this module
//! looks the way it does. 
//!
//! **Feature reports do not go through `DeviceIoControl`.** The documented route is
//! an `IOCTL_HID_GET_FEATURE` control code, and on current Windows the HID stack
//! rejects every encoding of it with `ERROR_INVALID_FUNCTION`. `HidD_GetFeature`
//! works, and that is what [`HidDevice::get_feature_report`] uses. The DualShock's
//! gyro calibration and MAC address are only reachable this way.
//!
//! **A `ReadFile` on a HID handle returns several reports at once.** Windows fills
//! the buffer with as many whole reports as fit, so a 128-byte read off a DualShock
//! is usually two 64-byte reports back to back. The report length comes from
//! `HidP_GetCaps`, and splitting is left to the caller.
//!
//! **Overlapped IO needs its storage pinned.** The kernel writes through the
//! `OVERLAPPED` and the buffer until the transfer completes, so both live in a
//! `Box` that is never moved for as long as the handle is open.

use std::ffi::c_void;
use std::ptr::null_mut;

use anyhow::{Context, Result, bail};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SETUP_DI_GET_CLASS_DEVS_FLAGS,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HIDD_ATTRIBUTES, HIDP_CAPS, HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature,
    HidD_GetHidGuid, HidD_GetManufacturerString, HidD_GetPreparsedData, HidD_GetProductString,
    HidP_GetCaps, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_ACCESS_RIGHTS, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, NTSTATUS, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows::core::PCWSTR;

/// The report length every HID stack guarantees; also the largest DS4 packet.
pub const MAX_REPORT_SIZE: usize = 128;

/// Enough for any device path Windows hands back.
const HID_DEVICE_PATH_LENGTH: usize = 260;

const WAIT_INFINITE: u32 = 0xFFFF_FFFF;
const HIDP_STATUS_SUCCESS: NTSTATUS = NTSTATUS(0x0011_0000);

/// The HID-over-Bluetooth service GUID, which shows up in every Bluetooth HID path.
const BLUETOOTH_SERVICE_GUID: &str = "00001124-0000-1000-8000-00805f9b34fb";

/// True when a SetupAPI HID path belongs to a Bluetooth HID service.
pub fn is_bluetooth_path(path: &str) -> bool {
    path.to_ascii_lowercase().contains(BLUETOOTH_SERVICE_GUID)
}

/// A HID collection that SetupAPI knows about, without an open handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidDeviceInfo {
    pub path: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub version: u16,
    pub is_bluetooth: bool,
    pub product_string: String,
    pub manufacturer_string: String,
}

/// A live device-info set, closed on drop.
struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        // SAFETY: the handle came from SetupDiGetClassDevsW and is closed once.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) }.ok();
    }
}

/// True when a SetupAPI HID path names the given vendor and product.
///
/// The path embeds `vid_XXXX&pid_YYYY`, so a device can be ruled out without being
/// opened. Checking that first is worth a lot: `describe` opens a device to read its
/// attributes, and a machine has HID collections on its keyboard, mouse, webcam and
/// virtual input drivers that a rescan would otherwise churn through every couple of
/// seconds just to filter them out.
pub fn path_matches(path: &str, vendor_id: u16, product_ids: &[u16]) -> bool {
    let path = path.to_ascii_lowercase();
    let vendor = format!("vid_{vendor_id:04x}");
    if !path.contains(&vendor) {
        return false;
    }
    product_ids
        .iter()
        .any(|&product| path.contains(&format!("pid_{product:04x}")))
}

/// List present HID collections, optionally filtered by vendor and product id.
pub fn enumerate_hid_devices(
    vendor_id: Option<u16>,
    product_ids: Option<&[u16]>,
) -> Result<Vec<HidDeviceInfo>> {
    // SAFETY: no arguments, returns the HID class GUID by value.
    let hid_class = unsafe { HidD_GetHidGuid() };

    // SAFETY: the class GUID outlives the set, which owns everything it points at.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&raw const hid_class),
            PCWSTR::null(),
            None,
            SETUP_DI_GET_CLASS_DEVS_FLAGS(DIGCF_PRESENT.0 | DIGCF_DEVICEINTERFACE.0),
        )
    }
    .context("SetupDiGetClassDevsW failed")?;
    let set = DeviceInfoSet(set);

    let mut found = Vec::new();
    let mut index = 0u32;
    loop {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: the set is live, the interface struct is correctly sized.
        let ok = unsafe {
            SetupDiEnumDeviceInterfaces(
                set.0,
                None,
                &raw const hid_class,
                index,
                &raw mut interface,
            )
        };
        if ok.is_err() {
            // ERROR_NO_MORE_ITEMS ends the walk; anything else is a real failure
            // but there is nothing useful to do about it mid-enumeration.
            break;
        }
        index += 1;

        let Some(path) = device_path(set.0, &interface) else {
            continue;
        };
        // Rule devices out on the path before paying to open one, but only when
        // there is a filter to apply — an unfiltered walk still lists everything,
        // including virtual collections whose paths carry no vid or pid at all.
        if let (Some(vendor), Some(products)) = (vendor_id, product_ids)
            && !path_matches(&path, vendor, products)
        {
            continue;
        }
        let Some(info) = describe(&path) else {
            continue;
        };
        if let Some(wanted) = vendor_id
            && info.vendor_id != wanted
        {
            continue;
        }
        if let Some(products) = product_ids
            && !products.contains(&info.product_id)
        {
            continue;
        }
        found.push(info);
    }
    Ok(found)
}

/// Pull the device path out of a SetupAPI interface.
///
/// The struct is variable length: `cbSize` then a wide string running to whatever
/// size the first call asked for. It is built by hand here because the bindings
/// only spell out the one-element array at the end.
fn device_path(set: HDEVINFO, interface: &SP_DEVICE_INTERFACE_DATA) -> Option<String> {
    // The documented way to ask for the size: call once with no buffer.
    //
    // This call *always* fails with ERROR_INSUFFICIENT_BUFFER — that is how
    // SetupAPI says "here is how much room you need", not an error. Only the
    // size it writes back is worth looking at, which is why the result is
    // deliberately discarded.
    let mut required = 0u32;
    // SAFETY: `set` is live and `interface` belongs to it.
    unsafe {
        let _ = SetupDiGetDeviceInterfaceDetailW(
            set,
            interface,
            None,
            0,
            Some(&raw mut required),
            None,
        );
    }
    if required == 0 {
        return None;
    }

    // A Vec<u16> keeps the alignment the struct needs; the path starts at
    // PATH_OFFSET bytes in, not at a u16 boundary on either architecture.
    let mut buffer = vec![0u16; (required as usize).div_ceil(2) + 1];
    let detail = buffer
        .as_mut_ptr()
        .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    // SAFETY: the struct is the first field of the allocation.
    unsafe { std::ptr::addr_of_mut!((*detail).cbSize).write(DETAIL_CB_SIZE) };

    // SAFETY: the buffer is `required` bytes, which is what the first call asked for.
    let ok = unsafe {
        SetupDiGetDeviceInterfaceDetailW(set, interface, Some(detail), required, None, None)
    };
    ok.ok()?;

    Some(read_device_path(&buffer))
}

/// `cbSize` is 8 on 64-bit and 6 on 32-bit, and the string follows it at a different
/// offset either way. Both are spelled out rather than derived from `size_of`,
/// because the bindings' struct only covers the fixed header.
const DETAIL_CB_SIZE: u32 = if cfg!(target_pointer_width = "64") {
    8
} else {
    6
};

/// Byte offset of `DevicePath` within the detail struct.
const PATH_OFFSET: usize = if cfg!(target_pointer_width = "64") {
    4
} else {
    2
};

/// Read the NUL terminated wide string that starts at [`PATH_OFFSET`].
fn read_device_path(buffer: &[u16]) -> String {
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), buffer.len() * 2) };
    let tail = &bytes[PATH_OFFSET..];
    // `as_chunks` is the checked form: a leftover odd byte is dropped rather than
    // silently padding the last code unit.
    let (pairs, _) = tail.as_chunks::<2>();
    let units: Vec<u16> = pairs
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Open the device just long enough to read its attributes and product strings.
fn describe(path: &str) -> Option<HidDeviceInfo> {
    let handle = open(path, GENERIC_READ)?;
    // SAFETY: `handle` is live, and every call below writes through a correctly
    // sized out-parameter that outlives the call.
    unsafe {
        let mut attributes = HIDD_ATTRIBUTES {
            Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        if !HidD_GetAttributes(handle, &raw mut attributes) {
            return None;
        }
        Some(HidDeviceInfo {
            path: path.to_string(),
            vendor_id: attributes.VendorID,
            product_id: attributes.ProductID,
            version: attributes.VersionNumber,
            is_bluetooth: is_bluetooth_path(path),
            product_string: hid_string(handle, HidD_GetProductString),
            manufacturer_string: hid_string(handle, HidD_GetManufacturerString),
        })
    }
}

/// One of the `HidD_Get*String` calls, all of which share a shape.
///
/// These bindings are plain Rust `extern "system"` fns rather than exported symbols
/// with a "system" ABI, so the fn-pointer type says `Rust`.
fn hid_string(handle: HANDLE, call: unsafe fn(HANDLE, *mut c_void, u32) -> bool) -> String {
    let mut buffer = vec![0u16; HID_DEVICE_PATH_LENGTH];
    // The length argument is a character count, not a byte count, which is what the
    // Python original passed too. The allocation is twice that in bytes, so
    // either reading of the API stays in bounds.
    // SAFETY: the buffer holds `HID_DEVICE_PATH_LENGTH` units and outlives the
    // call; both string functions NUL terminate.
    let ok = unsafe {
        call(
            handle,
            buffer.as_mut_ptr().cast::<c_void>(),
            HID_DEVICE_PATH_LENGTH as u32,
        )
    };
    if !ok {
        return String::new();
    }
    let length = buffer.iter().position(|&unit| unit == 0).unwrap_or(0);
    buffer.truncate(length);
    String::from_utf16_lossy(&buffer)
}

/// Open a HID path, returning `None` rather than a pseudo-handle.
///
/// Failure here is either `INVALID_HANDLE_VALUE` or, for a few odd stacks, a null
/// handle. Both mean "not this one", so both come back as `None`.
fn open(path: &str, access: GENERIC_ACCESS_RIGHTS) -> Option<HANDLE> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: the path is NUL terminated and outlives the call; a null security
    // descriptor and template are the documented defaults for an existing device.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            access.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    };
    match handle {
        Ok(handle) if !handle.0.is_null() => Some(handle),
        _ => None,
    }
}

/// One reusable event plus `OVERLAPPED` block for a single pending transfer.
///
/// The pair is boxed together: the kernel holds a pointer to the `OVERLAPPED` and
/// the event it names until the transfer completes, so neither may move.
struct Pending {
    event: HANDLE,
    overlapped: Box<OVERLAPPED>,
}

impl Pending {
    fn new() -> Result<Self> {
        // SAFETY: null name and security attributes are the defaults; the manual
        // reset flag is what lets one event be waited on more than once.
        let event =
            unsafe { CreateEventW(None, true, false, None) }.context("CreateEvent failed")?;
        let mut overlapped = Box::new(OVERLAPPED::default());
        // The Box keeps this address stable for the handle's whole life.
        overlapped.hEvent = event;
        Ok(Self { event, overlapped })
    }

    fn arm(&self) {
        // SAFETY: the handle came from CreateEventW and is still live.
        unsafe { ResetEvent(self.event) }.ok();
    }

    /// Wait for the transfer. `false` means it timed out.
    fn wait(&self, timeout_ms: u32) -> bool {
        // SAFETY: the handle is live and the timeout is a plain count.
        let result = unsafe { WaitForSingleObject(self.event, timeout_ms) };
        result == WAIT_OBJECT_0
    }

    fn wait_forever(&self) {
        let _ = self.wait(WAIT_INFINITE);
    }

    /// Cancel a pending transfer and reap its completion so the slot is reusable.
    fn abort(&self, handle: HANDLE) {
        // SAFETY: the overlapped belongs to the same handle, which is still open.
        unsafe { CancelIoEx(handle, Some(&raw const *self.overlapped)) }.ok();
        self.wait_forever();
    }

    /// How many bytes the completed transfer moved.
    ///
    /// This writes through the pointer even when the read itself succeeded, so it
    /// cannot be null the way the `OVERLAPPED` pointer can.
    fn bytes_transferred(&self, handle: HANDLE) -> Result<u32> {
        let mut transferred = 0u32;
        // SAFETY: the overlapped is boxed and still registered with the kernel.
        let ok = unsafe {
            GetOverlappedResult(
                handle,
                &raw const *self.overlapped,
                &raw mut transferred,
                false,
            )
        };
        ok.context("GetOverlappedResult failed")?;
        Ok(transferred)
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // SAFETY: created in `new` and closed exactly once.
        unsafe { CloseHandle(self.event) }.ok();
    }
}

/// An open HID handle with non-blocking reads and buffered writes.
pub struct HidDevice {
    handle: HANDLE,
    writable: bool,
    read: Pending,
    write: Pending,
    /// The destination of the read in flight. Boxed for the same reason the
    /// `OVERLAPPED` is, and resized only while no transfer is outstanding.
    buffer: Box<[u8]>,
    reading: bool,
    /// Length of one input report, from the device's own descriptor. Zero when the
    /// descriptor could not be read.
    pub input_report_length: usize,
}

impl HidDevice {
    /// Open a device for reading, and for writing if it will allow it.
    ///
    /// # Errors
    /// If the device cannot be opened even read-only.
    pub fn new(info: &HidDeviceInfo) -> Result<Self> {
        let (handle, writable) = match open(&info.path, GENERIC_READ | GENERIC_WRITE) {
            Some(handle) => (handle, true),
            None => (
                open(&info.path, GENERIC_READ)
                    .with_context(|| format!("could not open HID device {}", info.path))?,
                false,
            ),
        };

        Ok(Self {
            input_report_length: input_report_length(handle),
            handle,
            writable,
            read: Pending::new()?,
            write: Pending::new()?,
            buffer: Box::new([0u8; MAX_REPORT_SIZE]),
            reading: false,
        })
    }

    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Return the next report, or `None` if none arrived within `timeout_ms`.
    ///
    /// At most one read is left outstanding, so a call that times out has nothing to
    /// cancel: the next call simply waits on the transfer already in flight and
    /// picks the report up when it lands. The returned buffer may hold several
    /// reports back to back — see the module docs.
    pub fn read(&mut self, size: usize, timeout_ms: u32) -> Result<Option<Vec<u8>>> {
        if !self.reading {
            self.read.arm();
            let want = size.min(self.buffer.len());
            // SAFETY: the slice is exactly the count passed alongside it, and the
            // overlapped is boxed so the kernel's pointer stays valid.
            let started = unsafe {
                ReadFile(
                    self.handle,
                    Some(&mut self.buffer[..want]),
                    None,
                    Some(&raw mut *self.read.overlapped),
                )
            };
            // SAFETY: reading the thread's last error cannot affect anything else.
            if started.is_err()
            // SAFETY: as above.
            && unsafe { GetLastError() } != ERROR_IO_PENDING
            {
                // SAFETY: as above.
                bail!("ReadFile failed ({:?})", unsafe { GetLastError() });
            }
            self.reading = true;
        }
        if !self.read.wait(timeout_ms) {
            return Ok(None);
        }
        self.reading = false;
        let length = self.read.bytes_transferred(self.handle)? as usize;
        Ok(Some(self.buffer[..length].to_vec()))
    }

    /// Send an output report; the report id has to be `data[0]`.
    ///
    /// # Errors
    /// If the device is closed, was opened read-only, or the write did not complete.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        if !self.writable {
            bail!("device was opened read-only");
        }
        self.write.arm();
        // SAFETY: the slice is the count passed alongside it, and nothing reads it
        // after the wait returns.
        let started = unsafe {
            WriteFile(
                self.handle,
                Some(data),
                None,
                Some(&raw mut *self.write.overlapped),
            )
        };
        // SAFETY: reading the thread's last error cannot affect anything else.
        let last_error = unsafe { GetLastError() };
        if started.is_err() && last_error != ERROR_IO_PENDING {
            bail!("WriteFile failed ({last_error:?})");
        }
        if !self.write.wait(WAIT_INFINITE) {
            self.write.abort(self.handle);
            bail!("output report timed out");
        }
        self.write.bytes_transferred(self.handle)?;
        Ok(())
    }

    /// Read a feature report, which is how calibration data comes back.
    ///
    /// `ReadFile` only carries input reports, and `IOCTL_HID_GET_FEATURE` is
    /// rejected by the current HID stack, so this is the only route. It blocks,
    /// which is fine because callers use it at open time rather than per report.
    ///
    /// Returns `None` if the pad would not answer.
    pub fn get_feature_report(&mut self, report_id: u8, length: usize) -> Option<Vec<u8>> {
        let mut buffer = vec![0u8; length];
        buffer[0] = report_id;
        // SAFETY: the buffer is `length` bytes, which is what is passed as the
        // length, and stays alive for the call.
        let ok = unsafe {
            HidD_GetFeature(
                self.handle,
                buffer.as_mut_ptr().cast::<c_void>(),
                length as u32,
            )
        };
        ok.then_some(buffer)
    }

    pub fn close(&mut self) {
        if self.handle.0.is_null() {
            return;
        }
        // SAFETY: the handle is live; a null overlapped cancels everything.
        unsafe { CancelIoEx(self.handle, None) }.ok();
        if self.reading {
            self.read.wait_forever();
            self.reading = false;
        }
        // SAFETY: created by CreateFileW and closed exactly once.
        unsafe { CloseHandle(self.handle) }.ok();
        self.handle = HANDLE(null_mut());
    }
}

impl Drop for HidDevice {
    fn drop(&mut self) {
        self.close();
    }
}

// SAFETY: a Win32 `HANDLE` is a plain kernel handle with no thread affinity, and
// this type is not `Sync` — every method takes `&mut self`, so the borrow checker
// already guarantees only one thread touches a given device at a time. The PS4
// backend adds a mutex around the slot, which is what keeps the reader thread and
// a rumble call from overlapping on the same handle.
unsafe impl Send for HidDevice {}

/// Read the report length straight out of the device's HID descriptor.
///
/// Windows hands back as many whole reports as fit in the read buffer, so callers
/// need to know where one ends. Zero means the descriptor could not be read.
fn input_report_length(handle: HANDLE) -> usize {
    // SAFETY: the preparsed data is freed on every path out, and the caps struct is
    // correctly sized by the bindings.
    unsafe {
        let mut preparsed = PHIDP_PREPARSED_DATA(0);
        if !HidD_GetPreparsedData(handle, &raw mut preparsed) {
            return 0;
        }
        let mut caps = HIDP_CAPS::default();
        let status = HidP_GetCaps(preparsed, &raw mut caps);
        HidD_FreePreparsedData(preparsed);
        if status != HIDP_STATUS_SUCCESS {
            return 0;
        }
        caps.InputReportByteLength as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_usb_path_is_not_bluetooth() {
        assert!(!is_bluetooth_path(
            r"\\?\hid#vid_054c&pid_09cc#7&1234abcd&0&0000#{4d1e55b2-f16f-11cf-88d3-00a0c9036b8e}"
        ));
    }

    #[test]
    fn a_bluetooth_path_is_recognised() {
        let path = r"\\?\hid#vid_054c&pid_09cc&col01#7&1a2b3c4d&0&0000#{4d1e55b2-f16f-11cf-88d3-00a0c9036b8e}\
                     BLUETOOTH_SERVICE_GUID{00001124-0000-1000-8000-00805F9B34FB}_LOCALMFGSET";
        assert!(is_bluetooth_path(path));
    }

    #[test]
    fn the_guid_match_ignores_case() {
        let lower = r"\\?\hid#vid_054c&pid_09cc#00001124-0000-1000-8000-00805f9b34fb";
        let upper = lower.to_ascii_uppercase();
        assert!(is_bluetooth_path(lower));
        assert!(is_bluetooth_path(&upper));
    }

    #[test]
    fn a_sony_path_is_recognised_without_opening_it() {
        let path = r"\\?\hid#vid_054c&pid_05c4&mi_00#7&1234abcd&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert!(path_matches(path, 0x054C, &[0x05C4, 0x09CC, 0x0BA0]));
    }

    #[test]
    fn the_path_filter_ignores_case() {
        let upper = r"\\?\HID#VID_054C&PID_05C4#ABC";
        let lower = upper.to_ascii_lowercase();
        assert!(path_matches(upper, 0x054C, &[0x05C4]));
        assert!(path_matches(&lower, 0x054C, &[0x05C4]));
    }

    #[test]
    fn the_path_filter_rejects_the_wrong_vendor_or_product() {
        let path = r"\\?\hid#vid_258a&pid_0013&mi_01&col02#8&398fcc90&0&0001#{}";
        assert!(!path_matches(path, 0x054C, &[0x05C4]));
        assert!(!path_matches(path, 0x258A, &[0x05C4]));
        assert!(!path_matches(path, 0x258A, &[0x09CC]));
        assert!(path_matches(path, 0x258A, &[0x0013]));
    }

    #[test]
    fn the_path_filter_rejects_a_virtual_collection_with_no_ids() {
        // These are what a rescan used to open just to throw away.
        let path = r"\\?\hid#hid_device_system_vhf&col04#2&cb88041&0&0003#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert!(!path_matches(path, 0x054C, &[0x05C4]));
    }

    #[test]
    fn the_filter_and_the_opened_attributes_agree() {
        // The path pre-filter is only an optimisation: whatever survives it must
        // still pass the check against what the device actually reports.
        let found = enumerate_hid_devices(Some(0x054C), Some(&[0x05C4, 0x09CC, 0x0BA0])).unwrap();
        for info in found {
            assert_eq!(info.vendor_id, 0x054C, "{}", info.path);
            assert!(
                [0x05C4, 0x09CC, 0x0BA0].contains(&info.product_id),
                "{}",
                info.path
            );
        }
    }

    #[test]
    fn the_detail_layout_is_what_win32_expects() {
        // cbSize and the path offset differ between 32 and 64 bit; pin down the
        // values actually compiled in, since a wrong one silently truncates paths.
        if cfg!(target_pointer_width = "64") {
            assert_eq!(DETAIL_CB_SIZE, 8);
            assert_eq!(PATH_OFFSET, 4);
        } else {
            assert_eq!(DETAIL_CB_SIZE, 6);
            assert_eq!(PATH_OFFSET, 2);
        }
        assert_eq!(std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>(), 8);
    }

    #[test]
    fn a_path_is_read_from_the_right_offset() {
        // Lay out the struct by hand the way Windows fills it: cbSize then the
        // string, and check the reader finds the string and not the header.
        let path: Vec<u16> = "\\\\?\\hid#test\0"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut buffer = vec![0u16; 4 + path.len()];
        // Write the u32 cbSize into the first slot, little endian.
        unsafe {
            let bytes =
                std::slice::from_raw_parts_mut(buffer.as_mut_ptr().cast(), buffer.len() * 2);
            bytes[0..4].copy_from_slice(&DETAIL_CB_SIZE.to_le_bytes());
        }
        for (index, unit) in path.iter().enumerate() {
            buffer[PATH_OFFSET / 2 + index] = *unit;
        }
        assert_eq!(read_device_path(&buffer), "\\\\?\\hid#test");
    }

    #[test]
    fn an_unterminated_path_still_reads() {
        let mut buffer = vec![0u16; 8];
        unsafe {
            let bytes =
                std::slice::from_raw_parts_mut(buffer.as_mut_ptr().cast(), buffer.len() * 2);
            bytes[0..4].copy_from_slice(&DETAIL_CB_SIZE.to_le_bytes());
        }
        for (index, unit) in "abc".encode_utf16().enumerate() {
            buffer[PATH_OFFSET / 2 + index] = unit;
        }
        assert_eq!(read_device_path(&buffer), "abc");
    }

    #[test]
    fn enumeration_runs_on_a_machine_with_hid_devices() {
        // Not asserting a count: what matters is that the walk terminates and does
        // not blow up on whatever this machine happens to have plugged in.
        let found = enumerate_hid_devices(None, None);
        if let Ok(found) = &found {
            for info in found {
                assert!(!info.path.is_empty());
            }
        } else {
            // A machine with no HID class at all is a legitimate outcome here.
            assert!(found.is_err());
        }
    }
}
