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
// (Live-reload without restart is a natural follow-up once this is
// confirmed working, not included this milestone.)

use ds4l::ds4_input::{
    calibrated_gyro_deg_s, open_and_calibrate, parse_report, send_output_report, OutputReport,
    PadState,
};
use ds4l::gyro_stick::{self, GyroStickState};
use ds4l::kbm::{self, KbmState, PressedKey};
use ds4l::profile::{self, OutputMode, Profile};
use ds4l::touchpad::{self, ClickButton, MouseAction, TouchpadMode, TouchpadMouseState};
use ds4l::uinput_ds4::{self, VirtualDs4};
use ds4l::uinput_mouse::{self, VirtualMouse};
use hidapi::HidApi;
use std::time::Duration;

fn parse_args() -> String {
    let args: Vec<String> = std::env::args().collect();
    for i in 1..args.len() {
        if args[i] == "--profile" {
            if let Some(name) = args.get(i + 1) {
                return name.clone();
            }
        }
    }
    "Default".to_string()
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

fn main() {
    let profile_name = parse_args();

    println!("Loading profile \"{profile_name}\"...");
    let profile: Profile = profile::load(&profile_name).unwrap_or_else(|e| {
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
            "(Edit ~/.config/ds4l/profiles/{profile_name}.toml directly and restart to change \
             settings -- full path: {})",
            path.join(format!("{profile_name}.toml")).display()
        );
    }

    let api = HidApi::new().expect("failed to init hidapi (is hidraw accessible? check udev rules)");

    println!("Connecting to real DS4 v2...");
    let (device, cal) = open_and_calibrate(&api).unwrap_or_else(|e| {
        eprintln!("{e}\nIs it plugged in via USB? See README udev setup.");
        std::process::exit(1);
    });
    println!("Real DS4 connected, calibration loaded.");

    // Set lightbar color immediately, and pulse rumble briefly if this
    // profile requests it -- confirms the right profile loaded without
    // needing to look at a terminal. Errors here are logged but not
    // fatal: a lightbar/rumble failure shouldn't stop the daemon from
    // otherwise functioning normally.
    let lb = profile.feedback.lightbar;
    let led_report = OutputReport {
        led_red: lb.red,
        led_green: lb.green,
        led_blue: lb.blue,
        set_led: true,
        ..Default::default()
    };
    if let Err(e) = send_output_report(&device, &led_report) {
        eprintln!("Warning: failed to set lightbar color: {e}");
    }

    if profile.feedback.rumble_on_load {
        let pulse_on = OutputReport {
            rumble_weak: 150,
            rumble_strong: 150,
            set_rumble: true,
            ..Default::default()
        };
        if let Err(e) = send_output_report(&device, &pulse_on) {
            eprintln!("Warning: failed to start rumble pulse: {e}");
        }
        std::thread::sleep(Duration::from_millis(250));
        let pulse_off = OutputReport {
            rumble_weak: 0,
            rumble_strong: 0,
            set_rumble: true,
            ..Default::default()
        };
        if let Err(e) = send_output_report(&device, &pulse_off) {
            eprintln!("Warning: failed to stop rumble pulse: {e}");
        }
    }

    // Device creation branches on output_mode: Gamepad mode creates the
    // virtual DS4 (plus an optional virtual mouse for touchpad remap);
    // Kbm mode creates only the combined keyboard+mouse device, since
    // there's no virtual gamepad to drive in that mode.
    let mut virtual_pad = if profile.output_mode == OutputMode::Gamepad {
        Some(VirtualDs4::create().unwrap_or_else(|e| {
            eprintln!("Failed to create virtual DS4: {e}\nCheck /dev/uinput permissions.");
            std::process::exit(1);
        }))
    } else {
        None
    };

    let needs_mouse_device = profile.output_mode == OutputMode::Kbm
        || (profile.output_mode == OutputMode::Gamepad
            && profile.touchpad.mode == TouchpadMode::MouseRemap);
    let mut virtual_mouse = if needs_mouse_device {
        // KBM mode needs every key code its mapping actually uses
        // registered up front; touchpad-only MouseRemap mode (Gamepad
        // output) doesn't need any keyboard keys, just the base mouse
        // buttons uinput_mouse::create always registers.
        let extra_keys: Vec<u16> = if profile.output_mode == OutputMode::Kbm {
            collect_mapped_keys(&profile.kbm)
        } else {
            Vec::new()
        };
        Some(VirtualMouse::create(&extra_keys).unwrap_or_else(|e| {
            eprintln!("Failed to create virtual mouse: {e}\nCheck /dev/uinput permissions.");
            std::process::exit(1);
        }))
    } else {
        None
    };
    println!("Virtual device(s) created. Running with profile \"{}\".", profile.name);
    println!("Ctrl+C to quit.\n");

    let mut gyro_state = GyroStickState::default();
    let mut touchpad_mouse_state = TouchpadMouseState::default();
    let mut kbm_state = KbmState::default();

    let mut buf = [0u8; 64];
    loop {
        match device.read_timeout(&mut buf, 100) {
            Ok(len) if len >= 25 && buf[0] == 0x01 => {
                let state = parse_report(&buf);

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
                            if let Err(e) = emit_gamepad_state(
                                pad,
                                &state,
                                rx,
                                ry,
                                profile.touchpad.mode,
                            ) {
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
                                let click_target =
                                    touchpad::click_button_for_finger_count(finger_count);

                                let click_result = mouse
                                    .emit_key(
                                        uinput_mouse::BTN_LEFT,
                                        state.touchpad_click
                                            && click_target == Some(ClickButton::Left),
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
            Ok(_) => {}
            Err(e) => {
                eprintln!("\nreal pad read error: {e}");
                std::thread::sleep(Duration::from_millis(500));
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
