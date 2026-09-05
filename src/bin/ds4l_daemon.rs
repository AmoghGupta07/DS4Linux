// The real daemon: enumerates every connected DS4 v2 (USB and/or
// Bluetooth) and runs each one on its own thread, loading a profile from
// ~/.config/ds4l/profiles/ per controller and driving gyro-to-stick +
// touchpad (both modes, 2-finger) from it. Left stick, buttons, dpad,
// triggers still pass through 1:1 as they have since Milestone 3.
//
// On connect: sets each controller's lightbar to its profile's configured
// color, and (if enabled) pulses rumble briefly -- confirms the daemon
// connected and loaded the right profile without needing to check a
// terminal.
//
// Usage:
//   ds4l_daemon                 # every connected controller loads/creates the "Default" profile
//   ds4l_daemon --profile Name  # every connected controller loads a specific profile by name
//
// Profile files live at ~/.config/ds4l/profiles/<name>.toml and are plain,
// hand-editable TOML -- edit one, restart the daemon, changes take effect.
//
// Live profile switching WITHOUT a restart is available via a local
// control socket (see src/ipc.rs) -- ds4l_gui's tray icon (or any client
// speaking that plain-text protocol) can tell an already-running daemon
// to switch a SPECIFIC controller (by id -- see LIST_CONTROLLERS) to a
// different profile on the fly. This is new, NOT YET verified against a
// live GUI/daemon pair the way the rest of this project was checked
// against real hardware before being trusted -- see ipc.rs's doc comment.

// Multi-controller support: the daemon now enumerates EVERY connected DS4
// v2 (USB and/or Bluetooth) at startup and runs each one on its own
// thread, with its own profile, virtual devices, and controller-hiding
// state -- rather than the earlier single-controller design that opened
// exactly one device and ran one loop in main() itself.
//
// KNOWN LIMITATIONS (deliberate scope cuts for this pass, not oversights):
//   - Enumeration happens ONCE at startup. A controller plugged in after
//     the daemon starts is not picked up without a restart; one that
//     disconnects mid-session will make its thread log read errors and
//     retry forever rather than exiting cleanly. Hot-plug support (via
//     udev monitoring, most likely) is a natural follow-up, not included
//     here.
//   - Every controller starts on the SAME profile name (the single
//     global --profile flag / "Default"). Per-controller startup
//     profiles via the CLI would need something like
//     `--profile ctrl0=Racing --profile ctrl1=Default`, not implemented
//     yet -- for now, use ds4l_gui to switch each controller to a
//     different profile live after they've all connected.
//   - A controller's id is its USB/BT serial number when hidapi exposes
//     one (see `controller_id`'s doc comment for why that's usually
//     stable), falling back to an enumeration-order index ("ctrl0",
//     "ctrl1", ...) otherwise -- which is NOT stable across restarts if
//     controllers happen to connect in a different order next time.

use ds4l::ds4_bt::{self, trigger_full_report_mode};
use ds4l::ds4_input::{
    calibrated_gyro_deg_s, parse_report, send_output_report, OutputReport, PadState, SONY_VID,
};
use ds4l::gamepad_remap;
use ds4l::gyro_stick::{self, GyroStickState};
use ds4l::ipc;
use ds4l::kbm::{self, KbmState, PressedKey};
use ds4l::profile::{self, Ds4FeedbackConfig, OutputMode, Profile};
use ds4l::touchpad::{self, ClickButton, MouseAction, TouchpadMode, TouchpadMouseState};
use ds4l::uinput_absmouse::{self, VirtualAbsMouse};
use ds4l::uinput_ds4::{self, VirtualDs4};
use ds4l::uinput_mouse::{self, VirtualMouse};
use ds4l::uinput_x360::{self, VirtualX360};
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Which transport a controller connected over. Determines how reports
/// are read/parsed (parse_report vs ds4_bt::read_bt_report), but nothing
/// else -- everything downstream of a parsed PadState is identical
/// regardless of connection type, which is exactly why ds4_bt.rs was
/// built to produce the same PadState USB parsing does.
///
/// `Clone, Copy`: needed so a live profile switch (apply_profile_switch)
/// can be handed the connection type by value without fighting the
/// borrow checker over a `&Connection` that's also being matched on in
/// that controller's own loop at the same time.
#[derive(Clone, Copy)]
enum Connection {
    Usb,
    Bluetooth,
}

impl Connection {
    fn label(self) -> &'static str {
        match self {
            Connection::Usb => "USB",
            Connection::Bluetooth => "Bluetooth",
        }
    }
}

/// DS4 v2's Bluetooth PID -- confirmed identical to USB's (0x09CC) when
/// Milestone 8/9 testing connected successfully over BT using this same
/// constant.
const DS4_V2_BT_PID: u16 = 0x09CC;

/// One connected DS4, opened and ready to hand off to its own thread.
struct FoundController {
    id: String,
    device: HidDevice,
    connection: Connection,
    cal: ds4l::ds4_input::GyroCalibration,
    hidraw_path: std::path::PathBuf,
}

/// Stable-ish identifier for a controller: its USB/BT serial number when
/// hidapi exposes one (DS4's USB serial descriptor is commonly its
/// Bluetooth MAC address -- used by other DS4 tooling like ds4drv for
/// exactly this reason, so it's usually present and stable across
/// replugs/reboots), falling back to an enumeration-order index this
/// run assigns ("ctrl0", "ctrl1", ...) when no serial is available.
/// `DeviceInfo::serial_number()` returns `Option<&str>` (confirmed
/// against hidapi 2.6.6's docs), which can be `None` even when a serial
/// exists if the underlying string wasn't valid UTF-8 -- either way,
/// falling back to the index is the safe default.
fn controller_id(info: &hidapi::DeviceInfo, fallback_index: usize) -> String {
    match info.serial_number() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => format!("ctrl{fallback_index}"),
    }
}

