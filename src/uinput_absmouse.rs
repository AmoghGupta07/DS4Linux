//! Minimal raw `/dev/uinput` wrapper for an ABSOLUTE-positioning pointer
//! device (touchscreen/tablet-style), used for the touchpad's
//! AbsoluteMouse mode.
//!
//! DELIBERATELY SEPARATE from uinput_mouse.rs's relative mouse device
//! rather than added onto it: mixing EV_REL (relative motion) and
//! EV_ABS+INPUT_PROP_DIRECT (absolute, direct positioning) semantics on
//! one device isn't how real hardware works -- a device is either a
//! mouse or a touchscreen/tablet, not both -- and keeping them
//! genuinely separate avoids any ambiguity for the display server about
//! which paradigm a given event stream follows. Also keeps
//! uinput_mouse.rs (used by KBM mode and MouseRemap, both load-bearing
//! and already exercised) untouched, same reasoning uinput_x360.rs gave
//! for not modifying uinput_ds4.rs.
//!
//! INPUT_PROP_DIRECT is what tells the display server (X11/libinput,
//! Wayland compositors) to treat this as a DIRECT positioning device --
//! touching a point immediately warps the cursor there, proportionally
//! mapped across the whole screen -- rather than as a relative mouse or
//! an indirect pointing device like a graphics tablet
//! (INPUT_PROP_POINTER) that would otherwise need separate calibration.
//! This is the same property real touchscreens report. Set via
//! UI_SET_PROPBIT, confirmed against the actual kernel uinput.h source
//! (`#define UI_SET_PROPBIT _IOW(UINPUT_IOCTL_BASE, 110, int)`) and
//! linux/input.h (`#define INPUT_PROP_DIRECT 0x01`) rather than assumed
//! -- getting either of these wrong wouldn't just fail to compile, it
//! would silently create a device the display server interprets
//! incorrectly.
//!
//! ABS_X/ABS_Y range is set to DS4's native touchpad resolution
//! (0-1919, 0-941, matching uinput_ds4.rs's DS4_TOUCHPAD_MAX_X/Y)
//! exactly, so touchpad coordinates can be forwarded 1:1 with zero
//! rescaling -- the display server handles proportionally mapping that
//! declared range across however many actual screen pixels exist.
//!
//! NOT YET VERIFIED against a real display server the way the other
//! uinput_*.rs devices were checked with evtest/jstest/games -- confirm
//! with `evtest` (should show ABS_X/ABS_Y with the expected range and
//! an INPUT_PROP_DIRECT flag) and by touching different corners of the
//! DS4 touchpad and watching the cursor jump proportionally around the
//! actual screen before trusting this for real use.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::mem;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;

/// DS4 touchpad native resolution -- same values as
/// uinput_ds4::DS4_TOUCHPAD_MAX_X/Y, duplicated here rather than
/// imported so this file has no dependency on uinput_ds4.rs at all
/// (consistent with keeping every uinput_*.rs wrapper independent, see
/// module doc above).
pub const TOUCHPAD_MAX_X: i32 = 1919;
pub const TOUCHPAD_MAX_Y: i32 = 941;

/// Direct/touchscreen-style input property (linux/input.h). Tells the
/// display server this device positions the cursor directly rather
/// than relatively.
const INPUT_PROP_DIRECT: u32 = 0x01;

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
    kind: u16,
    code: u16,
    value: i32,
}

const UI_SET_EVBIT: u64 = ioctl_w_int(b'U', 100);
const UI_SET_KEYBIT: u64 = ioctl_w_int(b'U', 101);
const UI_SET_ABSBIT: u64 = ioctl_w_int(b'U', 103);
const UI_SET_PROPBIT: u64 = ioctl_w_int(b'U', 110);
const UI_DEV_SETUP: u64 = ioctl_w::<UinputSetup>(b'U', 3);
const UI_ABS_SETUP: u64 = ioctl_w::<UinputAbsSetup>(b'U', 4);
const UI_DEV_CREATE: u64 = ioctl_none(b'U', 1);
const UI_DEV_DESTROY: u64 = ioctl_none(b'U', 2);

const fn ioctl_none(kind: u8, nr: u8) -> u64 {
    ((kind as u64) << 8) | (nr as u64)
}
const fn ioctl_w_int(kind: u8, nr: u8) -> u64 {
    (1u64 << 30) | (4u64 << 16) | ((kind as u64) << 8) | (nr as u64)
}
const fn ioctl_w<T>(kind: u8, nr: u8) -> u64 {
    (1u64 << 30) | ((mem::size_of::<T>() as u64) << 16) | ((kind as u64) << 8) | (nr as u64)
}

extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

pub struct VirtualAbsMouse {
    file: File,
}

impl VirtualAbsMouse {
    pub fn create() -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")?;
        let fd = file.as_raw_fd();

        unsafe {
            check(ioctl(fd, UI_SET_EVBIT, EV_KEY as i32))?;
            check(ioctl(fd, UI_SET_KEYBIT, BTN_LEFT as i32))?;
            check(ioctl(fd, UI_SET_KEYBIT, BTN_RIGHT as i32))?;

            check(ioctl(fd, UI_SET_EVBIT, EV_ABS as i32))?;
            check(ioctl(fd, UI_SET_ABSBIT, ABS_X as i32))?;
            check(ioctl(fd, UI_SET_ABSBIT, ABS_Y as i32))?;

            // The property that actually makes this behave like a
            // touchscreen/tablet rather than an oddly-configured mouse
            // -- see this file's module doc for why it matters.
            check(ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT as i32))?;

            setup_abs(fd, ABS_X, 0, TOUCHPAD_MAX_X, 0, 0, 0)?;
            setup_abs(fd, ABS_Y, 0, TOUCHPAD_MAX_Y, 0, 0, 0)?;

            let mut name = [0i8; UINPUT_MAX_NAME_SIZE];
            let cname = CString::new("ds4l Virtual Absolute Pointer").unwrap();
            for (i, &b) in cname.as_bytes_with_nul().iter().take(UINPUT_MAX_NAME_SIZE).enumerate() {
                name[i] = b as i8;
            }

            let setup = UinputSetup {
                id: InputId {
                    bustype: BUS_USB,
                    // Arbitrary/generic VID/PID, same reasoning as
                    // uinput_mouse.rs: no protocol requirement to spoof
                    // a specific real device here, the display server
                    // just needs a normal-looking direct pointer.
                    vendor: 0x1234,
                    product: 0x0002,
                    version: 0x0100,
                },
                name,
                ff_effects_max: 0,
            };
            check(ioctl(fd, UI_DEV_SETUP, &setup))?;
            check(ioctl(fd, UI_DEV_CREATE))?;
        }

        std::thread::sleep(std::time::Duration::from_millis(200));

        Ok(VirtualAbsMouse { file })
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

impl Drop for VirtualAbsMouse {
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
