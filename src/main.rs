// Milestone 1 tool: read a real DS4 v2, print parsed buttons/sticks/gyro.
// Logic now lives in the shared `ds4l::ds4_input` module (see src/ds4_input.rs)
// so Milestone 3+ binaries can reuse the exact same, hardware-verified parsing
// instead of duplicating it.

use ds4l::ds4_input::{calibrated_gyro_deg_s, open_and_calibrate, parse_report};
use hidapi::HidApi;
use std::time::Duration;

fn main() {
    let api = HidApi::new().expect("failed to init hidapi (is hidraw accessible? check udev rules)");

    println!("Connecting to DS4 v2 and reading calibration...");
    let (device, cal) = open_and_calibrate(&api).unwrap_or_else(|e| {
        eprintln!(
            "{e}\n\
             Is it plugged in via USB (not Bluetooth yet), and do you have \
             permission to access /dev/hidraw*? See udev rule note in README."
        );
        std::process::exit(1);
    });
    println!("Calibration loaded: {:#?}", cal);

    println!("\nStreaming input. Ctrl+C to quit.\n");

    let mut buf = [0u8; 64];
    loop {
        match device.read_timeout(&mut buf, 100) {
            Ok(0) => {
                // no data this tick, keep polling
            }
            Ok(len) if len >= 25 && buf[0] == 0x01 => {
                let state = parse_report(&buf);
                let (gx, gy, gz) = calibrated_gyro_deg_s(&state, &cal);

                print!(
                    "\rLX:{:3} LY:{:3} RX:{:3} RY:{:3} | dpad:{} \
                     △:{} ○:{} ×:{} □:{} L1:{} R1:{} L2:{:3} R2:{:3} | \
                     gyro(deg/s) x:{:7.1} y:{:7.1} z:{:7.1}   ",
                    state.lx,
                    state.ly,
                    state.rx,
                    state.ry,
                    state.dpad,
                    state.triangle as u8,
                    state.circle as u8,
                    state.cross as u8,
                    state.square as u8,
                    state.l1 as u8,
                    state.r1 as u8,
                    state.l2_analog,
                    state.r2_analog,
                    gx,
                    gy,
                    gz,
                );
                use std::io::Write;
                std::io::stdout().flush().ok();
            }
            Ok(_) => {
                // short/unexpected report, ignore
            }
            Err(e) => {
                eprintln!("\nread error: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}
