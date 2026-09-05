// Bluetooth verification tool. Given how much of ds4_bt.rs's byte layout
// is INFERRED (shifted from psdevwiki's documented table, not directly
// confirmed against a live hidraw capture the way every other protocol
// detail in this project was), this tool exists specifically to print
// raw bytes alongside parsed values so you can sanity-check them against
// real controller behavior before trusting the daemon to use this parser
// for real -- the same verification discipline that caught the touchpad
// offset bug earlier, applied proactively this time instead of after the
// fact.
//
// What to check when running this:
// 1. Does it connect at all, and does report ID 0x11 start appearing
//    (not stuck on 10-byte 0x01 reports)? If stuck, the calibration
//    handshake isn't taking effect.
// 2. Do CRC validations mostly succeed (occasional failures from radio
//    noise are normal; constant failures mean the CRC offset/algorithm
//    itself is wrong)?
// 3. Do LX/LY/RX/RY/buttons/dpad move correctly when you use the pad?
//    These are the highest-confidence offsets (corroborated by
//    psdevwiki's explicit field table, not just inferred byte shifts).
// 4. Does gyro data look sane (small values near 0 when the pad sits
//    still, matching the same test used for USB calibration)?
// 5. Does touchpad finger position track correctly? This is explicitly
//    flagged as the LEAST certain part of the BT parser -- expect this
//    to need a fix.
// 6. NEW: does batt:% look sane (moves slowly, doesn't jump around) and
//    does cable: correctly reflect whether USB is actually plugged in
//    (while still connected over BT)? Derived from kernel source
//    (buf[32], see ds4_bt.rs's parse_report_bt) and cross-checked
//    against this file's own already-verified touchpad offset, but not
//    yet confirmed against a live hidraw capture the way the offsets
//    above were.

use ds4l::ds4_bt::{read_calibration_bt, trigger_full_report_mode};
use ds4l::ds4_input::{calibrated_gyro_deg_s, SONY_VID};
use hidapi::HidApi;

// DS4 v2 Bluetooth PID differs from USB's 0x09CC in some capture
// references but community tooling generally reports the same PID over
// both transports for this revision; if this fails to connect, run
// `bluetoothctl` / check `lsusb`-equivalent BT device info and adjust
// this constant -- unlike the USB PID (directly confirmed when you first
// connected in Milestone 1), this one is not yet confirmed against your
// specific hardware over BT.
const DS4_V2_BT_PID: u16 = 0x09CC;

