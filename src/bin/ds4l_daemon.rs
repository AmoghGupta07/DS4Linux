// The real daemon: loads a profile from ~/.config/ds4l/profiles/ and
// drives gyro-to-stick + touchpad (both modes, 2-finger) from it, instead
// of the hardcoded consts the earlier test binaries used. Left stick,
// buttons, dpad, triggers still pass through 1:1 as they have since
// Milestone 3.
//
// On connect: sets the lightbar to the profile's configured color, and
// (if enabled) pulses rumble briefly -- confirms the daemon connected and
// loaded the right profile without needing to check a terminal. This is
// the first milestone that WRITES to the controller (HID output report
// 0x05) rather than only reading from it.
//
// Usage:
//   ds4l_daemon                 # loads/creates the "Default" profile
//   ds4l_daemon --profile Name  # loads a specific profile by name
//
// Profile file lives at ~/.config/ds4l/profiles/<name>.toml and is plain,
// hand-editable TOML -- edit it, restart the daemon, changes take effect.
//
// Live profile switching WITHOUT a restart is now also available via a
// local control socket (see src/ipc.rs) -- ds4l_gui's tray icon (or any
// client speaking that plain-text protocol) can tell an already-running
// daemon to load a different profile on the fly. This is new,
// NOT YET verified against a live GUI/daemon pair the way the rest of
// this project was checked against real hardware before being trusted
// -- see ipc.rs's doc comment.

use ds4l::ds4_bt::{self, trigger_full_report_mode};
use ds4l::ds4_input::{
    calibrated_gyro_deg_s, open_and_calibrate, parse_report, send_output_report, OutputReport,
    PadState, SONY_VID,
};
use ds4l::gyro_stick::{self, GyroStickState};
use ds4l::ipc;
use ds4l::kbm::{self, KbmState, PressedKey};
use ds4l::profile::{self, Ds4FeedbackConfig, OutputMode, Profile};
use ds4l::touchpad::{self, ClickButton, MouseAction, TouchpadMode, TouchpadMouseState};
use ds4l::uinput_ds4::{self, VirtualDs4};
use ds4l::uinput_mouse::{self, VirtualMouse};
use hidapi::{HidApi, HidDevice};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Which transport the daemon connected over. Determines how reports are
/// read/parsed (parse_report vs ds4_bt::read_bt_report), but nothing else
/// -- everything downstream of a parsed PadState is identical regardless
/// of connection type, which is exactly why ds4_bt.rs was built to
/// produce the same PadState USB parsing does.
///
/// `Clone, Copy`: needed so a live profile switch (apply_profile_switch)
/// can be handed the connection type by value without fighting the
/// borrow checker over a `&Connection` that's also being matched on in
/// the main loop at the same time.
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

