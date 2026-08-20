//! Minimal raw `/dev/uinput` wrapper.
//!
//! We talk to uinput directly via ioctls rather than using the high-level
//! `uinput` crate, because that crate's builder API does not expose setting
//! a custom `input_id` (vendor/product/version) — and spoofing DS4's real
//! VID/PID (054C/09CC) is the entire point of this exercise, since that's
//! what SDL2/games use to recognize the virtual pad as a DS4.
//!
//! Reference: https://www.kernel.org/doc/html/latest/input/uinput.html

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::mem;
use std::os::unix::io::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

// ---- ioctl / event type constants (from linux/input-event-codes.h, linux/uinput.h) ----

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;

pub const SYN_REPORT: u16 = 0;

// Button codes (evdev), matching DS4's real mapping via hid-sony/SDL2.
// Note the rotation vs Xbox: cross=SOUTH, circle=EAST, triangle=NORTH, square=WEST.
pub const BTN_SOUTH: u16 = 0x130; // Cross
pub const BTN_EAST: u16 = 0x131; // Circle
pub const BTN_NORTH: u16 = 0x133; // Triangle
pub const BTN_WEST: u16 = 0x134; // Square
pub const BTN_TL: u16 = 0x136; // L1
pub const BTN_TR: u16 = 0x137; // R1
pub const BTN_TL2: u16 = 0x138; // L2 (digital click, analog is ABS_Z)
pub const BTN_TR2: u16 = 0x139; // R2 (digital click, analog is ABS_RZ)
pub const BTN_SELECT: u16 = 0x13a; // Share
pub const BTN_START: u16 = 0x13b; // Options
pub const BTN_THUMBL: u16 = 0x13d; // L3
pub const BTN_THUMBR: u16 = 0x13e; // R3
pub const BTN_MODE: u16 = 0x13c; // PS button

// Abs axis codes
pub const ABS_X: u16 = 0x00; // left stick X
pub const ABS_Y: u16 = 0x01; // left stick Y
pub const ABS_Z: u16 = 0x02; // L2 analog (DS4 convention via hid-sony)
pub const ABS_RX: u16 = 0x03; // right stick X
pub const ABS_RY: u16 = 0x04; // right stick Y
pub const ABS_RZ: u16 = 0x05; // R2 analog
pub const ABS_HAT0X: u16 = 0x10; // dpad X
pub const ABS_HAT0Y: u16 = 0x11; // dpad Y

const BUS_USB: u16 = 0x03;

const UINPUT_MAX_NAME_SIZE: usize = 80;

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [i8; UINPUT_MAX_NAME_SIZE],
    ff_effects_max: u32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsInfo,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    kind: u16, // "type" is reserved in Rust
    code: u16,
    value: i32,
}

// ioctl numbers, computed the same way the kernel headers do.
// These are stable across kernel versions for uinput.
const UI_SET_EVBIT: u64 = ioctl_w_int(b'U', 100);
const UI_SET_KEYBIT: u64 = ioctl_w_int(b'U', 101);
const UI_SET_ABSBIT: u64 = ioctl_w_int(b'U', 103);
const UI_DEV_SETUP: u64 = ioctl_w::<UinputSetup>(b'U', 3);
const UI_ABS_SETUP: u64 = ioctl_w::<UinputAbsSetup>(b'U', 4);
const UI_DEV_CREATE: u64 = ioctl_none(b'U', 1);
const UI_DEV_DESTROY: u64 = ioctl_none(b'U', 2);

const fn ioctl_none(kind: u8, nr: u8) -> u64 {
    ((kind as u64) << 8) | (nr as u64)
}
const fn ioctl_w_int(kind: u8, nr: u8) -> u64 {
    // _IOW(kind, nr, int) — direction=write(1), size=4
    (1u64 << 30) | (4u64 << 16) | ((kind as u64) << 8) | (nr as u64)
}
const fn ioctl_w<T>(kind: u8, nr: u8) -> u64 {
    (1u64 << 30) | ((mem::size_of::<T>() as u64) << 16) | ((kind as u64) << 8) | (nr as u64)
}

extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

pub struct VirtualDs4 {
    file: File,
}

impl VirtualDs4 {
    /// Opens /dev/uinput and registers a virtual gamepad presenting itself
    /// with DS4 v2's real vendor/product ID, so SDL2/evdev consumers treat
    /// it as a genuine DualShock 4.
    pub fn create() -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")?;
        let fd = file.as_raw_fd();

