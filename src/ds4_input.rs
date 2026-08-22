//! DS4 v2 (USB) HID report parsing and gyro/accel calibration.
//! Extracted from the Milestone 1 test tool so later milestones (3+) can
//! reuse the exact same, already-verified-on-hardware parsing logic
//! instead of re-implementing it.

use hidapi::HidDevice;

pub const SONY_VID: u16 = 0x054C;
pub const DS4_V2_PID: u16 = 0x09CC;
// pub const DS4_V1_PID: u16 = 0x05C4; // uncomment / try this if v2 PID isn't found

/// Per-axis gyro calibration derived from feature report 0x02.
/// DS4 reports two "speed" samples (min and max angular rate) per axis at
/// known reference points, plus a bias, letting us convert raw ticks to
/// degrees/sec. This mirrors what DS4Windows / hid-sony do internally.
#[derive(Debug, Default, Clone, Copy)]
pub struct GyroCalibration {
    pub pitch_bias: i32,
    pub yaw_bias: i32,
    pub roll_bias: i32,
    pub pitch_scale: f64,
    pub yaw_scale: f64,
    pub roll_scale: f64,
    pub accel_x_bias: i32,
    pub accel_y_bias: i32,
    pub accel_z_bias: i32,
    pub accel_x_scale: f64,
    pub accel_y_scale: f64,
    pub accel_z_scale: f64,
}