/// Tries USB first (existing, proven path), falls back to Bluetooth if
/// no USB device is found. `--bluetooth` forces BT even if USB is also
/// available, for testing or if someone specifically wants BT despite
/// having a cable plugged in.
///
/// FIXED: an earlier version called `api.open(vid, pid)` directly for
/// "USB," but that function matches by VID/PID alone -- it doesn't care
/// about transport, so when the DS4 was connected only over Bluetooth
/// (which reports the SAME VID/PID as USB), `open()` still succeeded and
/// the daemon ran USB-shaped parsing against BT-shaped reports. The fix
/// is to enumerate devices first via `HidApi::device_list()` and check
/// each `DeviceInfo::bus_type()` (confirmed against hidapi's actual
/// source: `BusType::Usb = 0x01`, `BusType::Bluetooth = 0x02`) before
/// opening, so we open the specific device path we've already confirmed
/// is on the transport we think it is.
///
/// FIXED (round 2): even after fixing bus_type detection, a DS4 can
/// expose more than one HID interface/collection under the same VID/PID
/// and bus type (the actual gamepad interface plus others). Without also
/// filtering by usage_page/usage, `device_list()` could match a non-
/// gamepad interface and overwrite the correct one depending on
/// enumeration order -- silently picking the wrong device. Filtering to
/// usage_page=0x01 (Generic Desktop) and usage=0x05 (Game Pad) is the
/// SAME filter DS4Windows itself uses to identify a genuine DS4 gamepad
/// interface (confirmed: "DS4Windows only checks for devices if their
/// usage is 0x04 or 0x05" -- Joystick or Game Pad), and matches the DS4
/// v2's actual documented HID report descriptor (Usage Page 0x05 0x01,
/// Usage 0x09 0x05). Note: usage_page/usage are NOT available via the
/// Linux libusb hidapi backend per hidapi's own docs -- this project
/// uses the default hidraw backend, where they are available.
fn connect(
    api: &HidApi,
    force_bluetooth: bool,
) -> (HidDevice, Connection, ds4l::ds4_input::GyroCalibration, std::path::PathBuf) {
    const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
    const USAGE_GAME_PAD: u16 = 0x05;

    let mut usb_info = None;
    let mut bt_info = None;

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
            hidapi::BusType::Usb => usb_info = Some(info.clone()),
            hidapi::BusType::Bluetooth => bt_info = Some(info.clone()),
            _ => {}
        }
    }

    if !force_bluetooth {
        if let Some(info) = usb_info {
            let hidraw_path = std::path::PathBuf::from(info.path().to_string_lossy().to_string());
            if let Ok(device) = info.open_device(api) {
                println!("Found DS4 v2 on USB bus, connecting...");
                if let Err(e) = device.set_blocking_mode(false) {
                    eprintln!("Warning: failed to set non-blocking mode: {e}");
                }
                let cal = ds4l::ds4_input::read_calibration(&device).unwrap_or_else(|e| {
                    eprintln!("Warning: USB calibration read failed ({e}) -- gyro will be uncalibrated.");
                    ds4l::ds4_input::GyroCalibration::identity()
                });
                return (device, Connection::Usb, cal, hidraw_path);
            }
        }
        println!("No USB DS4 found, trying Bluetooth...");
    }

    let info = bt_info.unwrap_or_else(|| {
        eprintln!(
            "Could not find a DS4 v2 on USB or Bluetooth.\n\
             Make sure it's either plugged in via USB or paired/connected \
             over Bluetooth (check `bluetoothctl devices`)."
        );
        std::process::exit(1);
    });

    let hidraw_path = std::path::PathBuf::from(info.path().to_string_lossy().to_string());
    let device = info.open_device(api).unwrap_or_else(|e| {
        eprintln!("Found a Bluetooth DS4 but failed to open it: {e}");
        std::process::exit(1);
    });

    println!("Found DS4 v2 on Bluetooth bus, connecting. Triggering full-report handshake...");
    if let Err(e) = trigger_full_report_mode(&device) {
        eprintln!(
            "Warning: BT handshake failed ({e}) -- may stay stuck receiving only \
             truncated reports with no gyro/touchpad data."
        );
    }

    let cal = ds4_bt::read_calibration_bt(&device).unwrap_or_else(|e| {
        eprintln!("Warning: BT calibration read failed ({e}) -- gyro will be uncalibrated (will drift).");
        ds4l::ds4_input::GyroCalibration::identity()
    });

    (device, Connection::Bluetooth, cal, hidraw_path)
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

