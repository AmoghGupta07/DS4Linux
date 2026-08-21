// Milestone 4: touchpad support, both modes.
//
// - Passthrough mode: real finger position -> virtual DS4's multitouch
//   axes, for games that read the DS4 touchpad natively.
// - MouseRemap mode: finger movement -> relative mouse deltas on a
//   separate virtual mouse device, DS4Windows-style.
//
// Single-finger only this milestone (finger 2 parsed but unused).
// Everything from Milestone 3.5 (gyro-to-right-stick, passthrough for
// sticks/buttons/dpad/triggers) is unchanged and still active.
//
// Switch modes by editing TOUCHPAD_MODE below and rebuilding -- stand-in
// for the future per-profile setting, same pattern as gyro_stick_test.

use ds4l::ds4_input::{calibrated_gyro_deg_s, open_and_calibrate, parse_report, PadState};
use ds4l::gyro_stick::{self, GyroMode, GyroStickConfig, GyroStickState};
use ds4l::touchpad::{self, TouchpadConfig, TouchpadMode, TouchpadMouseState};
use ds4l::uinput_ds4::{self, VirtualDs4};
use ds4l::uinput_mouse::{self, VirtualMouse};
use hidapi::HidApi;
use std::time::Duration;

/// Edit this to switch modes: TouchpadMode::Passthrough or
/// TouchpadMode::MouseRemap.
const TOUCHPAD_MODE: TouchpadMode = TouchpadMode::MouseRemap;

const GYRO_MODE: GyroMode = GyroMode::Hold;

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
        pad.emit_touch(
            state.finger1.touching,
            state.finger1.x as i32,
            state.finger1.y as i32,
        )?;
    }

    pad.sync()
}

fn main() {
    let api = HidApi::new().expect("failed to init hidapi (is hidraw accessible? check udev rules)");

    println!("Connecting to real DS4 v2...");
    let (device, cal) = open_and_calibrate(&api).unwrap_or_else(|e| {
        eprintln!("{e}\nIs it plugged in via USB? See README udev setup.");
        std::process::exit(1);
    });
    println!("Real DS4 connected, calibration loaded.");

    println!("Creating virtual DS4 via uinput...");
    let mut virtual_pad = VirtualDs4::create().unwrap_or_else(|e| {
        eprintln!("Failed to create virtual DS4: {e}\nCheck /dev/uinput permissions.");
        std::process::exit(1);
    });

    // Only spin up the virtual mouse if we're actually in MouseRemap mode
    // -- no point creating an unused input device otherwise.
    let mut virtual_mouse = if TOUCHPAD_MODE == TouchpadMode::MouseRemap {
        Some(VirtualMouse::create().unwrap_or_else(|e| {
            eprintln!("Failed to create virtual mouse: {e}\nCheck /dev/uinput permissions.");
            std::process::exit(1);
        }))
    } else {
        None
    };
    println!("Virtual device(s) created.");

    let gyro_cfg = GyroStickConfig {
        mode: GYRO_MODE,
        ..Default::default()
    };
    let mut gyro_state = GyroStickState::default();

    let touchpad_cfg = TouchpadConfig {
        mode: TOUCHPAD_MODE,
        ..Default::default()
    };
    let mut touchpad_mouse_state = TouchpadMouseState::default();

    let mode_desc = match TOUCHPAD_MODE {
        TouchpadMode::Passthrough => "PASSTHROUGH (native touchpad data to virtual DS4)",
        TouchpadMode::MouseRemap => "MOUSE-REMAP (finger movement -> virtual mouse)",
    };
    println!(
        "\nTouchpad mode: {mode_desc}\n\
         Gyro-to-right-stick: {:?} (gate: L2)\n\
         Single finger only this milestone.\n\
         Ctrl+C to quit.\n",
        GYRO_MODE
    );

    let mut buf = [0u8; 64];
    loop {
        match device.read_timeout(&mut buf, 100) {
            Ok(len) if len >= 25 && buf[0] == 0x01 => {
                let state = parse_report(&buf);
                let gyro = calibrated_gyro_deg_s(&state, &cal);

                let (gdx, gdy) = gyro_stick::compute_gyro_stick_delta(
                    &mut gyro_state,
                    &gyro_cfg,
                    &state,
                    gyro.yaw,
                    gyro.pitch,
                );
                let (rx, ry) = gyro_stick::blend_and_clamp(state.rx, state.ry, gdx, gdy);

                if let Err(e) =
                    emit_gamepad_state(&mut virtual_pad, &state, rx, ry, touchpad_cfg.mode)
                {
                    eprintln!("\nfailed to emit gamepad state: {e}");
                }

                if touchpad_cfg.mode == TouchpadMode::MouseRemap {
                    if let Some(mouse) = virtual_mouse.as_mut() {
                        if let Some((dx, dy)) = touchpad::compute_mouse_delta(
                            &mut touchpad_mouse_state,
                            &touchpad_cfg,
                            &state.finger1,
                        ) {
                            let result = mouse
                                .emit_rel(uinput_mouse::REL_X, dx)
                                .and_then(|_| mouse.emit_rel(uinput_mouse::REL_Y, dy))
                                .and_then(|_| mouse.sync());
                            if let Err(e) = result {
                                eprintln!("\nfailed to emit mouse motion: {e}");
                            }
                        }
                        // Touchpad click doubles as left mouse button in
                        // this mode, matching common DS4Windows touchpad
                        // remap defaults.
                        let click_result = mouse
                            .emit_key(uinput_mouse::BTN_LEFT, state.touchpad_click)
                            .and_then(|_| mouse.sync());
                        if let Err(e) = click_result {
                            eprintln!("\nfailed to emit mouse click: {e}");
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
