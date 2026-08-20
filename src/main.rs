use hidapi::HidApi;
use std::time::Duration;

const SONY_VID: u16 = 0x054C;
const DS4_V2_PID: u16 = 0x09CC;
// const DS4_V1_PID: u16 = 0x05C4; // uncomment / try this if v2 PID isn't found

/// Per-axis gyro calibration derived from feature report 0x02.
/// DS4 reports two "speed" samples (min and max angular rate) per axis at
/// known reference points, plus a bias, letting us convert raw ticks to
/// degrees/sec. This mirrors what DS4Windows / hid-sony do internally.
#[derive(Debug, Default, Clone, Copy)]
struct GyroCalibration {
    pitch_bias: i32,
    yaw_bias: i32,
    roll_bias: i32,
    pitch_scale: f64,
    yaw_scale: f64,
    roll_scale: f64,
    accel_x_bias: i32,
    accel_y_bias: i32,
    accel_z_bias: i32,
    accel_x_scale: f64,
    accel_y_scale: f64,
    accel_z_scale: f64,
}

#[derive(Debug, Default, Clone, Copy)]
struct PadState {
    lx: u8,
    ly: u8,
    rx: u8,
    ry: u8,
    dpad: u8,
    square: bool,
    triangle: bool,
    circle: bool,
    cross: bool,
    l1: bool,
    r1: bool,
    l2_digital: bool,
    r2_digital: bool,
    share: bool,
    options: bool,
    l3: bool,
    r3: bool,
    ps: bool,
    touchpad_click: bool,
    l2_analog: u8,
    r2_analog: u8,
    gyro_x: i16,
    gyro_y: i16,
    gyro_z: i16,
    accel_x: i16,
    accel_y: i16,
    accel_z: i16,
}

fn read_i16_le(buf: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes([buf[offset], buf[offset + 1]])
}

/// Reads and parses feature report 0x02 (37 bytes on USB) which contains
/// gyro/accel calibration constants baked into the controller at the factory.
/// Skipping this step is the #1 cause of "gyro drifts even when the pad is
/// sitting still" bugs.
fn read_calibration(device: &hidapi::HidDevice) -> Result<GyroCalibration, String> {
    let mut buf = [0u8; 37];
    buf[0] = 0x02; // feature report ID

    let len = device
        .get_feature_report(&mut buf)
        .map_err(|e| format!("failed to read calibration feature report: {e}"))?;

    if len < 37 {
        return Err(format!("calibration report too short: got {len} bytes"));
    }

    // Layout (USB, report 0x02), all little-endian i16 unless noted:
    // 1-2   gyro pitch bias
    // 3-4   gyro yaw bias
    // 5-6   gyro roll bias
    // 7-8   gyro pitch plus (speed at +Y reference)
    // 9-10  gyro yaw plus
    // 11-12 gyro roll plus
    // 13-14 gyro pitch minus
    // 15-16 gyro yaw minus
    // 17-18 gyro roll minus
    // 19-20 gyro speed plus (shared scale reference, ~123 in raw ticks)
    // 21-22 gyro speed minus
    // 23-24 accel x plus
    // 25-26 accel x minus
    // 27-28 accel y plus
    // 29-30 accel y minus
    // 31-32 accel z plus
    // 33-34 accel z minus
    // 35-36 accel range (shared)
    let pitch_bias = read_i16_le(&buf, 1) as i32;
    let yaw_bias = read_i16_le(&buf, 3) as i32;
    let roll_bias = read_i16_le(&buf, 5) as i32;

    let pitch_plus = read_i16_le(&buf, 7) as i32;
    let yaw_plus = read_i16_le(&buf, 9) as i32;
    let roll_plus = read_i16_le(&buf, 11) as i32;

    let pitch_minus = read_i16_le(&buf, 13) as i32;
    let yaw_minus = read_i16_le(&buf, 15) as i32;
    let roll_minus = read_i16_le(&buf, 17) as i32;

    let gyro_speed_plus = read_i16_le(&buf, 19) as i32;
    let gyro_speed_minus = read_i16_le(&buf, 21) as i32;

    let accel_x_plus = read_i16_le(&buf, 23) as i32;
    let accel_x_minus = read_i16_le(&buf, 25) as i32;
    let accel_y_plus = read_i16_le(&buf, 27) as i32;
    let accel_y_minus = read_i16_le(&buf, 29) as i32;
    let accel_z_plus = read_i16_le(&buf, 31) as i32;
    let accel_z_minus = read_i16_le(&buf, 33) as i32;

    // Known reference angular rate used by Sony's calibration procedure,
    // matches the constant used by hid-sony / DS4Windows.
    const GYRO_SPEED_SCALE_DEG_S: f64 = 2000.0; // full range +/-2000 deg/s at extremes

    let gyro_speed_2x = (gyro_speed_plus - gyro_speed_minus) as f64;
    let pitch_scale = GYRO_SPEED_SCALE_DEG_S / ((pitch_plus - pitch_minus) as f64).max(1.0);
    let yaw_scale = GYRO_SPEED_SCALE_DEG_S / ((yaw_plus - yaw_minus) as f64).max(1.0);
    let roll_scale = GYRO_SPEED_SCALE_DEG_S / ((roll_plus - roll_minus) as f64).max(1.0);
    // gyro_speed_2x currently unused directly but kept for future cross-check /
    // logging since some units report slightly different plus/minus symmetry.
    let _ = gyro_speed_2x;

    const ACCEL_RANGE_G: f64 = 2.0; // +/-2g nominal range between plus/minus refs

    let accel_x_scale = (2.0 * ACCEL_RANGE_G) / ((accel_x_plus - accel_x_minus) as f64).max(1.0);
    let accel_y_scale = (2.0 * ACCEL_RANGE_G) / ((accel_y_plus - accel_y_minus) as f64).max(1.0);
    let accel_z_scale = (2.0 * ACCEL_RANGE_G) / ((accel_z_plus - accel_z_minus) as f64).max(1.0);

    Ok(GyroCalibration {
        pitch_bias,
        yaw_bias,
        roll_bias,
        pitch_scale,
        yaw_scale,
        roll_scale,
        accel_x_bias: (accel_x_plus + accel_x_minus) / 2,
        accel_y_bias: (accel_y_plus + accel_y_minus) / 2,
        accel_z_bias: (accel_z_plus + accel_z_minus) / 2,
        accel_x_scale,
        accel_y_scale,
        accel_z_scale,
    })
}