        unsafe {
            // Enable key (button) event type and each button we use.
            check(ioctl(fd, UI_SET_EVBIT, EV_KEY as i32))?;
            for &btn in &[
                BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST, BTN_TL, BTN_TR, BTN_TL2,
                BTN_TR2, BTN_SELECT, BTN_START, BTN_THUMBL, BTN_THUMBR, BTN_MODE,
            ] {
                check(ioctl(fd, UI_SET_KEYBIT, btn as i32))?;
            }

            // Enable absolute axis event type and each axis we use.
            check(ioctl(fd, UI_SET_EVBIT, EV_ABS as i32))?;
            for &axis in &[
                ABS_X, ABS_Y, ABS_RX, ABS_RY, ABS_Z, ABS_RZ, ABS_HAT0X, ABS_HAT0Y,
            ] {
                check(ioctl(fd, UI_SET_ABSBIT, axis as i32))?;
            }

            // Configure each stick axis: DS4 native range is 0-255, 128=center,
            // matching the raw byte values we already parse in Milestone 1 —
            // no conversion needed when we wire real input in later.
            for &axis in &[ABS_X, ABS_Y, ABS_RX, ABS_RY] {
                setup_abs(fd, axis, 0, 255, 128, 2, 8)?;
            }
            // Triggers: 0-255, resting at 0 (unpressed).
            for &axis in &[ABS_Z, ABS_RZ] {
                setup_abs(fd, axis, 0, 255, 0, 0, 0)?;
            }
            // D-pad hat: -1/0/1 per axis.
            for &axis in &[ABS_HAT0X, ABS_HAT0Y] {
                setup_abs(fd, axis, -1, 1, 0, 0, 0)?;
            }

            // Device identity: real DS4 v2 VID/PID so SDL2's gamecontrollerdb
            // matches it as a DualShock 4.
            let mut name = [0i8; UINPUT_MAX_NAME_SIZE];
            let cname = CString::new("Sony Interactive Entertainment Wireless Controller")
                .unwrap();
            let bytes = cname.as_bytes_with_nul();
            for (i, &b) in bytes.iter().take(UINPUT_MAX_NAME_SIZE).enumerate() {
                name[i] = b as i8;
            }

            let setup = UinputSetup {
                id: InputId {
                    bustype: BUS_USB,
                    vendor: 0x054C,
                    product: 0x09CC, // DS4 v2
                    version: 0x0100,
                },
                name,
                ff_effects_max: 0,
            };
            check(ioctl(fd, UI_DEV_SETUP, &setup))?;
            check(ioctl(fd, UI_DEV_CREATE))?;
        }

        // Give userspace (udev, SDL) a moment to notice the new device
        // before we start sending events, as the kernel docs recommend.
        std::thread::sleep(std::time::Duration::from_millis(500));

        Ok(VirtualDs4 { file })
    }

    pub fn emit_abs(&mut self, code: u16, value: i32) -> io::Result<()> {
        self.write_event(EV_ABS, code, value)
    }

    pub fn emit_key(&mut self, code: u16, pressed: bool) -> io::Result<()> {
        self.write_event(EV_KEY, code, pressed as i32)
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.write_event(EV_SYN, SYN_REPORT, 0)
    }

    fn write_event(&mut self, kind: u16, code: u16, value: i32) -> io::Result<()> {
        let ev = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            kind,
            code,
            value,
        };
        let ptr = &ev as *const InputEvent as *const u8;
        let bytes = unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<InputEvent>()) };
        self.file.write_all(bytes)
    }
}

impl Drop for VirtualDs4 {
    fn drop(&mut self) {
        unsafe {
            let _ = ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

unsafe fn setup_abs(
    fd: i32,
    code: u16,
    min: i32,
    max: i32,
    value: i32,
    fuzz: i32,
    flat: i32,
) -> io::Result<()> {
    let setup = UinputAbsSetup {
        code,
        absinfo: InputAbsInfo {
            value,
            minimum: min,
            maximum: max,
            fuzz,
            flat,
            resolution: 0,
        },
    };
    check(ioctl(fd, UI_ABS_SETUP, &setup))
}

fn check(ret: i32) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
