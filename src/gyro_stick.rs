//! Gyro-to-right-stick blending.
//!
//! Converts calibrated gyro angular velocity (deg/s, from ds4_input) into a
//! stick displacement, applies sensitivity, and additively blends it onto
//! the real right stick value -- clamped to the stick's circular range so
//! diagonal aim doesn't get an unintended speed boost from naive per-axis
//! clamping.
//!
//! This module is deliberately independent of the eventual profile/config
//! system: `GyroStickConfig` is exactly the shape a profile loader will
//! deserialize into later, so wiring in TOML profiles down the line is a
//! matter of parsing into this struct, not restructuring this logic.

use crate::ds4_input::PadState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GyroMode {
    AlwaysOn,
    Toggle,
    Hold,
    /// Gyro-to-stick blending never contributes anything, ever --
    /// distinct from "Hold with a gate you never press," which still
    /// runs smoothing/gate-tracking every report for no purpose.
    /// Disabled skips all of that outright. Doesn't affect
    /// gyro_passthrough (Profile-level, exposes raw gyro to OTHER
    /// software via the kernel's Motion Sensors device) -- unrelated
    /// concerns that happen to share the same sensor.
    Disabled,
}

/// Which button gates gyro activation in Toggle/Hold mode. Previously
/// hardcoded to L2 -- some games' natural aim-down-sights/modifier
/// button is something else (L1 is common for "aim" in several
/// shooters), so this needed to be a per-profile choice, not a fixed
/// constant.
///
/// Analog triggers (L2/R2) are gated by GATE_PRESS_THRESHOLD, same
/// threshold and reasoning as before this became configurable; every
/// other option here is a plain digital button read straight off
/// PadState.
///
/// L2/R2 vs L2Digital/R2Digital: these are genuinely different signals,
/// not two names for the same thing. `L2`/`R2` here gate on the ANALOG
/// trigger value crossing a software threshold (GATE_PRESS_THRESHOLD) --
/// the original, pre-existing behavior, kept as-is for backward
/// compatibility (see GateButton::default() below). `L2Digital`/
/// `R2Digital` instead read the DS4's own hardware trigger-click bit
/// (`pad.l2_digital`/`pad.r2_digital`, the same signal
/// gamepad_remap.rs's "L2 (digital click)" row remaps) directly -- a
/// real, independent bit the controller reports when a trigger is
/// pressed to its full mechanical end, distinct from "analog value
/// happens to be past some threshold we picked." Some people may
/// prefer the crisper, hardware-defined feel of the digital bit over a
/// tunable software threshold; both are offered rather than one
/// replacing the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateButton {
    L1,
    R1,
    L2,
    R2,
    L2Digital,
    R2Digital,
    L3,
    R3,
    Cross,
    Circle,
    Square,
    Triangle,
    Share,
    Options,
    Ps,
    TouchpadClick,
}

impl Default for GateButton {
    fn default() -> Self {
        // L2 -- matches this project's original hardcoded behavior, so
        // existing profile TOML files saved before this field existed
        // (and therefore missing `gate_button` under `[gyro]`) keep
        // behaving exactly as they did before, rather than silently
        // changing which button controls gyro on upgrade.
        GateButton::L2
    }
}