/// Enumerates every connected DS4 v2 across both transports (unless
/// `--bluetooth` restricts to Bluetooth-only), SKIPPING any device whose
/// hidraw path is already in `already_running` (a controller this
/// daemon already has a thread for -- opening it a second time would
/// create a competing handle to a device something else is already
/// actively reading from). Returns a FoundController per NEWLY
/// available physical device, ready to be handed to its own thread.
///
/// Called repeatedly by main()'s rescan loop, not just once at startup
/// -- this is what makes hot-plug work: a controller connected after
/// the daemon started is picked up on the next scan, and a controller
/// that reconnects after run_controller detected it disconnecting (see
/// that function's doc comment) gets a fresh FoundController here too,
/// since its old hidraw path will have been removed from
/// `already_running` by then.
///
/// `next_fallback_index` is a PROCESS-LIFETIME counter, not reset per
/// call -- an earlier version reset it to 0 at the start of every
/// enumerate_controllers() call, which was harmless when this only ran
/// once at startup, but would have assigned the SAME fallback id
/// ("ctrl0") to two different no-serial-number controllers discovered
/// in two separate rescans, colliding with a still-running controller's
/// id. Passing an AtomicUsize shared across every call avoids that.
///
/// Filtering logic (usage_page/usage/bus_type checks) is unchanged from
/// the original single-controller `connect()` -- see the FIXED/FIXED
/// (round 2) history preserved below for why each filter exists; only
/// the "pick exactly one" behavior around it changed to "collect all
/// matches."
///
/// FIXED (from the original connect()): `api.open(vid, pid)` matches by
/// VID/PID alone, not transport -- since USB and BT report the SAME
/// VID/PID, that call could silently open a Bluetooth device while the
/// caller assumed USB. Fixed by enumerating via `device_list()` and
/// checking each `DeviceInfo::bus_type()` before opening.
///
/// FIXED (round 2): a DS4 can expose more than one HID
/// interface/collection under the same VID/PID and bus type. Filtering
/// to usage_page=0x01 (Generic Desktop) and usage=0x05 (Game Pad) --
/// the SAME filter DS4Windows itself uses -- picks out just the genuine
/// gamepad interface.
///
/// DEDUPE: if the SAME physical controller happens to be visible on
/// more than one transport at once (matching, non-empty serial
/// numbers), only its USB entry is kept -- preferring USB matches the
/// original single-controller behavior and avoids one physical pad
/// confusingly showing up as two separate entries in ds4l_gui.
/// Controllers with no serial number can't be deduped this way and are
/// trusted to be genuinely distinct (the common case: most setups won't
/// have an unnamed second controller anyway).
fn enumerate_controllers(
    api: &mut HidApi,
    force_bluetooth: bool,
    already_running: &std::collections::HashSet<std::path::PathBuf>,
    next_fallback_index: &std::sync::atomic::AtomicUsize,
) -> Vec<FoundController> {
    // BUGFIX: this was the actual root cause of hot-plug not working at
    // all in either direction. `HidApi::device_list()` returns a CACHED
    // snapshot taken at `HidApi::new()` (or whenever refresh_devices()
    // was last called) -- it does NOT re-scan the system on every call,
    // confirmed against hidapi's own docs ("Object for handling hidapi
    // context... Each instance has its own device list cache"). Without
    // calling refresh_devices() here, every rescan pass kept re-reading
    // the SAME stale list from startup forever: a newly-plugged-in
    // controller literally could not appear (it wasn't in the list
    // taken before it existed), and a controller that had physically
    // disconnected stayed listed with its now-invalid hidraw path,
    // which is exactly why `open_device()` kept failing with "No such
    // file or directory" every single rescan -- the list said the
    // device was still there, but the file backing that path was gone.
    if let Err(e) = api.refresh_devices() {
        eprintln!("Warning: failed to refresh HID device list ({e}) -- using previous scan results.");
    }

    const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
    const USAGE_GAME_PAD: u16 = 0x05;

    let mut matches: Vec<(hidapi::DeviceInfo, Connection)> = Vec::new();
    for info in api.device_list() {
        if info.vendor_id() != SONY_VID {
            continue;
        }
        if info.product_id() != ds4l::ds4_input::DS4_V2_PID && info.product_id() != DS4_V2_BT_PID {
            continue;
        }
        if info.usage_page() != USAGE_PAGE_GENERIC_DESKTOP || info.usage() != USAGE_GAME_PAD {
            continue;
        }
        match info.bus_type() {
            hidapi::BusType::Usb if !force_bluetooth => matches.push((info.clone(), Connection::Usb)),
            hidapi::BusType::Bluetooth => matches.push((info.clone(), Connection::Bluetooth)),
            _ => {}
        }
    }

    // USB entries first, so the dedupe loop below keeps USB when a
    // serial number collides across transports.
    matches.sort_by_key(|(_, connection)| matches!(connection, Connection::Bluetooth));

    let mut seen_serials = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    for (info, connection) in matches {
        if let Some(serial) = info.serial_number() {
            if !serial.is_empty() && !seen_serials.insert(serial.to_string()) {
                continue; // already have this physical controller via another transport
            }
        }
        deduped.push((info, connection));
    }

    let mut found = Vec::new();
    for (info, connection) in deduped {
        let hidraw_path = std::path::PathBuf::from(info.path().to_string_lossy().to_string());

        if already_running.contains(&hidraw_path) {
            continue; // already have a thread running this exact device
        }

        let device = match info.open_device(api) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "Warning: found a DS4 at {} but failed to open it: {e} -- skipping.",
                    hidraw_path.display()
                );
                continue;
            }
        };
        if let Err(e) = device.set_blocking_mode(false) {
            eprintln!(
                "Warning: failed to set non-blocking mode on {}: {e}",
                hidraw_path.display()
            );
        }

        let fallback_index = next_fallback_index.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = controller_id(&info, fallback_index);

        let cal = match connection {
            Connection::Usb => ds4l::ds4_input::read_calibration(&device).unwrap_or_else(|e| {
                eprintln!(
                    "[{id}] Warning: USB calibration read failed ({e}) -- gyro will be uncalibrated."
                );
                ds4l::ds4_input::GyroCalibration::identity()
            }),
            Connection::Bluetooth => {
                println!("[{id}] Triggering BT full-report handshake...");
                if let Err(e) = trigger_full_report_mode(&device) {
                    eprintln!(
                        "[{id}] Warning: BT handshake failed ({e}) -- may stay stuck receiving \
                         only truncated reports with no gyro/touchpad data."
                    );
                }
                ds4_bt::read_calibration_bt(&device).unwrap_or_else(|e| {
                    eprintln!(
                        "[{id}] Warning: BT calibration read failed ({e}) -- gyro will be \
                         uncalibrated (will drift)."
                    );
                    ds4l::ds4_input::GyroCalibration::identity()
                })
            }
        };

        println!("[{id}] Found DS4 v2 on {} bus.", connection.label());
        found.push(FoundController { id, device, connection, cal, hidraw_path });
    }

    found
}

fn parse_args() -> (String, bool, bool) {
    let args: Vec<String> = std::env::args().collect();
    let mut profile_name = "Default".to_string();
    let mut force_bluetooth = false;
    let mut allow_bt_feedback = false;
    for i in 1..args.len() {
        if args[i] == "--profile" {
            if let Some(name) = args.get(i + 1) {
                profile_name = name.clone();
            }
        }
        if args[i] == "--bluetooth" {
            force_bluetooth = true;
        }
        if args[i] == "--bt-feedback" {
            allow_bt_feedback = true;
        }
    }
    (profile_name, force_bluetooth, allow_bt_feedback)
}

/// Converts a dpad's four independent up/down/left/right booleans (as
/// GamepadFrame stores them, post-remap) into the -1/0/1 hat-axis pair
/// uinput's ABS_HAT0X/Y actually wants. Opposite directions active at
/// once (e.g. both up and down mapped from different physical inputs
/// that happened to both fire) cancel to 0 on that axis rather than
/// picking one arbitrarily -- there's no correct "which one wins"
/// answer in that case, so canceling is the least surprising choice.
fn dpad_bools_to_hat(up: bool, down: bool, left: bool, right: bool) -> (i32, i32) {
    let hat_x = match (left, right) {
        (true, false) => -1,
        (false, true) => 1,
        _ => 0,
    };
    let hat_y = match (up, down) {
        (true, false) => -1,
        (false, true) => 1,
        _ => 0,
    };
    (hat_x, hat_y)
}

/// Emits one fully-remapped GamepadFrame to the virtual DS4.
///
/// REDESIGNED: previously took `state: &PadState` and `touchpad_mode`
/// too, to re-emit real touchpad coordinates onto this virtual pad's
/// own multitouch axes when in Passthrough mode. That's gone now -- see
/// touchpad.rs's module doc for why (short version: the Linux kernel's
/// DS4 driver already exposes a fully separate, already-correct
/// touchpad evdev device on its own; re-implementing that here was
/// redundant effort on this project's single least-verified parsing
/// path, for no benefit over just leaving the kernel's own device
/// alone). Touchpad handling for Passthrough mode is now entirely about
/// which sibling device hide_controller.rs excludes from hiding, not
/// anything this function does.
fn emit_gamepad_state(pad: &mut VirtualDs4, frame: &gamepad_remap::GamepadFrame) -> std::io::Result<()> {
    pad.emit_abs(uinput_ds4::ABS_X, frame.left_x as i32)?;
    pad.emit_abs(uinput_ds4::ABS_Y, frame.left_y as i32)?;
    pad.emit_abs(uinput_ds4::ABS_RX, frame.right_x as i32)?;
    pad.emit_abs(uinput_ds4::ABS_RY, frame.right_y as i32)?;
    pad.emit_abs(uinput_ds4::ABS_Z, frame.l2_analog as i32)?;
    pad.emit_abs(uinput_ds4::ABS_RZ, frame.r2_analog as i32)?;

    let (hat_x, hat_y) = dpad_bools_to_hat(frame.dpad_up, frame.dpad_down, frame.dpad_left, frame.dpad_right);
    pad.emit_abs(uinput_ds4::ABS_HAT0X, hat_x)?;
    pad.emit_abs(uinput_ds4::ABS_HAT0Y, hat_y)?;

    pad.emit_key(uinput_ds4::BTN_SOUTH, frame.cross)?;
    pad.emit_key(uinput_ds4::BTN_EAST, frame.circle)?;
    pad.emit_key(uinput_ds4::BTN_NORTH, frame.triangle)?;
    pad.emit_key(uinput_ds4::BTN_WEST, frame.square)?;
    pad.emit_key(uinput_ds4::BTN_TL, frame.l1)?;
    pad.emit_key(uinput_ds4::BTN_TR, frame.r1)?;
    pad.emit_key(uinput_ds4::BTN_TL2, frame.l2_digital)?;
    pad.emit_key(uinput_ds4::BTN_TR2, frame.r2_digital)?;
    pad.emit_key(uinput_ds4::BTN_SELECT, frame.share)?;
    pad.emit_key(uinput_ds4::BTN_START, frame.options)?;
    pad.emit_key(uinput_ds4::BTN_THUMBL, frame.l3)?;
    pad.emit_key(uinput_ds4::BTN_THUMBR, frame.r3)?;
    pad.emit_key(uinput_ds4::BTN_MODE, frame.ps)?;

    pad.sync()
}