impl GyroCalibration {
    /// A neutral (1:1, no bias) calibration used as a fallback when the
    /// feature report can't be read, so gyro output degrades gracefully
    /// (uncalibrated, will drift) rather than the program crashing.
    pub fn identity() -> Self {
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
}

/// One touchpad finger contact. x in [0, 1919], y in [0, 941] (DS4's
/// native touchpad resolution, confirmed against the kernel's
/// DS4_TOUCHPAD_WIDTH/HEIGHT constants). x/y are only meaningful when
/// `touching` is true.
///
/// NOTE: this parsing is NEW for this milestone and has NOT yet been
/// verified against real hardware the way buttons/sticks/gyro have been
/// across Milestones 1-3.5. Treat touching/x/y as unconfirmed until
/// tested -- offsets are taken directly from Linux kernel hid-sony.c
/// source (confirmed: "multi-touch trackpad data starts at offset 33 on
/// USB"), cross-checked against the community TouchFingerData struct
/// layout, but this exact codepath hasn't run against your DS4 v2 yet.
#[derive(Debug, Default, Clone, Copy)]
pub struct TouchFinger {
    pub touching: bool,
    pub contact_id: u8,
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PadState {
    pub lx: u8,
    pub ly: u8,
    pub rx: u8,
    pub ry: u8,
    pub dpad: u8,
    pub square: bool,
    pub triangle: bool,
    pub circle: bool,
    pub cross: bool,
    pub l1: bool,
    pub r1: bool,
    pub l2_digital: bool,
    pub r2_digital: bool,
    pub share: bool,
    pub options: bool,
    pub l3: bool,
    pub r3: bool,
    pub ps: bool,
    pub touchpad_click: bool,
    pub l2_analog: u8,
    pub r2_analog: u8,
    pub finger1: TouchFinger,
    pub finger2: TouchFinger,
    pub gyro_x: i16,
    pub gyro_y: i16,
    pub gyro_z: i16,
    pub accel_x: i16,
    pub accel_y: i16,
    pub accel_z: i16,
}

fn read_i16_le(buf: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes([buf[offset], buf[offset + 1]])
}

/// Parses one 4-byte touch finger block per the layout confirmed from
/// kernel source: byte0 bit7 = NOT touching (0 = touching), bits0-6 =
/// contact id; byte1 = X low 8 bits; byte2 low nibble = X high 4 bits,
/// high nibble = Y low 4 bits; byte3 = Y high 8 bits.
fn parse_touch_finger(b: &[u8]) -> TouchFinger {
    debug_assert_eq!(b.len(), 4);
    let touching = (b[0] & 0x80) == 0; // MSB=0 means finger IS touching
    let contact_id = b[0] & 0x7F;
    let x = (b[1] as u16) | (((b[2] & 0x0F) as u16) << 8);
    let y = ((b[2] as u16) >> 4) | ((b[3] as u16) << 4);
    TouchFinger {
        touching,
        contact_id,
        x,
        y,
    }
}

/// Reads and parses feature report 0x02 (37 bytes on USB) which contains
/// gyro/accel calibration constants baked into the controller at the factory.
/// Skipping this step is the #1 cause of "gyro drifts even when the pad is
/// sitting still" bugs.
pub fn read_calibration(device: &HidDevice) -> Result<GyroCalibration, String> {
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
    // 19-20 gyro speed plus (shared scale reference)
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

    // Real formula, matching Linux's hid-sony.c / hid-playstation.c exactly:
    //   sens_numer = (gyro_speed_plus + gyro_speed_minus) * DS4_GYRO_RES_PER_DEG_S
    //   sens_denom = axis_plus - axis_minus   (per-axis range)
    //   evdev_value = (raw - bias) * sens_numer / sens_denom
    // evdev_value is in units of 1/DS4_GYRO_RES_PER_DEG_S degree/s, so
    // real_deg_s = evdev_value / DS4_GYRO_RES_PER_DEG_S. Substituting and
    // cancelling DS4_GYRO_RES_PER_DEG_S algebraically:
    //   real_deg_s = (raw - bias) * (speed_plus + speed_minus) / sens_denom
    // Confirmed correct against real hardware readings (see conversation
    // history / calibration debug output from testing).
    let speed_sum = (gyro_speed_plus + gyro_speed_minus) as f64;

    let pitch_scale = speed_sum / ((pitch_plus - pitch_minus) as f64).abs().max(1.0);
    let yaw_scale = speed_sum / ((yaw_plus - yaw_minus) as f64).abs().max(1.0);
    let roll_scale = speed_sum / ((roll_plus - roll_minus) as f64).abs().max(1.0);

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

/// Parses a single USB input report (report ID 0x01) into a PadState.
/// Caller must ensure buf[0] == 0x01 and buf.len() >= 25 before calling.
/// Touchpad fields are only populated if buf.len() >= 41.
pub fn parse_report(buf: &[u8]) -> PadState {
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

    // Touchpad: the kernel patch's "trackpad data starts at offset 33"
    // refers to the START of the touchpad block, which begins with a
    // packet counter/timestamp byte (offset 33) and a second byte
    // (offset 34, purpose unclear/padding) BEFORE finger 1's actual
    // contact byte. This was confirmed against a second, independent
    // source (DsHidMini GitHub issue #11, citing psdevwiki): "the MSB of
    // Byte 35... finger 1 is in contact... MSB of Byte 39... finger 2."
    // My first version incorrectly treated offset 33 as finger 1's
    // contact byte directly, causing it to read the counter/timestamp
    // byte as if it were touch data -- explaining the "moving in a loop
    // while stationary" symptom (a free-running counter cycling through
    // its range looks exactly like that when misread as position bits).
    // Finger 1 = bytes 35-38, finger 2 = bytes 39-42.
    if buf.len() >= 43 {
        s.finger1 = parse_touch_finger(&buf[35..39]);
        s.finger2 = parse_touch_finger(&buf[39..43]);
    }

    s
}

/// Calibrated gyro angular velocity in degrees/second, one field per axis
/// so call sites can't silently transpose pitch/yaw/roll the way an
/// unlabeled tuple return would allow.
#[derive(Debug, Clone, Copy, Default)]
pub struct GyroDegPerSec {
    pub pitch: f64,
    pub yaw: f64,
    pub roll: f64,
}

/// Converts raw gyro ticks to degrees/second using calibration.
pub fn calibrated_gyro_deg_s(state: &PadState, cal: &GyroCalibration) -> GyroDegPerSec {
    GyroDegPerSec {
        pitch: (state.gyro_x as i32 - cal.pitch_bias) as f64 * cal.pitch_scale,
        yaw: (state.gyro_y as i32 - cal.yaw_bias) as f64 * cal.yaw_scale,
        roll: (state.gyro_z as i32 - cal.roll_bias) as f64 * cal.roll_scale,
    }
}

/// Opens the DS4 v2 over USB via hidapi and reads its calibration,
/// printing progress the same way Milestone 1's tool did. Shared by any
/// binary that needs a ready-to-read device handle.
pub fn open_and_calibrate(api: &hidapi::HidApi) -> Result<(HidDevice, GyroCalibration), String> {
    let mut device = api
        .open(SONY_VID, DS4_V2_PID)
        .map_err(|e| format!("could not open DS4 v2 (VID {SONY_VID:04X} PID {DS4_V2_PID:04X}): {e}"))?;

    device
        .set_blocking_mode(false)
        .map_err(|e| format!("failed to set non-blocking mode: {e}"))?;

    let cal = match read_calibration(&device) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("WARNING: {e} - gyro output will be uncalibrated (will drift).");
            GyroCalibration::identity()
        }
    };

