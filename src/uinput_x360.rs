//! Minimal raw `/dev/uinput` wrapper for a virtual Xbox 360 wired
//! controller.
//!
//! DELIBERATELY A SEPARATE FILE from uinput_ds4.rs rather than a
//! parameterized/generalized version of it: uinput_ds4.rs is
//! hardware-verified and load-bearing for every profile that's ever
//! been tested against a real controller. Refactoring it to share code
//! with a brand new, unverified device type would risk that proven
//! path for the sake of avoiding some duplication. This file copies
//! uinput_ds4.rs's ioctl plumbing (same raw uinput approach, same
//! reasoning for not using the high-level `uinput` crate: it doesn't
//! expose a custom `input_id`, and spoofing a specific VID/PID is again
//! the entire point) and changes only what actually needs to differ:
//! device identity and axis ranges.
//!
//! WHAT "XINPUT" MEANS HERE: XInput itself is a Windows API with no
//! Linux equivalent -- there is nothing to literally reimplement. What
//! this module does instead is spoof the Xbox 360 wired controller's
//! real VID/PID (045E:028E) on a virtual uinput gamepad, the same
//! technique uinput_ds4.rs already uses for DS4's VID/PID. SDL2,
//! Proton/Wine, and anything else that identifies controller type by
//! VID/PID (which is how this actually works in practice -- there's no
//! "xpad.ko recognizes uinput devices" step; xpad.ko binds to real USB
//! devices over the USB bus, and a uinput device never touches that
//! bus at all) will see 045E:028E and treat it as a genuine Xbox 360
//! pad, matching SDL2's controller database entry for that ID.
//!
//! NOT YET VERIFIED against real software the way uinput_ds4.rs was
//! confirmed with evtest/jstest/sdl2-jstest/real games across several
//! milestones -- this is new. Before trusting it: `evtest` should show
//! a device named "Microsoft X-Box 360 pad", `sdl2-jstest --list`
//! should report it with that VID/PID, and a game/framework that
//! specifically branches on Xbox-vs-other controller type (rather than
//! just using generic SDL button prompts either way) is the real test
//! of whether this achieves anything uinput_ds4.rs didn't already.
//!
//! Reference: https://www.kernel.org/doc/html/latest/input/uinput.html

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::mem;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;

pub const SYN_REPORT: u16 = 0;

// Button codes (evdev) -- SAME codes as uinput_ds4.rs's cross/circle/
// triangle/square, since evdev's BTN_SOUTH/EAST/NORTH/WEST are generic
// "which corner of the diamond" codes, not PlayStation-specific ones.
// An Xbox pad's A/B/X/Y map onto exactly these same codes: A=SOUTH,
// B=EAST, X=WEST, Y=NORTH. This is WHY reusing PadState's existing
// cross/circle/square/triangle bools for X360 output (see
// ds4l_daemon.rs's emit_x360_state) needs no remapping table at all --
// the "translation" from DS4 button identity to Xbox button identity
// already happened for free in the Linux evdev ABI's naming scheme.
pub const BTN_SOUTH: u16 = 0x130; // A
pub const BTN_EAST: u16 = 0x131; // B
pub const BTN_NORTH: u16 = 0x133; // Y
pub const BTN_WEST: u16 = 0x134; // X
pub const BTN_TL: u16 = 0x136; // Left bumper
pub const BTN_TR: u16 = 0x137; // Right bumper
pub const BTN_SELECT: u16 = 0x13a; // Back
pub const BTN_START: u16 = 0x13b; // Start
pub const BTN_THUMBL: u16 = 0x13d; // Left stick click
pub const BTN_THUMBR: u16 = 0x13e; // Right stick click
pub const BTN_MODE: u16 = 0x13c; // Guide/Xbox button
// NOTE: no equivalent of DS4's BTN_TL2/BTN_TR2 (L2/R2 "digital click").
// A real Xbox 360 pad's triggers are purely analog with no separate
// digital click button -- omitting these isn't a missing feature, it's
// accurately NOT emulating something the real hardware doesn't have.

pub const ABS_X: u16 = 0x00; // left stick X
pub const ABS_Y: u16 = 0x01; // left stick Y
pub const ABS_Z: u16 = 0x02; // left trigger
pub const ABS_RX: u16 = 0x03; // right stick X
pub const ABS_RY: u16 = 0x04; // right stick Y
pub const ABS_RZ: u16 = 0x05; // right trigger
pub const ABS_HAT0X: u16 = 0x10; // dpad X
pub const ABS_HAT0Y: u16 = 0x11; // dpad Y