/// Xbox 360 equivalent of emit_gamepad_state. Button/dpad logic is
/// IDENTICAL (same evdev codes, see uinput_x360.rs's doc comment on why
/// DS4's cross/circle/square/triangle already line up with Xbox's
/// A/B/X/Y with no remapping needed) -- the only real difference is
/// stick values need rescaling from DS4's 0-255 byte range to Xbox
/// 360's -32768..32767 range via uinput_x360::rescale_stick_axis.
/// frame.l2_digital/r2_digital (GamepadFrame's remapped digital-click
/// bits) have no home here at all: a real 360 pad's triggers don't have
/// a separate digital click, so there's nothing to emit them as -- this
/// mirrors how state.l2_digital/r2_digital had no home here before
/// remapping existed either.
///
/// No touchpad handling -- an Xbox 360 pad has no touchpad axes to put
/// that data on. TouchpadMode::Passthrough is a no-op in Xbox360 output
/// mode as a result (see ds4l_daemon.rs's OutputMode::Xbox360 match arm
/// in run_controller); MouseRemap/AbsoluteMouse still work normally
/// since they target separate pointer devices, independent of which
/// gamepad type is active.
fn emit_x360_state(pad: &mut VirtualX360, frame: &gamepad_remap::GamepadFrame) -> std::io::Result<()> {
    pad.emit_abs(uinput_x360::ABS_X, uinput_x360::rescale_stick_axis(frame.left_x))?;
    pad.emit_abs(uinput_x360::ABS_Y, uinput_x360::rescale_stick_axis(frame.left_y))?;
    pad.emit_abs(uinput_x360::ABS_RX, uinput_x360::rescale_stick_axis(frame.right_x))?;
    pad.emit_abs(uinput_x360::ABS_RY, uinput_x360::rescale_stick_axis(frame.right_y))?;
    pad.emit_abs(uinput_x360::ABS_Z, frame.l2_analog as i32)?;
    pad.emit_abs(uinput_x360::ABS_RZ, frame.r2_analog as i32)?;

    let (hat_x, hat_y) = dpad_bools_to_hat(frame.dpad_up, frame.dpad_down, frame.dpad_left, frame.dpad_right);
    pad.emit_abs(uinput_x360::ABS_HAT0X, hat_x)?;
    pad.emit_abs(uinput_x360::ABS_HAT0Y, hat_y)?;

    pad.emit_key(uinput_x360::BTN_SOUTH, frame.cross)?;
    pad.emit_key(uinput_x360::BTN_EAST, frame.circle)?;
    pad.emit_key(uinput_x360::BTN_NORTH, frame.triangle)?;
    pad.emit_key(uinput_x360::BTN_WEST, frame.square)?;
    pad.emit_key(uinput_x360::BTN_TL, frame.l1)?;
    pad.emit_key(uinput_x360::BTN_TR, frame.r1)?;
    pad.emit_key(uinput_x360::BTN_SELECT, frame.share)?;
    pad.emit_key(uinput_x360::BTN_START, frame.options)?;
    pad.emit_key(uinput_x360::BTN_THUMBL, frame.l3)?;
    pad.emit_key(uinput_x360::BTN_THUMBR, frame.r3)?;
    pad.emit_key(uinput_x360::BTN_MODE, frame.ps)?;

    pad.sync()
}

/// Applies one KbmFrame to the virtual keyboard+mouse device: presses
/// newly-held keys/buttons, releases ones no longer held (diffing against
/// KbmState::prev_pressed), and emits mouse movement. Only sends actual
/// key events for the delta, not the full held set every report, to
/// avoid flooding uinput with redundant repeated key-down events.
fn apply_kbm_frame(
    mouse: &mut VirtualMouse,
    state: &mut KbmState,
    frame: kbm::KbmFrame,
) -> std::io::Result<()> {
    let newly_pressed: Vec<PressedKey> = frame.held.difference(&state.prev_pressed).copied().collect();
    let newly_released: Vec<PressedKey> = state.prev_pressed.difference(&frame.held).copied().collect();

    for key in newly_pressed {
        emit_pressed_key(mouse, key, true)?;
    }
    for key in newly_released {
        emit_pressed_key(mouse, key, false)?;
    }

    if frame.mouse_dx != 0 {
        mouse.emit_rel(uinput_mouse::REL_X, frame.mouse_dx)?;
    }
    if frame.mouse_dy != 0 {
        mouse.emit_rel(uinput_mouse::REL_Y, frame.mouse_dy)?;
    }
    mouse.sync()?;

    state.prev_pressed = frame.held;
    Ok(())
}

fn emit_pressed_key(mouse: &mut VirtualMouse, key: PressedKey, pressed: bool) -> std::io::Result<()> {
    match key {
        PressedKey::Key(code) => mouse.emit_key(code, pressed),
        PressedKey::MouseLeft => mouse.emit_key(uinput_mouse::BTN_LEFT, pressed),
        PressedKey::MouseRight => mouse.emit_key(uinput_mouse::BTN_RIGHT, pressed),
    }
}

/// Creates the virtual device(s) a profile needs: a VirtualDs4 for
/// Gamepad output mode, a VirtualX360 for Xbox360 mode, a VirtualMouse
/// for Kbm mode or for Gamepad/Xbox360 mode with touchpad MouseRemap, a
/// VirtualAbsMouse for Gamepad/Xbox360 mode with touchpad AbsoluteMouse
/// -- any combination, or none at all. Returns an error instead of
/// exiting the process, unlike an earlier inline version of this logic
/// that called `std::process::exit(1)` directly on failure -- that was
/// correct for the ORIGINAL startup-only call site (nothing useful to
/// fall back to if the very first device creation fails), but would be
/// wrong here now that this same logic also runs mid-session on a live
/// profile switch, where a failure should be reported back to the
/// caller (ds4l_gui) while the daemon keeps running its current,
/// still-working profile -- never killed by a switch attempt gone wrong.
fn try_create_virtual_devices(
    profile: &Profile,
) -> Result<
    (
        Option<VirtualDs4>,
        Option<VirtualX360>,
        Option<VirtualMouse>,
        Option<VirtualAbsMouse>,
    ),
    String,
> {
    let virtual_pad = if profile.output_mode == OutputMode::Gamepad {
        Some(VirtualDs4::create().map_err(|e| format!("failed to create virtual DS4: {e}"))?)
    } else {
        None
    };

    let virtual_x360 = if profile.output_mode == OutputMode::Xbox360 {
        Some(
            VirtualX360::create()
                .map_err(|e| format!("failed to create virtual Xbox 360 pad: {e}"))?,
        )
    } else {
        None
    };

    let uses_gamepad_style_output =
        profile.output_mode == OutputMode::Gamepad || profile.output_mode == OutputMode::Xbox360;

    let needs_mouse_device = profile.output_mode == OutputMode::Kbm
        || (uses_gamepad_style_output && profile.touchpad.mode == TouchpadMode::MouseRemap);
    let virtual_mouse = if needs_mouse_device {
        let extra_keys: Vec<u16> = if profile.output_mode == OutputMode::Kbm {
            collect_mapped_keys(&profile.kbm)
        } else {
            Vec::new()
        };
        Some(
            VirtualMouse::create(&extra_keys)
                .map_err(|e| format!("failed to create virtual mouse: {e}"))?,
        )
    } else {
        None
    };

    let needs_absmouse_device = profile.touchpad.mode == TouchpadMode::AbsoluteMouse
        && (uses_gamepad_style_output || profile.output_mode == OutputMode::Kbm);
    let virtual_absmouse = if needs_absmouse_device {
        Some(
            VirtualAbsMouse::create()
                .map_err(|e| format!("failed to create virtual absolute-position pointer: {e}"))?,
        )
    } else {
        None
    };

    Ok((virtual_pad, virtual_x360, virtual_mouse, virtual_absmouse))
}

/// Low battery threshold (percent, inclusive) below which
/// low_battery_flash (when enabled per-profile) starts flashing the
/// lightbar. Not a DS4Windows byte-for-byte value lookup -- chosen as a
/// conventional, sensible "should probably charge soon" cutoff, same as
/// most devices' default low-battery warnings.
const LOW_BATTERY_THRESHOLD_PERCENT: u8 = 20;

/// How often the lightbar toggles between the flash color and off while
/// actively flashing, in milliseconds. Slow enough to be an obvious,
/// unhurried warning rather than a seizure-inducing strobe, fast enough
/// to be clearly a deliberate pattern rather than "did the color just
/// glitch."
const BATTERY_FLASH_INTERVAL_MS: u64 = 500;

/// Full color-wheel rotation period for rainbow mode, in seconds --
/// chosen as a pleasant, unhurried cycle speed matching typical RGB
/// peripheral defaults, not tied to any DS4Windows-specific value.
const RAINBOW_CYCLE_SECONDS: f64 = 6.0;

/// How often rainbow mode actually sends a new color, in milliseconds.
/// 20 updates/sec is smooth to the eye without writing an output report
/// on every single ~4ms input report (250Hz USB) for no visible benefit.
const RAINBOW_UPDATE_INTERVAL_MS: u64 = 50;