fn emit_gamepad_state(
    pad: &mut VirtualDs4,
    state: &PadState,
    right_x: u8,
    right_y: u8,
    touchpad_mode: TouchpadMode,
) -> std::io::Result<()> {
    pad.emit_abs(uinput_ds4::ABS_X, state.lx as i32)?;
    pad.emit_abs(uinput_ds4::ABS_Y, state.ly as i32)?;
    pad.emit_abs(uinput_ds4::ABS_RX, right_x as i32)?;
    pad.emit_abs(uinput_ds4::ABS_RY, right_y as i32)?;
    pad.emit_abs(uinput_ds4::ABS_Z, state.l2_analog as i32)?;
    pad.emit_abs(uinput_ds4::ABS_RZ, state.r2_analog as i32)?;

    let (hat_x, hat_y) = match state.dpad {
        0 => (0, -1),
        1 => (1, -1),
        2 => (1, 0),
        3 => (1, 1),
        4 => (0, 1),
        5 => (-1, 1),
        6 => (-1, 0),
        7 => (-1, -1),
        _ => (0, 0),
    };
    pad.emit_abs(uinput_ds4::ABS_HAT0X, hat_x)?;
    pad.emit_abs(uinput_ds4::ABS_HAT0Y, hat_y)?;

    pad.emit_key(uinput_ds4::BTN_SOUTH, state.cross)?;
    pad.emit_key(uinput_ds4::BTN_EAST, state.circle)?;
    pad.emit_key(uinput_ds4::BTN_NORTH, state.triangle)?;
    pad.emit_key(uinput_ds4::BTN_WEST, state.square)?;
    pad.emit_key(uinput_ds4::BTN_TL, state.l1)?;
    pad.emit_key(uinput_ds4::BTN_TR, state.r1)?;
    pad.emit_key(uinput_ds4::BTN_TL2, state.l2_digital)?;
    pad.emit_key(uinput_ds4::BTN_TR2, state.r2_digital)?;
    pad.emit_key(uinput_ds4::BTN_SELECT, state.share)?;
    pad.emit_key(uinput_ds4::BTN_START, state.options)?;
    pad.emit_key(uinput_ds4::BTN_THUMBL, state.l3)?;
    pad.emit_key(uinput_ds4::BTN_THUMBR, state.r3)?;
    pad.emit_key(uinput_ds4::BTN_MODE, state.ps)?;

    if touchpad_mode == TouchpadMode::Passthrough {
        pad.emit_touch(0, state.finger1.touching, state.finger1.x as i32, state.finger1.y as i32)?;
        pad.emit_touch(1, state.finger2.touching, state.finger2.x as i32, state.finger2.y as i32)?;
    }

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
/// Gamepad output mode, a VirtualMouse for Kbm mode or for Gamepad mode
/// with touchpad MouseRemap, both, or neither. Returns an error instead
/// of exiting the process, unlike an earlier inline version of this
/// logic that called `std::process::exit(1)` directly on failure --
/// that was correct for the ORIGINAL startup-only call site (nothing
/// useful to fall back to if the very first device creation fails), but
/// would be wrong here now that this same logic also runs mid-session
/// on a live profile switch, where a failure should be reported back to
/// the caller (ds4l_gui) while the daemon keeps running its current,
/// still-working profile -- never killed by a switch attempt gone wrong.
fn try_create_virtual_devices(
    profile: &Profile,
) -> Result<(Option<VirtualDs4>, Option<VirtualMouse>), String> {
    let virtual_pad = if profile.output_mode == OutputMode::Gamepad {
        Some(VirtualDs4::create().map_err(|e| format!("failed to create virtual DS4: {e}"))?)
    } else {
        None
    };

    let needs_mouse_device = profile.output_mode == OutputMode::Kbm
        || (profile.output_mode == OutputMode::Gamepad
            && profile.touchpad.mode == TouchpadMode::MouseRemap);
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

    Ok((virtual_pad, virtual_mouse))
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
    let led_report = OutputReport {
        led_red: lb.red,
        led_green: lb.green,
        led_blue: lb.blue,
        set_led: true,
        ..Default::default()
    };
    let led_result = match connection {
        Connection::Usb => send_output_report(device, &led_report),
        Connection::Bluetooth => ds4_bt::send_output_report_bt(device, &led_report),
    };
    if let Err(e) = led_result {
        eprintln!("Warning: failed to set lightbar color: {e}");
    }

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
fn apply_profile_switch(
    name: &str,
    profile: &mut Profile,
    virtual_pad: &mut Option<VirtualDs4>,
    virtual_mouse: &mut Option<VirtualMouse>,
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
    let (new_pad, new_mouse) = try_create_virtual_devices(&new_profile)?;

    // Reconcile controller-hiding only if the setting actually changed,
    // so switching between two profiles that both hide the controller
    // doesn't needlessly toggle permissions off and back on.
    if new_profile.hide_real_controller != profile.hide_real_controller {
        if new_profile.hide_real_controller {
            match ds4l::hide_controller::HiddenController::hide(hidraw_path) {
                Ok(guard) => *hidden_controller.lock().unwrap() = Some(guard),
                Err(e) => eprintln!(
                    "Warning: switched to profile \"{name}\" but failed to hide the real \
                     controller: {e}"
                ),
            }
        } else {
            // Dropping the guard runs HiddenController::restore().
            *hidden_controller.lock().unwrap() = None;
        }
    }

    // Nothing past this point can fail -- safe to commit the swap. The
    // OLD virtual_pad/virtual_mouse Drop here (destroying their uinput
    // nodes) the moment they're overwritten.
    *virtual_pad = new_pad;
    *virtual_mouse = new_mouse;
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

    *status.lock().unwrap() = ipc::StatusSnapshot {
        profile_name: profile.name.clone(),
        output_mode: format!("{:?}", profile.output_mode),
        connection: connection.label().to_string(),
        hidden: hidden_controller.lock().unwrap().is_some(),
    };

    println!("\nSwitched to profile \"{}\" via control socket.", profile.name);
    Ok(())
}

fn main() {
    let (profile_name, force_bluetooth, allow_bt_feedback) = parse_args();

    println!("Loading profile \"{profile_name}\"...");
    let mut profile: Profile = profile::load(&profile_name).unwrap_or_else(|e| {
        eprintln!(
            "Failed to load profile \"{profile_name}\": {e}\n\
             Falling back to built-in defaults for this run (not saved)."
        );
        Profile {
            name: profile_name.clone(),
            ..Profile::default()
        }
    });
    println!(
        "Profile loaded: output mode={:?}, gyro mode={:?} sensitivity={:.0}deg/s, touchpad mode={:?}",
        profile.output_mode, profile.gyro.mode, profile.gyro.deg_per_sec_at_full_stick, profile.touchpad.mode
    );
    if let Ok(path) = profile::profiles_dir() {
        println!(
            "(Edit ~/.config/ds4l/profiles/{profile_name}.toml directly and restart, or use \
             ds4l_gui to edit and hot-switch profiles live -- full path: {})",
            path.join(format!("{profile_name}.toml")).display()
        );
    }

    let api = HidApi::new().expect("failed to init hidapi (is hidraw accessible? check udev rules)");

    println!("Connecting to DS4 v2...");
    let (device, connection, cal, hidraw_path) = connect(&api, force_bluetooth);
    println!("DS4 connected, calibration loaded.");

    // Controller hiding: restricts the real controller's device nodes
    // from other processes (Steam, games) while this daemon runs,
    // restoring original permissions on exit -- including Ctrl+C AND
    // `systemctl stop`/plain `kill` (SIGTERM), via the ctrlc handler
    // below. Opt-in per profile (hide_real_controller), off by default.
    //
    // The ctrlc handler and the HiddenController guard both need to run
    // restore() before exit -- Drop alone won't fire on an unhandled
    // signal, since the default Rust behavior for SIGINT/SIGTERM/SIGHUP
    // terminates the process without unwinding the stack. We hold the
    // guard in an Arc<Mutex<Option<_>>> shared between main() and the
    // signal handler so either path (normal Drop at end of main, or the
    // handler firing on a signal) can trigger restoration exactly once.
    //
    // IMPORTANT: the `ctrlc` crate only registers a handler for SIGINT
    // by default -- SIGTERM (what `systemctl stop` and a plain `kill`
    // send) and SIGHUP would otherwise still terminate the process
    // immediately, un-caught, leaving the controller hidden. Cargo.toml
    // enables ctrlc's `termination` feature specifically so the same
    // set_handler() call below covers SIGINT, SIGTERM, and SIGHUP --
    // don't drop that feature flag without re-adding equivalent
    // handling (e.g. via `signal-hook`), or this silently regresses.
    let hidden_controller: Arc<Mutex<Option<ds4l::hide_controller::HiddenController>>> =
        Arc::new(Mutex::new(None));

    if profile.hide_real_controller {
        match ds4l::hide_controller::HiddenController::hide(&hidraw_path) {
            Ok(guard) => {
                *hidden_controller.lock().unwrap() = Some(guard);
            }
            Err(e) => {
                eprintln!(
                    "Warning: failed to hide real controller: {e}\n\
                     Continuing without hiding -- other processes may still see the real controller."
                );
            }
        }
    }

    // Control socket: lets ds4l_gui (or anything else speaking the plain
    // text protocol documented in ipc.rs) query status and hot-switch
    // profiles on this already-running daemon without a restart. A
    // failure to bind here is a WARNING, not fatal -- the daemon is
    // still fully usable standalone, just not remotely controllable
    // this run (e.g. a stale-socket/permission problem on the runtime
    // dir, or another daemon instance already holding it).
    let status = Arc::new(Mutex::new(ipc::StatusSnapshot {
        profile_name: profile.name.clone(),
        output_mode: format!("{:?}", profile.output_mode),
        connection: connection.label().to_string(),
        hidden: hidden_controller.lock().unwrap().is_some(),
    }));
    let control_rx = match ipc::start(status.clone()) {
        Ok(rx) => {
            println!("Control socket listening at {}", ipc::socket_path().display());
            Some(rx)
        }
        Err(e) => {
            eprintln!(
                "Warning: could not start control socket ({e}) -- ds4l_gui profile switching \
                 will not work this run; the daemon itself is otherwise unaffected."
            );
            None
        }
    };

    {
        let hidden_controller_for_handler = hidden_controller.clone();
        let socket_path_for_handler = ipc::socket_path();
        if let Err(e) = ctrlc::set_handler(move || {
            // Explicitly drop the guard here (rather than relying on
            // process exit to run Drop) so permissions are restored
            // before the process actually terminates. Fires for
            // SIGINT, SIGTERM, or SIGHUP -- see the `termination`
            // feature note on the ctrlc dependency in Cargo.toml.
            *hidden_controller_for_handler.lock().unwrap() = None;
            // Also clean up the control socket file so a subsequent
            // restart's stale-socket check in ipc::start() has one less
            // thing to reason about -- best-effort, not required for
            // correctness (start() already handles a leftover file from
            // an unclean exit like kill -9 on its own).
            let _ = std::fs::remove_file(&socket_path_for_handler);
            std::process::exit(0);
        }) {
            eprintln!(
                "Warning: failed to install signal handler ({e}) -- if the real controller is \
                 hidden, permissions will only be restored on normal exit, not Ctrl+C/SIGTERM/ \
                 SIGHUP. Restore manually with chmod if the controller seems inaccessible after \
                 a forced quit."
            );
        }
    }

    // LED/rumble implemented for both USB and Bluetooth; BT support
    // carries a documented real risk (see ds4_bt::send_output_report_bt's
    // doc comment): a malformed BT output report has been reported to
    // silently stop the controller from streaming full input reports
    // until reconnected -- not just "rumble won't work," potentially
    // "input breaks too." Given that, BT LED/rumble is gated behind
    // --bt-feedback rather than firing automatically on every BT
    // connection, so a bad first test doesn't silently break your input
    // mid-session without you expecting it. This flag is a daemon-
    // startup setting (not per-profile), so it applies the same way to
    // every profile switch for the life of this process.
    let send_feedback = matches!(connection, Connection::Usb) || allow_bt_feedback;
    if send_feedback {
        apply_feedback(&device, connection, &profile.feedback);
    } else {
        println!(
            "(LED/rumble on load skipped over Bluetooth -- pass --bt-feedback to enable; \
             see source comments for a documented risk with this before enabling.)"
        );
    }

    // Device creation branches on output_mode: Gamepad mode creates the
    // virtual DS4 (plus an optional virtual mouse for touchpad remap);
    // Kbm mode creates only the combined keyboard+mouse device, since
    // there's no virtual gamepad to drive in that mode. Shared with the
    // live-switch path (apply_profile_switch) via try_create_virtual_devices
    // so both use identical logic.
    let (mut virtual_pad, mut virtual_mouse) =
        try_create_virtual_devices(&profile).unwrap_or_else(|e| {
            eprintln!("{e}\nCheck /dev/uinput permissions.");
            std::process::exit(1);
        });
    println!("Virtual device(s) created. Running with profile \"{}\".", profile.name);
    println!("Ctrl+C to quit.\n");

    let mut gyro_state = GyroStickState::default();
    let mut touchpad_mouse_state = TouchpadMouseState::default();
    let mut kbm_state = KbmState::default();

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
        if let Some(rx) = control_rx.as_ref() {
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    ipc::PendingCommand::SwitchProfile { name, reply } => {
                        let result = apply_profile_switch(
                            &name,
                            &mut profile,
                            &mut virtual_pad,
                            &mut virtual_mouse,
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
                        // (e.g. hit ipc.rs's own 5s timeout) -- the
                        // switch itself still applied successfully to
                        // the daemon either way.
                        let _ = reply.send(result);
                    }
                }
            }
        }

        let maybe_state: Option<PadState> = match connection {
            Connection::Usb => match device.read_timeout(&mut buf, 100) {
                Ok(len) if len >= 25 && buf[0] == 0x01 => Some(parse_report(&buf)),
                Ok(_) => None,
                Err(e) => {
                    eprintln!("\nUSB read error: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                    None
                }
            },
            Connection::Bluetooth => match ds4_bt::read_bt_report(&device, &mut buf) {
                Ok(state) => state,
                Err(e) => {
                    eprintln!("\nBT read error: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                    None
                }
            },
        };

        let state = match maybe_state {
            Some(s) => s,
            None => continue,
        };

        match profile.output_mode {
            OutputMode::Kbm => {
                if let Some(mouse) = virtual_mouse.as_mut() {
                    let frame = kbm::compute_frame(&state, &profile.kbm, &mut kbm_state);
                    if let Err(e) = apply_kbm_frame(mouse, &mut kbm_state, frame) {
                        eprintln!("\nfailed to emit KBM frame: {e}");
                    }
                }
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
                let (rx, ry) = gyro_stick::blend_and_clamp(state.rx, state.ry, gdx, gdy);

                if let Some(pad) = virtual_pad.as_mut() {
                    if let Err(e) = emit_gamepad_state(pad, &state, rx, ry, profile.touchpad.mode) {
                        eprintln!("\nfailed to emit gamepad state: {e}");
                    }
                }

                if profile.touchpad.mode == TouchpadMode::MouseRemap {
                    if let Some(mouse) = virtual_mouse.as_mut() {
                        let action = touchpad::compute_mouse_action(
                            &mut touchpad_mouse_state,
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
                            MouseAction::Scroll { amount } => {
                                mouse.emit_wheel(amount).and_then(|_| mouse.sync())
                            }
                        };
                        if let Err(e) = motion_result {
                            eprintln!("\nfailed to emit mouse motion/scroll: {e}");
                        }

                        let finger_count =
                            state.finger1.touching as u8 + state.finger2.touching as u8;
                        let click_target = touchpad::click_button_for_finger_count(finger_count);

                        let click_result = mouse
                            .emit_key(
                                uinput_mouse::BTN_LEFT,
                                state.touchpad_click && click_target == Some(ClickButton::Left),
                            )
                            .and_then(|_| {
                                mouse.emit_key(
                                    uinput_mouse::BTN_RIGHT,
                                    state.touchpad_click
                                        && click_target == Some(ClickButton::Right),
                                )
                            })
                            .and_then(|_| mouse.sync());
                        if let Err(e) = click_result {
                            eprintln!("\nfailed to emit mouse click: {e}");
                        }
                    }
                }
            }
        }
    }
}

/// Collects every distinct KEY_* code a KbmConfig actually maps to, so
/// the virtual keyboard device only registers the key bits it needs
/// rather than guessing a broad range (see uinput_mouse.rs's doc comment
/// on why we don't bulk-register an unverified range of codes).
fn collect_mapped_keys(cfg: &ds4l::kbm::KbmConfig) -> Vec<u16> {
    use ds4l::kbm::{KbmTarget, StickKbmMode};
    let mut keys = std::collections::HashSet::new();
    let mut add = |t: KbmTarget| {
        if let KbmTarget::Key(k) = t {
            keys.insert(k);
        }
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
