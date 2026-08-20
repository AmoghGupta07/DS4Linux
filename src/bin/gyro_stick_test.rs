// Milestone 3.5: gyro-to-right-stick blending, additive, three selectable
// modes (always-on / toggle / hold), gated by L2 for toggle/hold.
//
// Everything else is identical to passthrough_test.rs -- left stick,
// buttons, dpad, triggers all pass through 1:1. Only the right stick gets
// the gyro contribution blended in before being sent to the virtual pad.
//
// Try all three modes by editing MODE below and rebuilding -- this will
// become a per-profile setting once the profile system exists, but for
// now it's a one-line const so you can quickly A/B the feel of each.

use ds4l::ds4_input::{calibrated_gyro_deg_s, open_and_calibrate, parse_report, PadState};
use ds4l::gyro_stick::{self, GyroMode, GyroStickConfig, GyroStickState};
use ds4l::uinput_ds4::{self, VirtualDs4};
use hidapi::HidApi;
use std::time::Duration;

/// Change this to try the other modes: GyroMode::AlwaysOn, GyroMode::Toggle,
/// GyroMode::Hold. Gate button is L2 for Toggle/Hold (ignored for AlwaysOn).
const MODE: GyroMode = GyroMode::Hold;

fn emit_state(
    pad: &mut VirtualDs4,
    state: &PadState,
    right_x: u8,
    right_y: u8,
) -> std::io::Result<()> {
    pad.emit_abs(uinput_ds4::ABS_X, state.lx as i32)?;
    pad.emit_abs(uinput_ds4::ABS_Y, state.ly as i32)?;
    // Right stick uses the gyro-blended values instead of the raw real
    // stick -- this is the only line that differs from passthrough_test.
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
        eprintln!("Failed to create uinput device: {e}\nCheck /dev/uinput permissions.");
        std::process::exit(1);
    });
    println!("Virtual DS4 created.");

    let gyro_cfg = GyroStickConfig {
        mode: MODE,
        deg_per_sec_at_full_stick: 10000.0,
        ..Default::default()
    };
    let mut gyro_state = GyroStickState::default();

    let mode_desc = match MODE {
        GyroMode::AlwaysOn => "ALWAYS-ON (gyro constantly active)",
        GyroMode::Toggle => "TOGGLE (press L2 to flip gyro on/off)",
        GyroMode::Hold => "HOLD (hold L2 to activate gyro)",
    };
    println!(
        "\nGyro-to-right-stick active, mode: {mode_desc}\n\
         Sensitivity: {:.0} deg/s = full stick deflection.\n\
         Left stick, buttons, dpad, triggers pass through unchanged.\n\
         Ctrl+C to quit.\n",
        gyro_cfg.deg_per_sec_at_full_stick
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

                if let Err(e) = emit_state(&mut virtual_pad, &state, rx, state.ry) {
                    eprintln!("\nfailed to emit to virtual pad: {e}");
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