/// Per-controller state for lightbar effects (low-battery flash and
/// rainbow), carried in run_controller's loop alongside gyro_state/
/// touchpad_mouse_state/kbm_state. Tracks whether each effect is
/// CURRENTLY overriding the lightbar (so the real configured color can
/// be restored exactly once when an effect stops, rather than every
/// single loop iteration) plus each effect's own timing/phase state.
#[derive(Default)]
struct LightbarEffectState {
    battery_flash_active: bool,
    battery_flash_on: bool,
    last_battery_toggle: Option<std::time::Instant>,
    rainbow_active: bool,
    rainbow_hue: f64,
    last_rainbow_update: Option<std::time::Instant>,
}

/// Sends just the lightbar color, no rumble -- the piece of
/// apply_feedback's logic the battery-flash loop needs on its own
/// (every ~500ms while flashing is far too often to also repeat a
/// rumble pulse each time). Both apply_feedback and the flash logic
/// funnel through this single function so there's exactly one place
/// that knows how to address USB vs Bluetooth for an LED-only report.
fn set_lightbar(device: &HidDevice, connection: Connection, color: ds4l::profile::LightbarColor) {
    let report = OutputReport {
        led_red: color.red,
        led_green: color.green,
        led_blue: color.blue,
        set_led: true,
        ..Default::default()
    };
    let result = match connection {
        Connection::Usb => send_output_report(device, &report),
        Connection::Bluetooth => ds4_bt::send_output_report_bt(device, &report),
    };
    if let Err(e) = result {
        eprintln!("Warning: failed to update lightbar: {e}");
    }
}

/// Sends the profile's lightbar color and, if enabled, a brief rumble
/// pulse. Factored out of main() so both the initial connect AND a live
/// profile switch (ds4l_gui) reuse the exact same, already hardware-
/// verified-and-bugfixed logic rather than risking the two paths drift
/// apart. See the BUGFIX note below -- unchanged from the original,
/// this is load-bearing, not decorative.
fn apply_feedback(device: &HidDevice, connection: Connection, feedback: &Ds4FeedbackConfig) {
    let lb = feedback.lightbar;

    // BUGFIX: every output report below carries the current lightbar
    // color, even reports whose purpose is only rumble. Root cause:
    // this DS4's firmware appears to apply the LED RGB bytes
    // unconditionally, regardless of whether the LED bit in valid_flag0
    // is set -- a rumble-only report built with led fields left at 0
    // (the natural way to write "don't touch the LED") silently blanks
    // the lightbar a moment after it was set. Matches a documented
    // kernel quirk (a 2024 hid-playstation patch: "some 3rd party
    // gamepads expect updates to rumble and lightbar together, and
    // setting one may cancel the other"). Fix: always resend the
    // intended color alongside rumble.
    set_lightbar(device, connection, lb);

    if feedback.rumble_on_load {
        let pulse_on = OutputReport {
            rumble_weak: 150,
            rumble_strong: 150,
            set_rumble: true,
            led_red: lb.red,
            led_green: lb.green,
            led_blue: lb.blue,
            set_led: true,
            ..Default::default()
        };
        let pulse_on_result = match connection {
            Connection::Usb => send_output_report(device, &pulse_on),
            Connection::Bluetooth => ds4_bt::send_output_report_bt(device, &pulse_on),
        };
        if let Err(e) = pulse_on_result {
            eprintln!("Warning: failed to start rumble pulse: {e}");
        }
        std::thread::sleep(Duration::from_millis(250));
        let pulse_off = OutputReport {
            rumble_weak: 0,
            rumble_strong: 0,
            set_rumble: true,
            led_red: lb.red,
            led_green: lb.green,
            led_blue: lb.blue,
            set_led: true,
            ..Default::default()
        };
        let pulse_off_result = match connection {
            Connection::Usb => send_output_report(device, &pulse_off),
            Connection::Bluetooth => ds4_bt::send_output_report_bt(device, &pulse_off),
        };
        if let Err(e) = pulse_off_result {
            eprintln!("Warning: failed to stop rumble pulse: {e}");
        }
    }
}

/// Standard HSV-to-RGB conversion (s and v fixed at 1.0 by every call
/// site here, i.e. always fully saturated/full brightness -- s and v
/// params kept anyway since the formula needs them and a future variant
/// might want less-than-full brightness). Well-established, deterministic
/// formula -- not something needing hardware verification the way
/// protocol offsets do; the only thing worth double-checking is that
/// the resulting color actually reaches the controller correctly, which
/// is set_lightbar's job, not this function's.
fn hsv_to_rgb(h: f64, s: f64, v: f64) -> ds4l::profile::LightbarColor {
    let h = h.rem_euclid(360.0);
    let c = v * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match h_prime as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    ds4l::profile::LightbarColor {
        red: ((r1 + m) * 255.0).round() as u8,
        green: ((g1 + m) * 255.0).round() as u8,
        blue: ((b1 + m) * 255.0).round() as u8,
    }
}

/// Checks the current battery reading and rainbow setting against the
/// profile's feedback config and updates the lightbar accordingly.
/// PRECEDENCE: a low-battery warning always wins over rainbow cycling
/// -- safety-relevant information shouldn't be visually competed with
/// by a color cycle. Called once per report in run_controller's loop --
/// cheap when nothing needs to change (Instant comparisons, no I/O) so
/// it doesn't meaningfully affect the hot path even though it now
/// covers two effects instead of one.
fn update_lightbar_effects(
    state: &mut LightbarEffectState,
    device: &HidDevice,
    connection: Connection,
    profile: &Profile,
    pad: &PadState,
) {
    let low = profile.feedback.low_battery_flash
        && pad.battery_percent <= LOW_BATTERY_THRESHOLD_PERCENT
        && !pad.battery_charging;

    if low {
        // Battery flash preempts rainbow outright -- no explicit
        // "restore" needed for rainbow here, since the flash logic
        // below immediately takes over painting the lightbar; rainbow
        // just resumes naturally once `low` goes false again, same as
        // it would after being freshly enabled.
        state.rainbow_active = false;

        let now = std::time::Instant::now();
        let due = state
            .last_battery_toggle
            .map(|t| now.duration_since(t) >= Duration::from_millis(BATTERY_FLASH_INTERVAL_MS))
            .unwrap_or(true); // first tick since entering the low state

        if due {
            state.battery_flash_on = !state.battery_flash_on;
            state.last_battery_toggle = Some(now);
            let color = if state.battery_flash_on {
                ds4l::profile::LightbarColor { red: 255, green: 0, blue: 0 }
            } else {
                ds4l::profile::LightbarColor { red: 0, green: 0, blue: 0 }
            };
            set_lightbar(device, connection, color);
        }
        state.battery_flash_active = true;
        return;
    }

    if state.battery_flash_active {
        // Just exited the low-battery state (recovered, plugged in to
        // charge, or the profile turned the feature off mid-flash) --
        // restore the real configured color exactly once rather than
        // leaving the lightbar stuck off/red. Rainbow (if enabled)
        // resumes on its own below, since we don't return here.
        set_lightbar(device, connection, profile.feedback.lightbar);
        state.battery_flash_active = false;
        state.battery_flash_on = false;
        state.last_battery_toggle = None;
    }

    if profile.feedback.rainbow {
        let now = std::time::Instant::now();
        let due = state
            .last_rainbow_update
            .map(|t| now.duration_since(t) >= Duration::from_millis(RAINBOW_UPDATE_INTERVAL_MS))
            .unwrap_or(true);

        if due {
            // Advance hue by actual elapsed wall-clock time rather than
            // a fixed step per call, so rotation speed stays consistent
            // regardless of report rate (USB ~250Hz vs BT ~98Hz) or
            // scheduling jitter -- 0.0 elapsed on the very first tick
            // (last_rainbow_update is None) just paints hue 0 (red)
            // without advancing yet, which is a fine starting point.
            let elapsed = state
                .last_rainbow_update
                .map(|t| now.duration_since(t).as_secs_f64())
                .unwrap_or(0.0);
            state.rainbow_hue = (state.rainbow_hue + 360.0 * elapsed / RAINBOW_CYCLE_SECONDS) % 360.0;
            state.last_rainbow_update = Some(now);
            set_lightbar(device, connection, hsv_to_rgb(state.rainbow_hue, 1.0, 1.0));
        }
        state.rainbow_active = true;
    } else if state.rainbow_active {
        // Rainbow just got turned off -- restore the real configured
        // color exactly once.
        set_lightbar(device, connection, profile.feedback.lightbar);
        state.rainbow_active = false;
        state.last_rainbow_update = None;
    }
}