fn parse_report(buf: &[u8]) -> PadState {
    let mut s = PadState::default();

    s.lx = buf[1];
    s.ly = buf[2];
    s.rx = buf[3];
    s.ry = buf[4];

    s.dpad = buf[5] & 0x0F;
    s.square = buf[5] & 0x10 != 0;
    s.cross = buf[5] & 0x20 != 0;
    s.circle = buf[5] & 0x40 != 0;
    s.triangle = buf[5] & 0x80 != 0;

    s.l1 = buf[6] & 0x01 != 0;
    s.r1 = buf[6] & 0x02 != 0;
    // buf[6] bits 2/3 are L2/R2 "button" (digital trigger click), rarely used
    s.l2_digital = buf[6] & 0x04 != 0;
    s.r2_digital = buf[6] & 0x08 != 0;
    s.share = buf[6] & 0x10 != 0;
    s.options = buf[6] & 0x20 != 0;
    s.l3 = buf[6] & 0x40 != 0;
    s.r3 = buf[6] & 0x80 != 0;

    s.ps = buf[7] & 0x01 != 0;
    s.touchpad_click = buf[7] & 0x02 != 0;

    s.l2_analog = buf[8];
    s.r2_analog = buf[9];

    s.gyro_x = read_i16_le(buf, 13);
    s.gyro_y = read_i16_le(buf, 15);
    s.gyro_z = read_i16_le(buf, 17);

    s.accel_x = read_i16_le(buf, 19);
    s.accel_y = read_i16_le(buf, 21);
    s.accel_z = read_i16_le(buf, 23);

    s
}

/// Converts raw gyro ticks to degrees/second using calibration.
fn calibrated_gyro_deg_s(state: &PadState, cal: &GyroCalibration) -> (f64, f64, f64) {
    let x = (state.gyro_x as i32 - cal.pitch_bias) as f64 * cal.pitch_scale;
    let y = (state.gyro_y as i32 - cal.yaw_bias) as f64 * cal.yaw_scale;
    let z = (state.gyro_z as i32 - cal.roll_bias) as f64 * cal.roll_scale;
    (x, y, z)
}

fn main() {
    let api = HidApi::new().expect("failed to init hidapi (is hidraw accessible? check udev rules)");

    let device = api
        .open(SONY_VID, DS4_V2_PID)
        .unwrap_or_else(|_| {
            eprintln!(
                "Could not open DS4 v2 (VID {:04X} PID {:04X}). \
                 Is it plugged in via USB (not Bluetooth yet), and do you have \
                 permission to access /dev/hidraw*? See udev rule note below.",
                SONY_VID, DS4_V2_PID
            );
            std::process::exit(1);
        });

    device
        .set_blocking_mode(false)
        .expect("failed to set non-blocking mode");

    println!("Connected to DS4 v2. Reading calibration...");
    let cal = match read_calibration(&device) {
        Ok(c) => {
            println!("Calibration loaded: {:#?}", c);
            c
        }
        Err(e) => {
            eprintln!("WARNING: {e} - gyro output will be uncalibrated (will drift).");
            GyroCalibration {
                pitch_scale: 1.0,
                yaw_scale: 1.0,
                roll_scale: 1.0,
                accel_x_scale: 1.0,
                accel_y_scale: 1.0,
                accel_z_scale: 1.0,
                ..Default::default()
            }
        }
    };

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
