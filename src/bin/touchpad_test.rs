// Milestone 4 (+2-finger extension, +AbsoluteMouse): touchpad support,
// all three modes, up to 2 simultaneous fingers.
//
// - Passthrough mode: real finger positions -> virtual DS4's multitouch
//   axes (both slots), for games that read the DS4 touchpad natively.
// - MouseRemap mode: 1 finger drags the cursor; 2 fingers scroll instead
//   (vertical) and switch click-to-right-click -- confirmed DS4Windows
//   convention ("Two Finger Slide" = Scroll, 2-finger press = right
//   click), not an invented behavior.
// - AbsoluteMouse mode: touchpad position maps directly onto the screen
//   like a graphics tablet/touchscreen (uinput_absmouse.rs's
//   INPUT_PROP_DIRECT virtual device) -- NEW, not yet verified against
//   a live display server the same way MouseRemap/Passthrough were
//   confirmed here. This tool is the right place to check it: run with
//   TOUCHPAD_MODE = TouchpadMode::AbsoluteMouse, then confirm touching
//   different corners of the DS4 touchpad moves the cursor to the
//   corresponding corner of the actual screen.
//
// Everything from Milestone 3.5 (gyro-to-right-stick, passthrough for
// sticks/buttons/dpad/triggers) is unchanged and still active.
//
// Switch modes by editing TOUCHPAD_MODE below and rebuilding -- stand-in
// for the future per-profile setting, same pattern as gyro_stick_test.

use ds4l::ds4_input::{calibrated_gyro_deg_s, open_and_calibrate, parse_report, PadState};
use ds4l::gyro_stick::{self, GyroMode, GyroStickConfig, GyroStickState};
use ds4l::touchpad::{self, ClickButton, MouseAction, TouchpadConfig, TouchpadMode, TouchpadMouseState};
use ds4l::uinput_absmouse::{self, VirtualAbsMouse};
use ds4l::uinput_ds4::{self, VirtualDs4};
use ds4l::uinput_mouse::{self, VirtualMouse};
use hidapi::HidApi;
use std::time::Duration;

/// Edit this to switch modes: TouchpadMode::Passthrough,
/// TouchpadMode::MouseRemap, or TouchpadMode::AbsoluteMouse.
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
            0,
            state.finger1.touching,
            state.finger1.x as i32,
            state.finger1.y as i32,
        )?;
        pad.emit_touch(
            1,
            state.finger2.touching,
            state.finger2.x as i32,
            state.finger2.y as i32,
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
        Some(VirtualMouse::create(&[]).unwrap_or_else(|e| {
            eprintln!("Failed to create virtual mouse: {e}\nCheck /dev/uinput permissions.");
            std::process::exit(1);
        }))
    } else {
        None
    };

    // Same reasoning for the absolute pointer device: only created when
    // actually testing AbsoluteMouse mode.
    let mut virtual_absmouse = if TOUCHPAD_MODE == TouchpadMode::AbsoluteMouse {
        Some(VirtualAbsMouse::create().unwrap_or_else(|e| {
            eprintln!(
                "Failed to create virtual absolute pointer: {e}\nCheck /dev/uinput permissions."
            );
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
        TouchpadMode::AbsoluteMouse => {
            "ABSOLUTE-MOUSE (touchpad position -> screen position directly, tablet-style)"
        }
        TouchpadMode::Disabled => "DISABLED (no touchpad processing at all)",
    };
    println!(
        "\nTouchpad mode: {mode_desc}\n\
         Gyro-to-right-stick: {:?} (gate: L2)\n\
         2-finger support: MouseRemap = scroll + right-click, \
         Passthrough = both slots.\n\
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
                        let action = touchpad::compute_mouse_action(
                            &mut touchpad_mouse_state,
                            &touchpad_cfg,
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

                        // Click button depends on how many fingers are down
                        // at the moment of the click: 1 finger = left,
                        // 2 fingers = right (confirmed DS4Windows
                        // convention). Only fires the button matching the
                        // current finger count; the other stays released.
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

                if touchpad_cfg.mode == TouchpadMode::AbsoluteMouse {
                    if let Some(abs) = virtual_absmouse.as_mut() {
                        if let touchpad::AbsoluteMouseAction::Move { x, y } =
                            touchpad::compute_absolute_mouse_action(&state.finger1)
                        {
                            let move_result = abs
                                .emit_abs(uinput_absmouse::ABS_X, x)
                                .and_then(|_| abs.emit_abs(uinput_absmouse::ABS_Y, y))
                                .and_then(|_| abs.sync());
                            if let Err(e) = move_result {
                                eprintln!("\nfailed to emit absolute pointer position: {e}");
                            }
                        }

                        // Same 1-finger-left/2-finger-right convention
                        // as MouseRemap, for consistency -- see
                        // touchpad.rs's module doc comment on why this
                        // is our own choice, not an independently
                        // confirmed DS4Windows behavior for this
                        // specific mode.
                        let finger_count =
                            state.finger1.touching as u8 + state.finger2.touching as u8;
                        let click_target = touchpad::click_button_for_finger_count(finger_count);

                        let click_result = abs
                            .emit_key(
                                uinput_absmouse::BTN_LEFT,
                                state.touchpad_click && click_target == Some(ClickButton::Left),
                            )
                            .and_then(|_| {
                                abs.emit_key(
                                    uinput_absmouse::BTN_RIGHT,
                                    state.touchpad_click
                                        && click_target == Some(ClickButton::Right),
                                )
                            })
                            .and_then(|_| abs.sync());
                        if let Err(e) = click_result {
                            eprintln!("\nfailed to emit absolute pointer click: {e}");
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