impl GateButton {
    fn is_pressed(self, pad: &PadState) -> bool {
        match self {
            GateButton::L1 => pad.l1,
            GateButton::R1 => pad.r1,
            GateButton::L2 => pad.l2_analog >= GATE_PRESS_THRESHOLD,
            GateButton::R2 => pad.r2_analog >= GATE_PRESS_THRESHOLD,
            GateButton::L2Digital => pad.l2_digital,
            GateButton::R2Digital => pad.r2_digital,
            GateButton::L3 => pad.l3,
            GateButton::R3 => pad.r3,
            GateButton::Cross => pad.cross,
            GateButton::Circle => pad.circle,
            GateButton::Square => pad.square,
            GateButton::Triangle => pad.triangle,
            GateButton::Share => pad.share,
            GateButton::Options => pad.options,
            GateButton::Ps => pad.ps,
            GateButton::TouchpadClick => pad.touchpad_click,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GyroStickConfig {
    pub mode: GyroMode,
    /// Which button gates activation in Toggle/Hold mode (ignored for
    /// AlwaysOn). `#[serde(default)]` so profile files saved before this
    /// field existed still load, falling back to GateButton::default()
    /// (L2) -- same backward-compatibility pattern profile.rs already
    /// uses for Ds4FeedbackConfig/OutputMode.
    #[serde(default)]
    pub gate_button: GateButton,
    /// Degrees/sec of gyro rotation that maps to full stick deflection.
    /// Lower = more sensitive (less rotation needed for max stick output).
    /// This is the "moderate, DS4Windows-like default" starting point --
    /// tune by feel from here.
    pub deg_per_sec_at_full_stick: f64,
    /// Simple exponential moving average factor for smoothing raw gyro
    /// jitter before it's used, in [0.0, 1.0]. Higher = less smoothing
    /// (more responsive but jitterier), lower = smoother but laggier.
    pub smoothing_alpha: f64,
    /// Small deadzone in deg/s below which gyro input is ignored, to
    /// avoid drift/noise causing stick creep when the pad is held still.
    pub deadzone_deg_s: f64,
}

impl Default for GyroStickConfig {
    fn default() -> Self {
        GyroStickConfig {
            mode: GyroMode::Hold,
            gate_button: GateButton::default(),
            // Moderate starting point: ~120 deg/s of wrist rotation for
            // full stick deflection is a common comfortable default in
            // DS4Windows-style setups -- fast flicks reach max easily,
            // slow aiming stays controllable.
            deg_per_sec_at_full_stick: 120.0,
            smoothing_alpha: 0.35,
            deadzone_deg_s: 2.0,
        }
    }
}

/// Persistent state carried between report reads: the smoothed gyro value
/// and the toggle latch. Lives across the whole session, not per-report.
#[derive(Debug, Default)]
pub struct GyroStickState {
    smoothed_yaw: f64,
    smoothed_pitch: f64,
    toggle_active: bool,
    /// Tracks previous gate-button "pressed" state so toggle mode only
    /// flips on the rising edge (press), not every report while held.
    prev_gate_pressed: bool,
}

/// Analog trigger value (0-255) above which L2/R2 count as "held" when
/// configured as the gate button -- roughly 50% press, matching
/// DS4Windows' treatment of analog buttons used as digital gates.
const GATE_PRESS_THRESHOLD: u8 = 128;

/// Given the current real pad state, calibrated gyro deg/s, and config,
/// returns the (dx, dy) stick displacement gyro should contribute this
/// frame, already smoothed and gated by mode -- but NOT yet blended with
/// the real stick or clamped to the circle (caller does that, since it
/// needs the real stick value too).
pub fn compute_gyro_stick_delta(
    state: &mut GyroStickState,
    cfg: &GyroStickConfig,
    pad: &PadState,
    gyro_yaw_deg_s: f64,
    gyro_pitch_deg_s: f64,
) -> (f64, f64) {
    // Disabled skips everything below outright -- no gate tracking, no
    // smoothing, no computation at all -- rather than running all of
    // that every report just to discard the result, which is what
    // "Hold with an unreachable gate" would otherwise cost for the same
    // observable behavior.
    if cfg.mode == GyroMode::Disabled {
        return (0.0, 0.0);
    }

    let gate_pressed = cfg.gate_button.is_pressed(pad);

    let active = match cfg.mode {
        GyroMode::AlwaysOn => true,
        GyroMode::Hold => gate_pressed,
        GyroMode::Toggle => {
            // Flip the latch only on the rising edge (press), so holding
            // the trigger down doesn't rapidly re-toggle.
            if gate_pressed && !state.prev_gate_pressed {
                state.toggle_active = !state.toggle_active;
            }
            state.toggle_active
        }
        GyroMode::Disabled => unreachable!("handled by the early return above"),
    };
    state.prev_gate_pressed = gate_pressed;

    // Smooth the raw gyro signal even when inactive, so there's no sudden
    // jump from a stale smoothed value the instant gyro activates.
    state.smoothed_yaw =
        cfg.smoothing_alpha * gyro_yaw_deg_s + (1.0 - cfg.smoothing_alpha) * state.smoothed_yaw;
    state.smoothed_pitch = cfg.smoothing_alpha * gyro_pitch_deg_s
        + (1.0 - cfg.smoothing_alpha) * state.smoothed_pitch;

    if !active {
        return (0.0, 0.0);
    }

    let yaw = apply_deadzone(state.smoothed_yaw, cfg.deadzone_deg_s);
    let pitch = apply_deadzone(state.smoothed_pitch, cfg.deadzone_deg_s);

    // Map deg/s to a [-1.0, 1.0]-ish stick displacement. Not clamped here
    // (caller clamps after combining with the real stick) so fast flicks
    // can contribute proportionally more before the final circular clamp.
    //
    // Sign convention: positive yaw (turning right) -> positive stick X
    // (right). Positive pitch (tilting up) -> negative stick Y (DS4 stick
    // Y convention: up is negative, matching the raw byte range where
    // 0=up, 255=down). If this feels inverted on your hardware, flip the
    // sign here -- gyro axis sign conventions vary slightly by unit
    // orientation and this is a one-line change once you've felt it.
    let dx = yaw / cfg.deg_per_sec_at_full_stick;
    let dy = -pitch / cfg.deg_per_sec_at_full_stick;

    (dx, dy)
}

fn apply_deadzone(value: f64, deadzone: f64) -> f64 {
    if value.abs() < deadzone {
        0.0
    } else {
        value
    }
}

/// Combines the real stick (DS4 native 0-255, 128=center) with a gyro
/// delta (roughly [-1.0, 1.0] range, additive), clamping the RESULT to
/// the stick's circular range -- not clamping each axis independently,
/// which would let diagonal aim exceed the real stick's max speed.
pub fn blend_and_clamp(real_x: u8, real_y: u8, gyro_dx: f64, gyro_dy: f64) -> (u8, u8) {
    const CENTER: f64 = 128.0;
    const RADIUS: f64 = 127.0; // max distance from center in either direction

    let real_dx = (real_x as f64 - CENTER) / RADIUS;
    let real_dy = (real_y as f64 - CENTER) / RADIUS;

    let mut combined_x = real_dx + gyro_dx;
    let mut combined_y = real_dy + gyro_dy;

    let magnitude = (combined_x * combined_x + combined_y * combined_y).sqrt();
    if magnitude > 1.0 {
        combined_x /= magnitude;
        combined_y /= magnitude;
    }

    let out_x = (CENTER + combined_x * RADIUS).round().clamp(0.0, 255.0) as u8;
    let out_y = (CENTER + combined_y * RADIUS).round().clamp(0.0, 255.0) as u8;

    (out_x, out_y)
}