    Ok((device, cal))
}

/// Sends a USB output report (0x05) to set rumble motors and/or lightbar
/// color/blink. Layout confirmed against two independent sources: the
/// actual hid-playstation.c kernel driver (dualshock4_output_report_common
/// struct, LKML patch series) and the Game Controller Collective wiki's
/// USBSetStateData struct, which agree byte-for-byte.
///
/// The controller only applies fields whose "enable" flag is set in
/// valid_flag0 -- this lets a single report update just rumble, just the
/// lightbar, or both, without disturbing whichever part isn't flagged.
pub struct OutputReport {
    pub rumble_weak: u8,   // "motor_right" in kernel naming
    pub rumble_strong: u8, // "motor_left" in kernel naming
    pub led_red: u8,
    pub led_green: u8,
    pub led_blue: u8,
    pub led_blink_on: u8,
    pub led_blink_off: u8,
    pub set_rumble: bool,
    pub set_led: bool,
}

impl Default for OutputReport {
    fn default() -> Self {
        OutputReport {
            rumble_weak: 0,
            rumble_strong: 0,
            led_red: 0,
            led_green: 0,
            led_blue: 0,
            led_blink_on: 0,
            led_blink_off: 0,
            set_rumble: false,
            set_led: false,
        }
    }
}

pub fn send_output_report(device: &HidDevice, report: &OutputReport) -> Result<(), String> {
    let mut buf = [0u8; 32]; // DS4_OUTPUT_REPORT_USB_SIZE per kernel source
    buf[0] = 0x05; // DS4_OUTPUT_REPORT_USB

    let mut valid_flag0: u8 = 0;
    if report.set_rumble {
        valid_flag0 |= 0x01; // EnableRumbleUpdate
    }
    if report.set_led {
        valid_flag0 |= 0x02; // EnableLedUpdate
        valid_flag0 |= 0x04; // EnableLedBlink -- always set alongside LED
                              // update so blink_on/off (0 = solid, no
                              // blink) is honored rather than ignored.
    }
    buf[1] = valid_flag0;
    // buf[2] = valid_flag1, unused for rumble/LED -- leave 0.
    // buf[3] = reserved -- leave 0.
    buf[4] = report.rumble_weak;
    buf[5] = report.rumble_strong;
    buf[6] = report.led_red;
    buf[7] = report.led_green;
    buf[8] = report.led_blue;
    buf[9] = report.led_blink_on;
    buf[10] = report.led_blink_off;

    device
        .write(&buf)
        .map_err(|e| format!("failed to write output report: {e}"))?;
    Ok(())
}
