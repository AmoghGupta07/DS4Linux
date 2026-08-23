//! DS4 v2 Bluetooth support.
//!
//! Three things USB didn't need, confirmed against psdevwiki's exact
//! byte-level capture (the DS4-BT page, which includes literal hex dumps
//! of real reports cross-referenced against a documented field table) --
//! not inferred by analogy to USB:
//!
//! 1. A calibration-read handshake: a fresh BT connection sends only a
//!    truncated 10-byte report (0x01) until GET_FEATURE report 0x02 is
//!    read once, after which it switches to sending the full report
//!    0x11 (78 bytes on the wire, including 2 bytes of protocol framing
//!    that hidraw strips before userspace sees it -- see offset note
//!    below).
//! 2. CRC32 (IEEE polynomial) validation on every 0x11 report, computed
//!    over a synthetic seed byte (0xA1) plus the report bytes minus the
//!    trailing 4-byte CRC itself.
//! 3. A different byte layout than USB's 0x01 report -- shifted, with
//!    extra header/timestamp/battery bytes not present on USB.
//!
//! IMPORTANT CAVEAT: the offset table below was derived by taking
//! psdevwiki's documented byte table (which includes 2 bytes of raw
//! Bluetooth transport framing before the report ID that hidraw is
//! expected to strip) and shifting every offset down by 2 to match what
//! hidraw should hand userspace. This shift is inference, not something
//! directly confirmed against a live hidraw capture -- unlike every
//! other protocol detail in this project so far, which was verified
//! either against kernel source directly or against your own hardware's
//! output. Milestone testing MUST include printing raw bytes from a real
//! BT connection and checking them against this table before trusting
//! parsed values, the same way the touchpad offset bug was actually
//! caught and fixed.

use crate::ds4_input::{GyroCalibration, OutputReport, PadState, TouchFinger};
use hidapi::HidDevice;

pub const DS4_INPUT_REPORT_BT_ID: u8 = 0x11;
/// Full report size as hidraw should present it: report ID + payload +
/// 4-byte trailing CRC. 78 bytes matches the kernel's
/// DS4_INPUT_REPORT_0x11_SIZE constant referenced in hid-sony.c's CRC
/// validation code (confirmed earlier against kernel source).
pub const DS4_INPUT_REPORT_BT_SIZE: usize = 78;

/// Feature report that must be read once after connecting to unlock the
/// full 0x11 report stream -- before this, the controller only sends a
/// truncated 10-byte 0x01 report with no gyro/touchpad/battery data.
/// Confirmed via Game Controller Collective wiki: "Reading calibration
/// is required to switch input reports from the truncated 0x01 report
/// to the expanded 0x11-0x19 reports."
pub const DS4_CALIBRATION_FEATURE_BT_ID: u8 = 0x02;
/// CORRECTED based on real hardware: initial assumption of 41 bytes was
/// wrong -- actual capture showed 37 bytes, identical to USB's
/// calibration report size. This suggests hidapi/the kernel's BT HID
/// implementation normalizes this particular feature report to the same
/// size as USB rather than using a BT-specific longer layout, contrary
/// to what the psdevwiki capture (of a different report family, 0x02
/// FEATURE under a different context) suggested. Treating the internal
/// layout as IDENTICAL to USB's read_calibration function is now the
/// working hypothesis -- test this explicitly (see calibration debug
/// output) since it's a revised assumption, not yet independently
/// re-confirmed the way the CRC fix was.
pub const DS4_CALIBRATION_FEATURE_BT_SIZE: usize = 37;

/// Reads GET_FEATURE report 0x02 once, which is the documented trigger
/// that switches the controller from sending truncated 0x01 reports to
/// full 0x11 reports over Bluetooth. Must be called once after opening
/// a BT connection, before attempting to read 0x11 reports -- skipping
/// this means the daemon will only ever see 10-byte reports with no
/// gyro/touchpad/battery data, silently.
pub fn trigger_full_report_mode(device: &HidDevice) -> Result<(), String> {
    let mut buf = [0u8; DS4_CALIBRATION_FEATURE_BT_SIZE];
    buf[0] = DS4_CALIBRATION_FEATURE_BT_ID;
    device
        .get_feature_report(&mut buf)
        .map_err(|e| format!("failed to read BT calibration feature report: {e}"))?;
    Ok(())
}