fn main() {
    let api = HidApi::new().expect("failed to init hidapi");

    println!(
        "Looking for DS4 v2 over Bluetooth (VID {:04X} PID {:04X})...",
        SONY_VID, DS4_V2_BT_PID
    );
    let device = api.open(SONY_VID, DS4_V2_BT_PID).unwrap_or_else(|e| {
        eprintln!(
            "Could not open device: {e}\n\
             Make sure the DS4 is paired and connected over Bluetooth \
             (check `bluetoothctl devices` / `bluetoothctl info <mac>`), \
             and that no other process (e.g. Steam) is already holding \
             the hidraw device open."
        );
        std::process::exit(1);
    });

    println!("Connected. Triggering full-report handshake (reading feature report 0x02)...");
    if let Err(e) = trigger_full_report_mode(&device) {
        eprintln!("Handshake failed: {e}");
        eprintln!("Will still attempt to read reports, but may stay stuck in truncated mode.");
    } else {
        println!("Handshake sent. Reports should switch from 0x01 (truncated) to 0x11 (full) now.");
    }

    println!("Reading calibration (feature report 0x02, BT layout)...");
    let cal = match read_calibration_bt(&device) {
        Ok(c) => {
            println!("Calibration parsed: {:#?}", c);
            println!(
                "CHECK: do these bias/scale values look plausible (non-zero bias is normal, \
                 scale should be a small positive number similar to what USB produced)?"
            );
            c
        }
        Err(e) => {
            eprintln!("Calibration read failed: {e}");
            eprintln!(
                "Continuing with uncalibrated gyro (will drift) so button/stick testing can proceed."
            );
            ds4l::ds4_input::GyroCalibration::default()
        }
    };

    println!("\nStreaming BT reports. Move the pad, press buttons, touch the touchpad.");
    println!("Ctrl+C to quit.\n");

    let mut buf = [0u8; 128]; // oversized vs the 78-byte report, headroom for safety
    let mut total_reads = 0u64;
    let mut crc_failures = 0u64;
    let mut valid_reports = 0u64;
    let rate_check_start = std::time::Instant::now();
    let mut last_rate_print = std::time::Instant::now();

    loop {
        match device.read_timeout(&mut buf, 100) {
            Ok(0) => {}
            Ok(len) => {
                total_reads += 1;

                if total_reads % 60 == 1 {
                    eprintln!(
                        "\n[raw dump] len={len} bytes={:02x?}",
                        &buf[..len.min(80)]
                    );
                }

                if buf[0] != ds4l::ds4_bt::DS4_INPUT_REPORT_BT_ID {
                    print!(
                        "\r[raw] report_id=0x{:02x} len={len} (not 0x11 yet - handshake may not have taken effect)          ",
                        buf[0]
                    );
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                    continue;
                }

                if len < ds4l::ds4_bt::DS4_INPUT_REPORT_BT_SIZE {
                    print!(
                        "\r[raw] 0x11 report but short: len={len} (expected {})          ",
                        ds4l::ds4_bt::DS4_INPUT_REPORT_BT_SIZE
                    );
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                    continue;
                }

                let report = &buf[..ds4l::ds4_bt::DS4_INPUT_REPORT_BT_SIZE];
                if !ds4l::ds4_bt::validate_crc(report) {
                    crc_failures += 1;
                    print!(
                        "\r[crc] FAILED ({crc_failures}/{total_reads} total) -- if this is constant, \
                         the CRC offset/algorithm is wrong, not just radio noise          "
                    );
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                    continue;
                }

                let state = ds4l::ds4_bt::parse_report_bt(report);
                let gyro = calibrated_gyro_deg_s(&state, &cal);
                valid_reports += 1;

                if last_rate_print.elapsed().as_secs_f64() >= 2.0 {
                    let elapsed = rate_check_start.elapsed().as_secs_f64();
                    let hz = valid_reports as f64 / elapsed;
                    eprintln!(
                        "\n[rate] {valid_reports} valid reports in {elapsed:.1}s = {hz:.1} Hz \
                         (USB is normally ~250Hz; if this is stuck near ~20Hz, that's a known \
                         DS4-over-BlueZ kernel-level report-rate cap, not a bug in this program)"
                    );
                    last_rate_print = std::time::Instant::now();
                }

                print!(
                    "\rLX:{:3} LY:{:3} RX:{:3} RY:{:3} dpad:{} △:{} ○:{} ×:{} □:{} \
                     gyro(deg/s) p:{:6.1} y:{:6.1} r:{:6.1} touch1(active:{} x:{} y:{}) \
                     batt:{:3}% chg:{} cable:{} \
                     crc_ok:{}/{}          ",
                    state.lx,
                    state.ly,
                    state.rx,
                    state.ry,
                    state.dpad,
                    state.triangle as u8,
                    state.circle as u8,
                    state.cross as u8,
                    state.square as u8,
                    gyro.pitch,
                    gyro.yaw,
                    gyro.roll,
                    state.finger1.touching as u8,
                    state.finger1.x,
                    state.finger1.y,
                    state.battery_percent,
                    state.battery_charging as u8,
                    state.cable_connected as u8,
                    total_reads - crc_failures,
                    total_reads,
                );
                use std::io::Write;
                std::io::stdout().flush().ok();
                // CHECK (new): does batt:% look like a sane battery
                // reading, moving down slowly over real time and NOT
                // jumping around erratically report-to-report? Does
                // cable: flip to 1 only when actually plugged in via
                // USB while still connected over BT (if you can test
                // that combination), and chg: go to 1 only while cable
                // is 1 and the battery hasn't hit "charge complete"? If
                // any of these look wrong, the buf[32] offset this
                // reads from (see ds4_bt.rs's parse_report_bt) needs a
                // second look -- it was derived from kernel source, not
                // confirmed against your specific hardware yet.
            }
            Err(e) => {
                eprintln!("\nread error: {e}");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
}
