// Milestone 3: real DS4 v2 input -> virtual DS4 output, 1:1, no remapping.
//
// This is the first milestone that actually behaves like a (minimal)
// working replica: move the real stick, the virtual stick moves; press a
// real button, the virtual button fires. Test this in `evtest`/`jstest`
// first, then in an actual game or Steam Big Picture.
//
// Deliberately NOT doing yet: gyro-to-stick blending, touchpad handling,
// profiles, LED/rumble. Those build on top of this once passthrough is
// confirmed solid.

use ds4l::ds4_input::{open_and_calibrate, parse_report, PadState};
use ds4l::uinput_ds4::{self, VirtualDs4};
use hidapi::HidApi;
use std::time::Duration;

/// Sends one PadState to the virtual pad. Buttons and sticks/triggers use
/// DS4's native ranges directly (0-255 for sticks/triggers), matching what
/// Milestone 2's uinput setup expects -- no conversion needed, which was
/// the point of designing both sides around the same raw byte ranges.
fn emit_state(pad: &mut VirtualDs4, state: &PadState) -> std::io::Result<()> {
    pad.emit_abs(uinput_ds4::ABS_X, state.lx as i32)?;
    pad.emit_abs(uinput_ds4::ABS_Y, state.ly as i32)?;
    pad.emit_abs(uinput_ds4::ABS_RX, state.rx as i32)?;
    pad.emit_abs(uinput_ds4::ABS_RY, state.ry as i32)?;
    pad.emit_abs(uinput_ds4::ABS_Z, state.l2_analog as i32)?;
    pad.emit_abs(uinput_ds4::ABS_RZ, state.r2_analog as i32)?;

    // D-pad nibble (buf[5] & 0x0F) uses the standard DS4 8-way encoding:
    // 0=N,1=NE,2=E,3=SE,4=S,5=SW,6=W,7=NW,8=released. Convert to two
    // -1/0/1 hat axes for uinput.
    let (hat_x, hat_y) = match state.dpad {
        0 => (0, -1),  // N
        1 => (1, -1),  // NE
        2 => (1, 0),   // E
        3 => (1, 1),   // SE
        4 => (0, 1),   // S
        5 => (-1, 1),  // SW
        6 => (-1, 0),  // W
        7 => (-1, -1), // NW
        _ => (0, 0),   // released (8) or invalid
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
    let (device, _cal) = open_and_calibrate(&api).unwrap_or_else(|e| {
        eprintln!(
            "{e}\n\
             Is it plugged in via USB, and do you have permission to access \
             /dev/hidraw*? See README udev rule setup."
        );
        std::process::exit(1);
    });
    println!("Real DS4 connected. (Gyro calibration loaded but unused this milestone.)");

    println!("Creating virtual DS4 via uinput...");
    let mut virtual_pad = VirtualDs4::create().unwrap_or_else(|e| {
        eprintln!(
            "Failed to create uinput device: {e}\n\
             Check /dev/uinput permissions -- see README."
        );
        std::process::exit(1);
    });
    println!("Virtual DS4 created.");

    println!(
        "\nPassthrough active: move the real pad, the virtual pad follows 1:1.\n\
         Check `evtest` / `jstest` / a game now. Ctrl+C to quit.\n"
    );

    let mut buf = [0u8; 64];
    loop {
        match device.read_timeout(&mut buf, 100) {
            Ok(len) if len >= 25 && buf[0] == 0x01 => {
                let state = parse_report(&buf);
                if let Err(e) = emit_state(&mut virtual_pad, &state) {
                    eprintln!("\nfailed to emit to virtual pad: {e}");
                }
            }
            Ok(_) => {
                // no data / short or unexpected report this tick, keep polling
            }
            Err(e) => {
                eprintln!("\nreal pad read error: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}