/// Validates a Bluetooth input report's CRC32, per hid-sony.c's
/// documented algorithm (confirmed against kernel source earlier):
/// seed with a synthetic 0xA1 "header" byte (representing the HID
/// transport's DATA|INPUT byte, which hidraw itself doesn't include but
/// the CRC was computed including it on the wire), then hash the report
/// bytes minus the trailing 4-byte CRC, compare against the CRC stored
/// in those trailing 4 bytes (little-endian).
pub fn validate_crc(report: &[u8]) -> bool {
    if report.len() < 4 {
        return false;
    }
    let (payload, crc_bytes) = report.split_at(report.len() - 4);
    let stored_crc = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);

    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&[0xA1u8]);
    hasher.update(payload);
    let computed = hasher.finalize();
    // NOTE: earlier version applied `!` (negation) here based on reading
    // the kernel's C source as `crc = ~crc32_le(...)`, but the C kernel
    // code's own crc32_le() helper already applies the final XOR
    // internally as part of its own convention -- so re-negating in Rust
    // (where crc32fast::Hasher::finalize() already returns the standard,
    // fully-finalized CRC32) double-inverted the result. Confirmed
    // against a real captured Bluetooth report from actual hardware:
    // this un-negated form matches the stored CRC exactly, the negated
    // form does not.

    computed == stored_crc
}

fn read_i16_le(buf: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes([buf[offset], buf[offset + 1]])
}

/// Parses a validated Bluetooth 0x11 report into the same PadState USB
/// parsing produces, so every downstream module (gyro, touchpad, KBM,
/// profiles) works identically regardless of connection type.
///
/// Caller must validate CRC (validate_crc) before calling this -- a
/// corrupted report parsed as if valid could produce nonsensical input.
///
/// OFFSET CAVEAT: see module doc comment. These offsets are derived by
/// shifting psdevwiki's documented table down by 2 bytes (removing the
/// raw transport framing hidraw is expected to strip). Verify against
/// real hardware before trusting this fully.
pub fn parse_report_bt(buf: &[u8]) -> PadState {
    let mut s = PadState::default();

    s.lx = buf[3];
    s.ly = buf[4];
    s.rx = buf[5];
    s.ry = buf[6];

    s.dpad = buf[7] & 0x0F;
    s.square = buf[7] & 0x10 != 0;
    s.cross = buf[7] & 0x20 != 0;
    s.circle = buf[7] & 0x40 != 0;
    s.triangle = buf[7] & 0x80 != 0;

    s.l1 = buf[8] & 0x01 != 0;
    s.r1 = buf[8] & 0x02 != 0;
    s.l2_digital = buf[8] & 0x04 != 0;
    s.r2_digital = buf[8] & 0x08 != 0;
    s.share = buf[8] & 0x10 != 0;
    s.options = buf[8] & 0x20 != 0;
    s.l3 = buf[8] & 0x40 != 0;
    s.r3 = buf[8] & 0x80 != 0;

    s.ps = buf[9] & 0x01 != 0;
    s.touchpad_click = buf[9] & 0x02 != 0;

    s.l2_analog = buf[10];
    s.r2_analog = buf[11];

    // buf[12-13]: timestamp -- not currently used, kept for future
    // reference (would matter for precise gyro integration timing).
    // buf[14]: battery -- not currently exposed to PadState; a natural
    // follow-up once this milestone is confirmed working.

    s.gyro_x = read_i16_le(buf, 15);
    s.gyro_y = read_i16_le(buf, 17);
    s.gyro_z = read_i16_le(buf, 19);

    s.accel_x = read_i16_le(buf, 21);
    s.accel_y = read_i16_le(buf, 23);
    s.accel_z = read_i16_le(buf, 25);

    // Touchpad: first touch-report block starts after the "number of
    // trackpad packets" (buf[35]) and first packet counter (buf[36]).
    // Only the first of up to 4 packed sub-reports is read here, matching
    // the existing single-report-per-poll approach used on USB. Each
    // finger is 4 bytes (corrected from an earlier 3-byte version that
    // truncated Y -- see parse_touch_finger_bt's doc comment), so
    // finger2 starts 4 bytes after finger1, not 3.
    if buf.len() >= 45 {
        s.finger1 = parse_touch_finger_bt(&buf[37..41]);
        s.finger2 = parse_touch_finger_bt(&buf[41..45]);
    }

    s
}