/// Xbox 360 stick range, matching the Linux kernel xpad driver's actual
/// reported evdev range for a real wired 360 pad (16-bit signed,
/// 0=center) -- NOT DS4's 0-255 range. This is the main reason
/// ds4l_daemon.rs needs a distinct emit function (emit_x360_state)
/// rather than reusing emit_gamepad_state: every stick value has to be
/// rescaled, not just relabeled.
pub const STICK_MIN: i32 = -32768;
pub const STICK_MAX: i32 = 32767;

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

pub struct VirtualX360 {
    file: File,
}

impl VirtualX360 {
    /// Opens /dev/uinput and registers a virtual gamepad presenting
    /// itself with the Xbox 360 wired controller's real vendor/product
    /// ID, so SDL2/evdev consumers treat it as a genuine Xbox 360 pad.
    pub fn create() -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")?;
        let fd = file.as_raw_fd();

        unsafe {
            check(ioctl(fd, UI_SET_EVBIT, EV_KEY as i32))?;
            for &btn in &[
                BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST, BTN_TL, BTN_TR, BTN_SELECT, BTN_START,
                BTN_THUMBL, BTN_THUMBR, BTN_MODE,
            ] {
                check(ioctl(fd, UI_SET_KEYBIT, btn as i32))?;
            }

            check(ioctl(fd, UI_SET_EVBIT, EV_ABS as i32))?;
            for &axis in &[ABS_X, ABS_Y, ABS_RX, ABS_RY, ABS_Z, ABS_RZ, ABS_HAT0X, ABS_HAT0Y] {
                check(ioctl(fd, UI_SET_ABSBIT, axis as i32))?;
            }

            // Sticks: -32768..32767, centered at 0 -- Xbox 360's actual
            // reported range, NOT DS4's 0-255. Fuzz/flat values (16, 128)
            // are conventional defaults matching what the real kernel
            // xpad driver reports for a wired 360 pad, giving games a
            // small amount of built-in noise tolerance similar to real
            // hardware rather than a razor-sharp centered value.
            for &axis in &[ABS_X, ABS_Y, ABS_RX, ABS_RY] {
                setup_abs(fd, axis, STICK_MIN, STICK_MAX, 0, 16, 128)?;
            }
            // Triggers: 0-255, resting at 0 (unpressed) -- same range as
            // DS4's, no rescaling needed for these when translating.
            for &axis in &[ABS_Z, ABS_RZ] {
                setup_abs(fd, axis, 0, 255, 0, 0, 0)?;
            }
            // D-pad hat: -1/0/1 per axis, same convention as uinput_ds4.rs.
            for &axis in &[ABS_HAT0X, ABS_HAT0Y] {
                setup_abs(fd, axis, -1, 1, 0, 0, 0)?;
            }

            let mut name = [0i8; UINPUT_MAX_NAME_SIZE];
            let cname = CString::new("Microsoft X-Box 360 pad").unwrap();
            let bytes = cname.as_bytes_with_nul();
            for (i, &b) in bytes.iter().take(UINPUT_MAX_NAME_SIZE).enumerate() {
                name[i] = b as i8;
            }

            let setup = UinputSetup {
                id: InputId {
                    bustype: BUS_USB,
                    vendor: 0x045E,
                    product: 0x028E, // Xbox 360 wired controller
                    version: 0x0100,
                },
                name,
                ff_effects_max: 0,
            };
            check(ioctl(fd, UI_DEV_SETUP, &setup))?;
            check(ioctl(fd, UI_DEV_CREATE))?;
        }

        std::thread::sleep(std::time::Duration::from_millis(500));

        Ok(VirtualX360 { file })
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

impl Drop for VirtualX360 {
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

/// Rescales a DS4-native stick byte (0-255, 128=center) to the Xbox 360
/// range (-32768..32767, 0=center). Not a simple linear `(x - 128) *
/// 257` because that would map 255 -> 32639, short of the true max
/// (32767) by 128 -- close enough to "full deflection" that most games
/// wouldn't notice, but not exact. Splitting the scale at center and
/// using each half's own multiplier (128 steps below center -> -32768,
/// 127 steps above center -> 32767) lands exactly on both true extremes
/// instead of leaving the positive side slightly short.
pub fn rescale_stick_axis(byte_value: u8) -> i32 {
    let centered = byte_value as i32 - 128;
    if centered < 0 {
        // -128..-1 -> -32768..-256, i.e. multiply by 256
        centered * 256
    } else {
        // 0..127 -> 0..32767 -- 127 * 258 = 32766, one short of 32767;
        // handle the exact top value explicitly so full-right/full-up
        // reliably reports the true maximum rather than 32766.
        if byte_value == 255 {
            STICK_MAX
        } else {
            centered * 258
        }
    }
}