/// Handles one SWITCH_PROFILE request relayed from ipc.rs's control
/// socket: loads the named profile from disk, and only if EVERYTHING
/// needed to run it succeeds does it replace the live profile, virtual
/// devices, and per-session state -- a failed switch must never leave
/// the daemon half-migrated between two profiles.
///
/// KNOWN LIMITATION (deliberate, documented rather than silently
/// accepted): this always tears down and recreates BOTH virtual devices
/// on every switch, even when the new profile needs exactly the same
/// device shape as the old one (e.g. switching between two Gamepad-mode
/// profiles that only differ in gyro sensitivity). The alternative --
/// diffing whether the new profile's device requirements, and for Kbm
/// mode specifically its exact registered key set, actually differ from
/// what's currently created -- adds real complexity for a case
/// (profile switching) that's a deliberate, infrequent user action, not
/// a hot path. Recreating unconditionally is trivially correct instead:
/// VirtualDs4/VirtualMouse's existing Drop impls already destroy their
/// uinput nodes cleanly, so `*virtual_pad = new_pad` below just works.
/// Cost: this makes every switch take ~500-700ms (VirtualDs4::create's
/// 500ms + VirtualMouse::create's 200ms settle sleeps, when both are
/// needed) plus another ~250ms if the new profile has rumble_on_load
/// enabled -- up to ~1s, during which the main loop (and therefore
/// input processing) is blocked. Bounded and expected for a deliberate
/// switch, not a hang; ipc.rs's SWITCH_PROFILE reply wait is sized with
/// that in mind. Revisit if this ever feels too slow in practice.
#[allow(clippy::too_many_arguments)]
/// Computes which sibling device name-suffixes hide_controller.rs
/// should leave visible when hiding this profile's controller -- see
/// hide_controller.rs's module doc for exactly which kernel devices
/// these suffixes correspond to (" Touchpad", " Motion Sensors") and
/// why this matters. Returns an empty Vec (hide everything, the
/// original behavior) when neither passthrough option is on.
fn passthrough_exclusions(profile: &Profile) -> Vec<&'static str> {
    let mut exclusions = Vec::new();
    if profile.touchpad.mode == TouchpadMode::Passthrough {
        exclusions.push(" Touchpad");
    }
    if profile.gyro_passthrough {
        exclusions.push(" Motion Sensors");
    }
    exclusions
}

fn apply_profile_switch(
    controller_id: &str,
    name: &str,
    profile: &mut Profile,
    virtual_pad: &mut Option<VirtualDs4>,
    virtual_x360: &mut Option<VirtualX360>,
    virtual_mouse: &mut Option<VirtualMouse>,
    virtual_absmouse: &mut Option<VirtualAbsMouse>,
    gyro_state: &mut GyroStickState,
    touchpad_mouse_state: &mut TouchpadMouseState,
    kbm_state: &mut KbmState,
    hidden_controller: &Arc<Mutex<Option<ds4l::hide_controller::HiddenController>>>,
    device: &HidDevice,
    connection: Connection,
    hidraw_path: &std::path::Path,
    send_feedback: bool,
    status: &Arc<Mutex<ipc::StatusSnapshot>>,
) -> Result<(), String> {
    let new_profile =
        profile::load(name).map_err(|e| format!("failed to load profile \"{name}\": {e}"))?;

    // Build the new virtual devices FIRST -- this is the step in this
    // function most likely to fail (permissions, uinput resource
    // limits), and doing it before touching any existing state means a
    // failure here leaves the old profile/devices completely untouched.
    let (new_pad, new_x360, new_mouse, new_absmouse) = try_create_virtual_devices(&new_profile)?;

    // Reconcile controller-hiding. REVISED: previously this only acted
    // when hide_real_controller's BOOLEAN changed, on the assumption
    // that "nothing to reconcile" otherwise. That assumption broke once
    // passthrough_exclusions() existed -- hide_real_controller can stay
    // TRUE across a switch while touchpad.mode or gyro_passthrough
    // changes, which changes WHICH exclusions should apply even though
    // the boolean itself didn't move (e.g. switching from a Passthrough
    // profile to a MouseRemap profile, both with hiding on, needs to
    // start hiding the Touchpad sibling device it was previously
    // excluding). Simplest correct fix: whenever the NEW profile wants
    // hiding at all, unconditionally restore-then-rehide with freshly
    // computed exclusions -- a redundant chmod cycle in the common case
    // where nothing relevant actually changed is a small, one-time cost
    // for a switch that's already paying a much larger one (uinput
    // device recreation, up to ~1s -- see this function's own doc
    // comment above), not a hot path worth optimizing around.
    *hidden_controller.lock().unwrap() = None; // restore whatever was hidden, if anything
    if new_profile.hide_real_controller {
        let exclusions = passthrough_exclusions(&new_profile);
        match ds4l::hide_controller::HiddenController::hide(hidraw_path, &exclusions) {
            Ok(guard) => *hidden_controller.lock().unwrap() = Some(guard),
            Err(e) => eprintln!(
                "[{controller_id}] Warning: switched to profile \"{name}\" but failed to \
                 hide the real controller: {e}"
            ),
        }
    }

    // Nothing past this point can fail -- safe to commit the swap. The
    // OLD virtual_pad/virtual_x360/virtual_mouse/virtual_absmouse Drop
    // here (destroying their uinput nodes) the moment they're
    // overwritten.
    *virtual_pad = new_pad;
    *virtual_x360 = new_x360;
    *virtual_mouse = new_mouse;
    *virtual_absmouse = new_absmouse;
    *profile = new_profile;

    // Fresh per-session state for the new profile: gyro smoothing/
    // toggle-latch, touchpad delta baseline, and kbm's held-key set are
    // all meaningless -- or actively wrong, e.g. a stale toggle_active
    // silently reactivating gyro under a different profile's totally
    // different sensitivity -- if carried over from the old profile.
    *gyro_state = GyroStickState::default();
    *touchpad_mouse_state = TouchpadMouseState::default();
    *kbm_state = KbmState::default();

    if send_feedback {
        apply_feedback(device, connection, &profile.feedback);
    }

    // Battery fields aren't reset here -- they're not tied to which
    // profile is active, and the main loop overwrites them from the
    // very next report anyway. Resetting to 0/false here would just
    // cause a misleading flicker to "0%, not charging" for the brief
    // window until that next report arrives.
    {
        let mut s = status.lock().unwrap();
        s.profile_name = profile.name.clone();
        s.output_mode = format!("{:?}", profile.output_mode);
        s.connection = connection.label().to_string();
        s.hidden = hidden_controller.lock().unwrap().is_some();
    }

    println!("[{controller_id}] Switched to profile \"{}\" via control socket.", profile.name);
    Ok(())
}

