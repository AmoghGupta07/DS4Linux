//! Touchpad mode selection and mouse-remap math.
//!
//! Two modes, matching what was scoped: Passthrough sends real touchpad
//! coordinates straight to the virtual DS4's multitouch axes (for games
//! that read touchpad-as-touchpad); MouseRemap converts finger movement
//! deltas into relative mouse motion on a separate virtual mouse device.
//! Selected per config for now (stand-in for the future per-profile
//! setting), matching the same pattern as gyro_stick's GyroMode.

use crate::ds4_input::TouchFinger;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchpadMode {
    Passthrough,
    MouseRemap,
}

#[derive(Debug, Clone, Copy)]
pub struct TouchpadConfig {
    pub mode: TouchpadMode,
    /// Mouse pixels of movement per unit of raw touchpad delta. DS4's
    /// touchpad is ~1920x942 units; this scale is a starting point for
    /// "moderate" mouse feel, tune by preference.
    pub mouse_sensitivity: f64,
}

impl Default for TouchpadConfig {
    fn default() -> Self {
        TouchpadConfig {
            mode: TouchpadMode::MouseRemap,
            mouse_sensitivity: 1.5,
        }
    }
}

/// Tracks the previous finger position so MouseRemap mode can compute a
/// delta between reports, since the touchpad reports absolute position,
/// not relative motion -- relative deltas are what EV_REL mouse movement
/// needs. Also tracks whether the finger was down last frame, so a
/// fresh touch-down doesn't produce a spurious large jump from wherever
/// the finger last was before lifting.
#[derive(Debug, Default)]
pub struct TouchpadMouseState {
    prev_x: i32,
    prev_y: i32,
    was_touching: bool,
}

/// Computes the (dx, dy) mouse delta for this frame given the current
/// finger state, or None if there's no motion to report (finger not
/// touching, or this is the first frame of a new touch with nothing to
/// diff against yet).
pub fn compute_mouse_delta(
    state: &mut TouchpadMouseState,
    cfg: &TouchpadConfig,
    finger: &TouchFinger,
) -> Option<(i32, i32)> {
    if !finger.touching {
        state.was_touching = false;
        return None;
    }

    let x = finger.x as i32;
    let y = finger.y as i32;

    if !state.was_touching {
        // Finger just touched down -- record position but don't emit a
        // delta yet, since there's no previous position on this touch to
        // diff against (would otherwise jump from last touch's endpoint).
        state.prev_x = x;
        state.prev_y = y;
        state.was_touching = true;
        return None;
    }

    let raw_dx = x - state.prev_x;
    let raw_dy = y - state.prev_y;
    state.prev_x = x;
    state.prev_y = y;

    if raw_dx == 0 && raw_dy == 0 {
        return None;
    }

    let dx = (raw_dx as f64 * cfg.mouse_sensitivity).round() as i32;
    let dy = (raw_dy as f64 * cfg.mouse_sensitivity).round() as i32;

    Some((dx, dy))
}