/// BT touch finger encoding per psdevwiki's table: 1-byte "active low +
/// id" byte followed by 2 bytes of packed X/Y (documented there as a
/// combined "finger coordinates" field, less explicitly bit-mapped than
/// USB's table). This mirrors USB's 12-bit-X/12-bit-Y packing as a best
/// match, but is the single least-certain offset/format in this
/// milestone -- verify against real touchpad interaction over BT before
/// trusting finger coordinates; everything else (buttons, sticks, gyro)
/// is much higher confidence since it's corroborated by a direct field
/// table, not just a byte count.
/// Fixed: Y was truncating to 4 bits (max value 15) because this only
/// read a 3-byte block per finger, missing the 4th byte that supplies
/// Y's high bits. Real-hardware testing caught this immediately (Y
/// capped at 15 is a dead giveaway for a stray 4-bit field). Now uses
/// the same 4-byte-per-finger packing already proven correct for USB
/// (12-bit X, 12-bit Y split across bytes 1-3), which makes sense since
/// dsremap's docs describe the BT touchpad payload as "identical in
/// structure to the USB input reports 0x01" -- this was already the
/// right assumption, I just implemented an incomplete version of it the
/// first time.
fn parse_touch_finger_bt(b: &[u8]) -> TouchFinger {
    debug_assert_eq!(b.len(), 4);
    let touching = (b[0] & 0x80) == 0;
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

/// Reads and validates one Bluetooth input report from the device,
/// returning a parsed PadState only if the CRC checks out. Returns
/// Ok(None) for non-fatal cases (short/wrong-ID reports, likely still in
/// truncated mode or a different report type came through) so the caller
/// can just skip that poll cycle rather than treat it as an error.
pub fn read_bt_report(device: &HidDevice, buf: &mut [u8]) -> Result<Option<PadState>, String> {
    let len = device
        .read_timeout(buf, 100)
        .map_err(|e| format!("BT read error: {e}"))?;

    if len == 0 {
        return Ok(None);
    }
    if buf[0] != DS4_INPUT_REPORT_BT_ID {
        // Likely still in truncated 0x01 mode (handshake not yet
        // triggered or not yet taken effect) or an unrelated report.
        return Ok(None);
    }
    if len < DS4_INPUT_REPORT_BT_SIZE {
        return Ok(None);
    }

    let report = &buf[..DS4_INPUT_REPORT_BT_SIZE];
    if !validate_crc(report) {
        // Corrupted over the air -- drop this report rather than trust
        // possibly-garbled button/stick data. Silent drop is intentional
        // for now; revisit if drops turn out to be frequent enough to
        // need surfacing to the person.
        return Ok(None);
    }

    Ok(Some(parse_report_bt(report)))
}

/// Reads BT calibration by reusing USB's exact calibration parser
/// (`ds4_input::read_calibration`), now that real hardware confirms the
/// BT feature report 0x02 is the same 37-byte size as USB's. Rather than
/// maintain a second, separately-unverified offset table for a report
/// that turns out to be byte-identical in size, we just call the
/// already-correct USB function directly -- if BT's internal field
/// layout ever turns out to differ despite matching size, this is the
/// one place to fix it, and it'll show up as implausible bias/scale
/// values the same way the original USB calibration bug was caught.
pub fn read_calibration_bt(device: &HidDevice) -> Result<GyroCalibration, String> {
    crate::ds4_input::read_calibration(device)
}

/// Sends a Bluetooth output report (0x11) to set rumble/lightbar, reusing
/// the same OutputReport struct USB uses so callers don't need a
/// transport-specific type.
///
/// CORRECTED against a real, hardware-confirmed working reference: a
/// Noctalia shell plugin (ds4-color) whose Lua source was provided
/// directly and diffed byte-for-byte against this function. Three real
/// bugs were found and fixed:
///
/// 1. CRC seed byte is 0xA2, not 0xA1. Input reports use 0xA1 (HID
///    transaction type DATA|INPUT), confirmed correct earlier against
///    real captured input bytes -- but OUTPUT reports use 0xA2
///    (DATA|OUTPUT), a different transaction type byte. Using the wrong
///    seed produces a CRC the controller's firmware rejects, which
///    explains the observed symptom: LED briefly shows the requested
///    color then reverts to the firmware's own fault-indicator red.
/// 2. The CRC must be negated (~) after hashing -- opposite of what was
///    correct for input report validation. Confirmed by reproducing the
///    reference's exact CRC computation in Python and matching its
///    output.
/// 3. Byte 1 is a fixed 0xC4 (labelled DS4_BT_HW_CONTROL in the
///    reference), not the 0x80 poll-rate-unlock byte I'd carried over
///    from a different context (input report handling). Field offsets
///    also corrected: valid_flag0 at byte 3, lightbar RGB at bytes 8/9/10
///    (not 6/8/9/10 as before).
///
/// Rumble motor byte positions are NOT independently confirmed by this
/// reference (it only ever sets LED, never rumble) -- kept at a
/// best-guess position consistent with the corrected offsets, but treat
/// rumble-over-BT with more caution than the now-confirmed LED path
/// until tested.
pub fn send_output_report_bt(device: &HidDevice, report: &OutputReport) -> Result<(), String> {
    let mut buf = [0u8; 78]; // matches DS4_INPUT_REPORT_BT_SIZE / the reference's DS4_BT_REPORT_LEN
    buf[0] = 0x11; // BT output report ID
    buf[1] = 0xC4; // fixed header byte, confirmed via working reference (was wrongly 0x80)
    buf[2] = 0x00;

    let mut valid_flag0: u8 = 0;
    if report.set_rumble {
        valid_flag0 |= 0x01;
    }
    if report.set_led {
        valid_flag0 |= 0x02; // confirmed: DS4_OUTPUT_VALID_FLAG0_LED = 0x02
    }
    buf[3] = valid_flag0;

    // Rumble position: unconfirmed by the reference (LED-only), kept
    // adjacent to the confirmed valid_flag0 offset as a reasonable
    // extrapolation consistent with USB's own field ordering
    // (rumble immediately follows valid_flag0/reserved there too).
    buf[6] = report.rumble_weak;
    buf[7] = report.rumble_strong;

    // Confirmed lightbar offsets from the reference: R=8, G=9, B=10.
    buf[8] = report.led_red;
    buf[9] = report.led_green;
    buf[10] = report.led_blue;
    // led_blink_on/off intentionally not set -- the reference doesn't
    // use them either, and their exact confirmed offset is unknown.

    // CRC32: seed with 0xA2 (not 0xA1 -- see doc comment above), hash
    // bytes [0..74), then NEGATE the result (opposite of input report
    // validation) before storing little-endian in the last 4 bytes.
    // Verified by reproducing this exact computation in Python against
    // the working reference's algorithm and confirming matching output.
    let payload_end = buf.len() - 4;
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&[0xA2u8]);
    hasher.update(&buf[..payload_end]);
    let crc = !hasher.finalize();
    buf[payload_end..].copy_from_slice(&crc.to_le_bytes());

    device
        .write(&buf)
        .map_err(|e| format!("failed to write BT output report: {e}"))?;
    Ok(())
}