/// Runs one controller for the life of its CONNECTION (not necessarily
/// the life of the process anymore): loads its starting profile,
/// registers itself in the shared control-socket Registry, creates its
/// virtual devices, and loops reading/emitting input -- almost exactly
/// the OLD single-controller main()'s body, unchanged in its per-report
/// logic, just parameterized so N of these can run concurrently (one
/// per spawned thread, one per physical controller).
///
/// DISCONNECT HANDLING (new): after DISCONNECT_ERROR_THRESHOLD
/// consecutive read errors, this function concludes the controller has
/// disconnected, restores any hidden-controller permissions, removes
/// itself from the registry and from `running_paths`, and RETURNS --
/// ending this thread cleanly (virtual devices and the hidraw handle
/// Drop normally as locals go out of scope) rather than spinning
/// forever printing errors. main()'s rescan loop picks the physical
/// controller back up automatically on reconnect, with no restart
/// needed. An earlier version had no such exit path at all: a
/// disconnected controller's thread ran forever, permissions (if
/// hidden) were never restored until the WHOLE daemon exited, and
/// reconnecting the same controller did nothing until a manual restart.
#[allow(clippy::too_many_arguments)]
fn run_controller(
    controller_id: String,
    device: HidDevice,
    connection: Connection,
    cal: ds4l::ds4_input::GyroCalibration,
    hidraw_path: std::path::PathBuf,
    initial_profile_name: String,
    allow_bt_feedback: bool,
    registry: ipc::Registry,
    hidden_controller: Arc<Mutex<Option<ds4l::hide_controller::HiddenController>>>,
    running_paths: Arc<Mutex<std::collections::HashSet<std::path::PathBuf>>>,
) {
    println!("[{controller_id}] Loading profile \"{initial_profile_name}\"...");
    let mut profile: Profile = profile::load(&initial_profile_name).unwrap_or_else(|e| {
        eprintln!(
            "[{controller_id}] Failed to load profile \"{initial_profile_name}\": {e}\n\
             Falling back to built-in defaults for this run (not saved)."
        );
        Profile {
            name: initial_profile_name.clone(),
            ..Profile::default()
        }
    });
    println!(
        "[{controller_id}] Profile loaded: output mode={:?}, gyro mode={:?} \
         sensitivity={:.0}deg/s, touchpad mode={:?}",
        profile.output_mode, profile.gyro.mode, profile.gyro.deg_per_sec_at_full_stick, profile.touchpad.mode
    );
    if let Ok(path) = profile::profiles_dir() {
        println!(
            "[{controller_id}] (Edit {} directly and restart, or use ds4l_gui to edit and \
             hot-switch this controller's profile live.)",
            path.join(format!("{initial_profile_name}.toml")).display()
        );
    }

    // Controller hiding: restricts this specific controller's device
    // nodes from other processes (Steam, games) while this thread runs,
    // restoring original permissions on exit -- including Ctrl+C AND
    // `systemctl stop`/plain `kill` (SIGTERM), via the process-wide
    // ctrlc handler installed once in main() (it iterates every
    // controller's hidden_controller guard, not just this one). Opt-in
    // per profile (hide_real_controller), off by default. See
    // passthrough_exclusions() for which sibling devices stay visible
    // even while hidden, based on touchpad.mode/gyro_passthrough.
    if profile.hide_real_controller {
        let exclusions = passthrough_exclusions(&profile);
        match ds4l::hide_controller::HiddenController::hide(&hidraw_path, &exclusions) {
            Ok(guard) => {
                *hidden_controller.lock().unwrap() = Some(guard);
            }
            Err(e) => {
                eprintln!(
                    "[{controller_id}] Warning: failed to hide real controller: {e}\n\
                     Continuing without hiding -- other processes may still see the real controller."
                );
            }
        }
    }

    // Register with the shared control socket so ds4l_gui (or anything
    // else speaking ipc.rs's protocol) can query this controller's
    // status and hot-switch its profile by this controller_id.
    let status = Arc::new(Mutex::new(ipc::StatusSnapshot {
        profile_name: profile.name.clone(),
        output_mode: format!("{:?}", profile.output_mode),
        connection: connection.label().to_string(),
        hidden: hidden_controller.lock().unwrap().is_some(),
        battery_percent: 0,
        battery_charging: false,
    }));
    let (cmd_tx, control_rx) = std::sync::mpsc::channel::<ipc::PendingCommand>();
    registry.lock().unwrap().insert(
        controller_id.clone(),
        ipc::ControllerHandle {
            cmd_tx,
            status: status.clone(),
        },
    );

    // LED/rumble implemented for both USB and Bluetooth; BT support
    // carries a documented real risk (see ds4_bt::send_output_report_bt's
    // doc comment): a malformed BT output report has been reported to
    // silently stop the controller from streaming full input reports
    // until reconnected -- not just "rumble won't work," potentially
    // "input breaks too." Given that, BT LED/rumble is gated behind
    // --bt-feedback rather than firing automatically on every BT
    // connection, so a bad first test doesn't silently break your input
    // mid-session without you expecting it. This flag is a daemon-
    // startup setting (not per-profile, not per-controller), so it
    // applies the same way to every profile switch on every controller
    // for the life of this process.
    let send_feedback = matches!(connection, Connection::Usb) || allow_bt_feedback;
    if send_feedback {
        apply_feedback(&device, connection, &profile.feedback);
    } else {
        println!(
            "[{controller_id}] (LED/rumble on load skipped over Bluetooth -- pass --bt-feedback \
             to enable; see source comments for a documented risk with this before enabling.)"
        );
    }

    let (mut virtual_pad, mut virtual_x360, mut virtual_mouse, mut virtual_absmouse) =
        match try_create_virtual_devices(&profile) {
            Ok(devices) => devices,
            Err(e) => {
                // FIXED: this used to be `.unwrap_or_else(|e| {
                // eprintln!(...); std::process::exit(1); })`, which took
                // down the ENTIRE daemon -- every other already-running
                // controller included -- over a single controller
                // failing to create its virtual devices (e.g. a
                // transient /dev/uinput permission hiccup). Now it just
                // gives up on THIS controller: cleans up its own
                // tracking entries and returns, ending only this
                // thread. main()'s rescan loop will try again from
                // scratch next pass since this hidraw path is no longer
                // in running_paths -- a transient failure gets retried
                // automatically instead of requiring a full daemon
                // restart.
                eprintln!("[{controller_id}] {e}\nCheck /dev/uinput permissions.");
                registry.lock().unwrap().remove(&controller_id);
                running_paths.lock().unwrap().remove(&hidraw_path);
                return;
            }
        };
    println!(
        "[{controller_id}] Virtual device(s) created. Running with profile \"{}\".",
        profile.name
    );

    let mut gyro_state = GyroStickState::default();
    let mut touchpad_mouse_state = TouchpadMouseState::default();
    let mut kbm_state = KbmState::default();
    let mut lightbar_effect_state = LightbarEffectState::default();

    // Consecutive READ ERROR counter (not "no new data this cycle" --
    // that's the normal idle case and treating it as suspicious would
    // risk false-positives I can't fully rule out without independently
    // confirming exactly how continuously a DS4 streams reports at rest;
    // an actual I/O error, by contrast, is a specific, unambiguous
    // signal something is wrong). Each error already sleeps 500ms (see
    // below), so DISCONNECT_ERROR_THRESHOLD consecutive errors means
    // ~5s of confirmed, continuous failure before concluding the
    // controller is gone -- long enough to ride out a transient BT
    // radio glitch, short enough that a real unplug is noticed quickly.
    const DISCONNECT_ERROR_THRESHOLD: u32 = 10;
    let mut consecutive_errors: u32 = 0;

    let mut buf = [0u8; 128]; // sized for BT's 78-byte report; USB's 64-byte report fits too
    loop {
        // Non-blocking poll for control-socket commands (profile
        // switches from ds4l_gui), once per iteration -- see ipc.rs for
        // the protocol. device.read_timeout() below already blocks up
        // to 100ms per iteration, so a requested switch takes effect
        // within ~100ms: plenty responsive for a deliberate user
        // action, and try_recv() never blocks, so this adds no latency
        // to the hot input-read path on iterations with no command
        // pending (the overwhelming majority of them).
        while let Ok(cmd) = control_rx.try_recv() {
            match cmd {
                ipc::PendingCommand::SwitchProfile { name, reply } => {
                    let result = apply_profile_switch(
                        &controller_id,
                        &name,
                        &mut profile,
                        &mut virtual_pad,
                        &mut virtual_x360,
                        &mut virtual_mouse,
                        &mut virtual_absmouse,
                        &mut gyro_state,
                        &mut touchpad_mouse_state,
                        &mut kbm_state,
                        &hidden_controller,
                        &device,
                        connection,
                        &hidraw_path,
                        send_feedback,
                        &status,
                    );
                    // Ignore a send failure here: it only means the
                    // requesting client already gave up/disconnected
                    // (e.g. hit ipc.rs's own 5s timeout) -- the switch
                    // itself still applied successfully to this
                    // controller either way.
                    let _ = reply.send(result);
                }
            }
        }

        let maybe_state: Option<PadState> = match connection {
            Connection::Usb => match device.read_timeout(&mut buf, 100) {
                Ok(len) if len >= 25 && buf[0] == 0x01 => {
                    consecutive_errors = 0;
                    Some(parse_report(&buf))
                }
                Ok(_) => {
                    consecutive_errors = 0; // a successful (if short/empty) read -- device still responding
                    None
                }
                Err(e) => {
                    consecutive_errors += 1;
                    eprintln!(
                        "\n[{controller_id}] USB read error ({consecutive_errors}/{DISCONNECT_ERROR_THRESHOLD}): {e}"
                    );
                    std::thread::sleep(Duration::from_millis(500));
                    None
                }
            },
            Connection::Bluetooth => match ds4_bt::read_bt_report(&device, &mut buf) {
                Ok(state) => {
                    consecutive_errors = 0;
                    state
                }
                Err(e) => {
                    consecutive_errors += 1;
                    eprintln!(
                        "\n[{controller_id}] BT read error ({consecutive_errors}/{DISCONNECT_ERROR_THRESHOLD}): {e}"
                    );
                    std::thread::sleep(Duration::from_millis(500));
                    None
                }
            },
        };

        if consecutive_errors >= DISCONNECT_ERROR_THRESHOLD {
            println!(
                "\n[{controller_id}] Controller appears to have disconnected ({DISCONNECT_ERROR_THRESHOLD} \
                 consecutive read errors) -- shutting down this controller's thread. It will be \
                 picked up automatically on reconnect (no daemon restart needed)."
            );
            // Restore hidden-controller permissions (if this profile
            // had hiding enabled) before this thread exits -- otherwise
            // a controller that disconnects while hidden would stay
            // hidden until the WHOLE daemon exits, not just this one
            // controller's session.
            *hidden_controller.lock().unwrap() = None;
            registry.lock().unwrap().remove(&controller_id);
            running_paths.lock().unwrap().remove(&hidraw_path);
            // virtual_pad/virtual_x360/virtual_mouse/virtual_absmouse
            // and `device` itself all Drop normally as this function
            // returns -- uinput nodes destroyed, hidraw handle closed,
            // no explicit cleanup needed beyond what's above.
            return;
        }

        let state = match maybe_state {
            Some(s) => s,
            None => continue,
        };

        // Keep the shared status snapshot's battery fields current --
        // unlike profile_name/output_mode/connection/hidden (which only
        // change on a profile switch), battery drains continuously on
        // its own, so this updates every report rather than only at
        // switch time. Cheap: an uncontended mutex lock plus two field
        // writes, negligible next to the per-report work already
        // happening here.
        {
            let mut s = status.lock().unwrap();
            s.battery_percent = state.battery_percent;
            s.battery_charging = state.battery_charging;
        }

        if send_feedback {
            update_lightbar_effects(&mut lightbar_effect_state, &device, connection, &profile, &state);
        }

        match profile.output_mode {
            OutputMode::Kbm => {
                if let Some(mouse) = virtual_mouse.as_mut() {
                    let frame = kbm::compute_frame(&state, &profile.kbm, &mut kbm_state);
                    if let Err(e) = apply_kbm_frame(mouse, &mut kbm_state, frame) {
                        eprintln!("\n[{controller_id}] failed to emit KBM frame: {e}");
                    }
                }

                // BUGFIX: this call was missing entirely -- Kbm mode
                // never processed touchpad MouseRemap/AbsoluteMouse at
                // all, so only the right stick's own StickKbmMode::Mouse
                // ever drove the cursor. Both now compose correctly:
                // the stick and the touchpad target the same virtual
                // mouse device (or, for AbsoluteMouse, the separate abs-
                // pointer device), and their contributions simply add
                // up within the same report the same way two physical
                // pointing devices moving at once would.
                handle_touchpad_pointer_output(
                    &mut virtual_mouse,
                    &mut virtual_absmouse,
                    &mut touchpad_mouse_state,
                    &profile,
                    &state,
                    &controller_id,
                );
            }
            OutputMode::Gamepad => {
                let gyro = calibrated_gyro_deg_s(&state, &cal);

                let (gdx, gdy) = gyro_stick::compute_gyro_stick_delta(
                    &mut gyro_state,
                    &profile.gyro,
                    &state,
                    gyro.yaw,
                    gyro.pitch,
                );

                // Full button/stick/trigger remapping happens here --
                // see gamepad_remap.rs's module doc for the model.
                // Gyro still blends onto the FINAL output right stick
                // AFTER remapping (not the physical right stick before
                // it), so "gyro augments the right stick" keeps that
                // exact meaning even if right_stick_analog has been
                // reconfigured to pull from the physical left stick --
                // a deliberate choice, documented in gamepad_remap.rs.
                let mut frame = gamepad_remap::compute_gamepad_frame(&state, &profile.gamepad_remap);
                let (final_rx, final_ry) =
                    gyro_stick::blend_and_clamp(frame.right_x, frame.right_y, gdx, gdy);
                frame.right_x = final_rx;
                frame.right_y = final_ry;

                if let Some(pad) = virtual_pad.as_mut() {
                    if let Err(e) = emit_gamepad_state(pad, &frame) {
                        eprintln!("\n[{controller_id}] failed to emit gamepad state: {e}");
                    }
                }

                handle_touchpad_pointer_output(
                    &mut virtual_mouse,
                    &mut virtual_absmouse,
                    &mut touchpad_mouse_state,
                    &profile,
                    &state,
                    &controller_id,
                );
            }
            OutputMode::Xbox360 => {
                let gyro = calibrated_gyro_deg_s(&state, &cal);

                let (gdx, gdy) = gyro_stick::compute_gyro_stick_delta(
                    &mut gyro_state,
                    &profile.gyro,
                    &state,
                    gyro.yaw,
                    gyro.pitch,
                );

                // Same remap-then-gyro-blend order as Gamepad mode
                // above -- gamepad_remap.rs's frame is transport/output-
                // type agnostic, which is exactly why one remap config
                // serves both Gamepad and Xbox360 output.
                let mut frame = gamepad_remap::compute_gamepad_frame(&state, &profile.gamepad_remap);
                let (final_rx, final_ry) =
                    gyro_stick::blend_and_clamp(frame.right_x, frame.right_y, gdx, gdy);
                frame.right_x = final_rx;
                frame.right_y = final_ry;

                if let Some(pad) = virtual_x360.as_mut() {
                    if let Err(e) = emit_x360_state(pad, &frame) {
                        eprintln!("\n[{controller_id}] failed to emit Xbox 360 state: {e}");
                    }
                }

                // TouchpadMode::Passthrough is a no-op here -- an Xbox
                // 360 pad has no touchpad axes to pass anything through
                // to (see emit_x360_state's doc comment). MouseRemap and
                // AbsoluteMouse still work normally: both target a
                // separate pointer device regardless of which gamepad
                // type is active.
                handle_touchpad_pointer_output(
                    &mut virtual_mouse,
                    &mut virtual_absmouse,
                    &mut touchpad_mouse_state,
                    &profile,
                    &state,
                    &controller_id,
                );
            }
        }
    }
}

