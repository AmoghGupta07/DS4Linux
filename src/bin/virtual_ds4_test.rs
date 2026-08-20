// Milestone 2 test binary. Creates a virtual DS4 (real VID/PID, correct
// axis/button layout) via raw uinput ioctls and sweeps both sticks in a
// circle continuously. This isolates and verifies the OUTPUT side only —
// no real controller involved yet.
//
// Verify with, in another terminal:
//   evtest            (pick the "Sony Interactive Entertainment Wireless
//                       Controller" device, watch ABS_X/Y/RX/RY cycle)
//   jstest /dev/input/jsN     (visual bar-graph version, easier to eyeball
//                               a clean circle)
//   sdl2-jstest --list        (confirms SDL recognizes VID 054c PID 09cc)

use ds4l::uinput_ds4::{VirtualDs4, ABS_RX, ABS_RY, ABS_X, ABS_Y};
use std::f64::consts::PI;
use std::time::Duration;

fn main() {
    println!("Creating virtual DS4 device via uinput...");
    let mut pad = VirtualDs4::create().unwrap_or_else(|e| {
        eprintln!(
            "Failed to create uinput device: {e}\n\
             Common causes:\n\
             - /dev/uinput needs root or a udev rule + group membership\n\
             - try: sudo modprobe uinput\n\
             - try: sudo chmod 666 /dev/uinput  (quick test only, not for real use)"
        );
        std::process::exit(1);
    });

    println!("Virtual DS4 created. Sweeping sticks in a circle. Ctrl+C to quit.");
    println!("Check `evtest` or `jstest` in another terminal now.");

    // DS4 native stick range: 0-255, 128 = center, so radius 100 keeps us
    // safely inside range without clipping at the edges.
    const CENTER: f64 = 128.0;
    const RADIUS: f64 = 100.0;

    let mut angle: f64 = 0.0;
    loop {
        let lx = (CENTER + RADIUS * angle.cos()).round() as i32;
        let ly = (CENTER + RADIUS * angle.sin()).round() as i32;
        // Right stick sweeps opposite direction so it's visually obvious
        // in evtest/jstest that RX/RY are distinct from X/Y, not just
        // mirrored values from a copy-paste bug.
        let rx = (CENTER + RADIUS * (-angle).cos()).round() as i32;
        let ry = (CENTER + RADIUS * (-angle).sin()).round() as i32;

        pad.emit_abs(ABS_X, lx).expect("emit ABS_X");
        pad.emit_abs(ABS_Y, ly).expect("emit ABS_Y");
        pad.emit_abs(ABS_RX, rx).expect("emit ABS_RX");
        pad.emit_abs(ABS_RY, ry).expect("emit ABS_RY");
        pad.sync().expect("sync");

        angle += 0.05;
        if angle > 2.0 * PI {
            angle -= 2.0 * PI;
        }

        std::thread::sleep(Duration::from_millis(16)); // ~60Hz
    }
}