/// Dispatches touchpad output to whichever pointer device (if any) the
/// profile's touchpad mode actually needs. TouchpadMode::Passthrough
/// isn't handled here at all -- it targets the GAMEPAD's own touch
/// axes (see emit_gamepad_state), not a separate pointer device, so
/// there's nothing for this function to do in that case.
fn handle_touchpad_pointer_output(
    virtual_mouse: &mut Option<VirtualMouse>,
    virtual_absmouse: &mut Option<VirtualAbsMouse>,
    touchpad_mouse_state: &mut TouchpadMouseState,
    profile: &Profile,
    state: &PadState,
    controller_id: &str,
) {
    match profile.touchpad.mode {
        TouchpadMode::MouseRemap => {
            if let Some(mouse) = virtual_mouse.as_mut() {
                handle_touchpad_mouse_remap(mouse, touchpad_mouse_state, profile, state, controller_id);
            }
        }
        TouchpadMode::AbsoluteMouse => {
            if let Some(abs) = virtual_absmouse.as_mut() {
                handle_touchpad_absolute_mouse(abs, state, controller_id);
            }
        }
        TouchpadMode::Passthrough | TouchpadMode::Disabled => {}
    }
}

/// AbsoluteMouse handling: forwards the touchpad's raw position 1:1
/// (uinput_absmouse.rs's ABS_X/ABS_Y range already matches the
/// touchpad's native resolution exactly, so no rescaling is needed --
/// see that module's doc comment), and applies the same 1-finger-left/
/// 2-finger-right click convention MouseRemap uses, for consistency
/// within this project.
fn handle_touchpad_absolute_mouse(abs: &mut VirtualAbsMouse, state: &PadState, controller_id: &str) {
    if let touchpad::AbsoluteMouseAction::Move { x, y } =
        touchpad::compute_absolute_mouse_action(&state.finger1)
    {
        let move_result = abs
            .emit_abs(uinput_absmouse::ABS_X, x)
            .and_then(|_| abs.emit_abs(uinput_absmouse::ABS_Y, y))
            .and_then(|_| abs.sync());
        if let Err(e) = move_result {
            eprintln!("\n[{controller_id}] failed to emit absolute pointer position: {e}");
        }
    }

    let finger_count = state.finger1.touching as u8 + state.finger2.touching as u8;
    let click_target = touchpad::click_button_for_finger_count(finger_count);

    let click_result = abs
        .emit_key(
            uinput_absmouse::BTN_LEFT,
            state.touchpad_click && click_target == Some(ClickButton::Left),
        )
        .and_then(|_| {
            abs.emit_key(
                uinput_absmouse::BTN_RIGHT,
                state.touchpad_click && click_target == Some(ClickButton::Right),
            )
        })
        .and_then(|_| abs.sync());
    if let Err(e) = click_result {
        eprintln!("\n[{controller_id}] failed to emit absolute pointer click: {e}");
    }
}

/// Shared touchpad-MouseRemap handling: identical regardless of which
/// gamepad type (DS4 or Xbox 360) is being emitted, since it targets
/// the separate virtual mouse device either way -- factored out here so
/// OutputMode::Gamepad and OutputMode::Xbox360 don't duplicate this
/// block verbatim.
fn handle_touchpad_mouse_remap(
    mouse: &mut VirtualMouse,
    touchpad_mouse_state: &mut TouchpadMouseState,
    profile: &Profile,
    state: &PadState,
    controller_id: &str,
) {
    let action = touchpad::compute_mouse_action(
        touchpad_mouse_state,
        &profile.touchpad,
        &state.finger1,
        &state.finger2,
    );

    let motion_result = match action {
        MouseAction::None => Ok(()),
        MouseAction::Move { dx, dy } => mouse
            .emit_rel(uinput_mouse::REL_X, dx)
            .and_then(|_| mouse.emit_rel(uinput_mouse::REL_Y, dy))
            .and_then(|_| mouse.sync()),
        MouseAction::Scroll { amount } => mouse.emit_wheel(amount).and_then(|_| mouse.sync()),
    };
    if let Err(e) = motion_result {
        eprintln!("\n[{controller_id}] failed to emit mouse motion/scroll: {e}");
    }

    let finger_count = state.finger1.touching as u8 + state.finger2.touching as u8;
    let click_target = touchpad::click_button_for_finger_count(finger_count);

    let click_result = mouse
        .emit_key(
            uinput_mouse::BTN_LEFT,
            state.touchpad_click && click_target == Some(ClickButton::Left),
        )
        .and_then(|_| {
            mouse.emit_key(
                uinput_mouse::BTN_RIGHT,
                state.touchpad_click && click_target == Some(ClickButton::Right),
            )
        })
        .and_then(|_| mouse.sync());
    if let Err(e) = click_result {
        eprintln!("\n[{controller_id}] failed to emit mouse click: {e}");
    }
}

/// How often main()'s rescan loop re-enumerates for newly connected
/// controllers, in seconds. 1s keeps the worst-case hot-plug delay
/// low enough to feel essentially instant, while still being cheap:
/// refresh_devices() + a device_list() walk is a sysfs read, not
/// meaningfully more expensive once a second than once every five. The
/// true zero-delay approach would be real udev event-driven hotplug
/// (subscribing to kernel add/remove netlink events instead of polling
/// at all) -- a bigger change (new dependency, different architecture)
/// deliberately not done here; this polling interval was judged a
/// better cost/complexity trade-off for now. The actual overhead
/// problem this project had wasn't really the scan interval at all --
/// it was a bug (see enumerate_controllers' doc comment on
/// refresh_devices()) that made every rescan pass repeatedly attempt
/// and fail to open a device that no longer existed, forever. Fixing
/// that root cause matters far more for background-forever overhead
/// than this interval does.
const RESCAN_INTERVAL_SECS: u64 = 1;

fn main() {
    let (profile_name, force_bluetooth, allow_bt_feedback) = parse_args();

    let mut api = HidApi::new().expect("failed to init hidapi (is hidraw accessible? check udev rules)");

    let registry: ipc::Registry = Arc::new(Mutex::new(HashMap::new()));
    // Growable and shared now (not a fixed Vec built once at startup) --
    // controllers can be spawned and reaped throughout the process's
    // life via hot-plug, not just once up front. The ctrlc handler below
    // locks this to iterate whatever's in it AT SIGNAL TIME, however
    // many controllers that happens to be.
    let hidden_guards: Arc<Mutex<Vec<Arc<Mutex<Option<ds4l::hide_controller::HiddenController>>>>>> =
        Arc::new(Mutex::new(Vec::new()));
    // Tracks which hidraw paths currently have a running thread, so
    // enumerate_controllers() never tries to open (and double-drive) a
    // controller that's already got one -- shared between the rescan
    // loop (which adds to it on spawn) and run_controller (which
    // removes its own entry on disconnect, letting a reconnect be
    // picked back up on the next scan).
    let running_paths: Arc<Mutex<std::collections::HashSet<std::path::PathBuf>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    // Process-lifetime fallback-id counter -- see enumerate_controllers'
    // doc comment on why this must NOT reset per call now that it's
    // called repeatedly for rescans, not just once at startup.
    let next_fallback_index = std::sync::atomic::AtomicUsize::new(0);

    // Control socket and signal handler are started ONCE, up front,
    // regardless of whether any controller is connected yet -- an
    // earlier version exited the whole process with an error if zero
    // controllers were found at startup, which meant the daemon
    // couldn't simply be left running waiting for someone to plug
    // something in later. Now it just keeps scanning.
    match ipc::start(registry.clone()) {
        Ok(()) => {
            println!("Control socket listening at {}", ipc::socket_path().display());
        }
        Err(e) => {
            eprintln!(
                "Warning: could not start control socket ({e}) -- ds4l_gui profile switching \
                 will not work this run; every controller's own thread is otherwise unaffected."
            );
        }
    }

    // Single process-wide signal handler covering every controller,
    // present AND future: the ctrlc handler and each controller's
    // HiddenController guard both need restore() to run before exit --
    // Drop alone won't fire on an unhandled signal, since the default
    // Rust behavior for SIGINT/SIGTERM/SIGHUP terminates the process
    // without unwinding the stack. hidden_guards is locked at signal
    // time and iterated fresh, so this correctly covers controllers
    // that connected long after startup, not just ones present when the
    // handler was installed.
    //
    // IMPORTANT: the `ctrlc` crate only registers a handler for SIGINT
    // by default -- SIGTERM (what `systemctl stop` and a plain `kill`
    // send) and SIGHUP would otherwise still terminate the process
    // immediately, un-caught, leaving every hidden controller hidden.
    // Cargo.toml enables ctrlc's `termination` feature specifically so
    // the same set_handler() call below covers SIGINT, SIGTERM, and
    // SIGHUP -- don't drop that feature flag without re-adding
    // equivalent handling (e.g. via `signal-hook`), or this silently
    // regresses.
    {
        let hidden_guards_for_handler = hidden_guards.clone();
        let socket_path_for_handler = ipc::socket_path();
        if let Err(e) = ctrlc::set_handler(move || {
            for guard in hidden_guards_for_handler.lock().unwrap().iter() {
                *guard.lock().unwrap() = None;
            }
            let _ = std::fs::remove_file(&socket_path_for_handler);
            std::process::exit(0);
        }) {
            eprintln!(
                "Warning: failed to install signal handler ({e}) -- if any real controller is \
                 hidden, permissions will only be restored on normal exit, not Ctrl+C/SIGTERM/ \
                 SIGHUP. Restore manually with chmod if a controller seems inaccessible after \
                 a forced quit."
            );
        }
    }

    println!(
        "Scanning for DS4 v2 controllers (rescanning every {RESCAN_INTERVAL_SECS}s -- \
         hot-plug is supported, no restart needed when a controller connects or disconnects). \
         Ctrl+C to quit.\n"
    );

    // Rescan loop: this IS main() now, not a one-time startup step
    // followed by parking forever. Each pass finds controllers not
    // already running (enumerate_controllers filters those out via
    // running_paths) and spawns a thread for each. A controller that
    // disconnects has its own thread detect that (see run_controller's
    // doc comment) and remove itself from running_paths, so it's
    // naturally picked up fresh on a later pass through this same loop
    // if it reconnects -- no special "reconnect" logic needed here
    // beyond the ordinary "found something not already running" path.
    loop {
        let found = {
            let running = running_paths.lock().unwrap();
            enumerate_controllers(&mut api, force_bluetooth, &running, &next_fallback_index)
        };

        for controller in found {
            println!("[{}] New controller -- starting a thread for it.", controller.id);
            running_paths.lock().unwrap().insert(controller.hidraw_path.clone());

            let hidden_guard = Arc::new(Mutex::new(None));
            hidden_guards.lock().unwrap().push(hidden_guard.clone());

            let registry = registry.clone();
            let profile_name = profile_name.clone();
            let running_paths = running_paths.clone();
            std::thread::spawn(move || {
                run_controller(
                    controller.id,
                    controller.device,
                    controller.connection,
                    controller.cal,
                    controller.hidraw_path,
                    profile_name,
                    allow_bt_feedback,
                    registry,
                    hidden_guard,
                    running_paths,
                );
            });
        }

        std::thread::sleep(Duration::from_secs(RESCAN_INTERVAL_SECS));
    }
}

/// Collects every distinct KEY_* code a KbmConfig actually maps to, so
/// the virtual keyboard device only registers the key bits it needs
/// rather than guessing a broad range (see uinput_mouse.rs's doc comment
/// on why we don't bulk-register an unverified range of codes).
fn collect_mapped_keys(cfg: &ds4l::kbm::KbmConfig) -> Vec<u16> {
    use ds4l::kbm::{KbmTarget, StickKbmMode};
    let mut keys = std::collections::HashSet::new();
    let mut add = |t: KbmTarget| match t {
        KbmTarget::Key(k) => {
            keys.insert(k);
        }
        KbmTarget::Combo(combo) => {
            for k in combo.into_iter().flatten() {
                keys.insert(k);
            }
        }
        KbmTarget::None | KbmTarget::MouseLeft | KbmTarget::MouseRight => {}
    };
    add(cfg.cross);
    add(cfg.circle);
    add(cfg.triangle);
    add(cfg.square);
    add(cfg.l1);
    add(cfg.r1);
    add(cfg.l2);
    add(cfg.r2);
    add(cfg.l3);
    add(cfg.r3);
    add(cfg.share);
    add(cfg.options);
    add(cfg.ps);
    add(cfg.touchpad_click);
    add(cfg.dpad_up);
    add(cfg.dpad_down);
    add(cfg.dpad_left);
    add(cfg.dpad_right);
    for stick in [&cfg.left_stick, &cfg.right_stick] {
        if let StickKbmMode::Digital { up, down, left, right, .. } = stick {
            add(*up);
            add(*down);
            add(*left);
            add(*right);
        }
    }
    keys.into_iter().collect()
}
