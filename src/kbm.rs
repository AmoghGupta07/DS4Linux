//! KBM (keyboard + mouse) output mode.
//!
//! Distinct from the virtual DS4 gamepad output: instead of driving a
//! virtual controller, each DS4 input drives a keyboard key or mouse
//! action on the combined virtual keyboard+mouse device (uinput_mouse.rs,
//! which despite its filename now handles both). This is DS4Windows's
//! "Controls" mode, confirmed against DS4Windows documentation before
//! building -- distinct from "Controller" mode (virtual gamepad output),
//! not an invented behavior.
//!
//! Scope for this milestone: whole-profile mode switch (a profile is
//! either Gamepad or Kbm output, matching how the person chose to scope
//! this rather than per-button granularity within a single profile).
//! Every button, both sticks, and both triggers are independently
//! configurable -- nothing is hardcoded to a specific game's WASD
//! convention, since "everything customizable" was the explicit ask.
//!
//! KEY_* codes below are individually verified against the current Linux
//! kernel's include/uapi/linux/input-event-codes.h rather than assumed,
//! the same discipline used for every other protocol constant in this
//! project.

use crate::ds4_input::PadState;
use serde::{Deserialize, Serialize};

// Verified keyboard key codes (see module doc for source).
pub const KEY_ESC: u16 = 1;
pub const KEY_1: u16 = 2;
pub const KEY_TAB: u16 = 15;
pub const KEY_Q: u16 = 16;
pub const KEY_W: u16 = 17;
pub const KEY_E: u16 = 18;
pub const KEY_R: u16 = 19;
pub const KEY_T: u16 = 20;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_A: u16 = 30;
pub const KEY_S: u16 = 31;
pub const KEY_D: u16 = 32;
pub const KEY_F: u16 = 33;
pub const KEY_G: u16 = 34;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_Z: u16 = 44;
pub const KEY_X: u16 = 45;
pub const KEY_C: u16 = 46;
pub const KEY_V: u16 = 47;
pub const KEY_B: u16 = 48;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;

/// What a single DS4 input (a button, or one direction of a stick used
/// digitally) maps to in KBM mode. `None` means "produces no output" --
/// distinct from not being in the mapping at all, so a profile can
/// explicitly silence an input rather than it defaulting unpredictably.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KbmTarget {
    None,
    Key(u16),
    MouseLeft,
    MouseRight,
}

/// How a stick behaves in KBM mode: either drives the mouse cursor
/// (reusing the same delta-based movement math the touchpad's MouseRemap
/// mode uses), or acts as four independent digital directions, each
/// mapped to its own KbmTarget (typically keyboard keys, e.g. WASD).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StickKbmMode {
    Mouse { sensitivity: f64 },
    Digital {
        up: KbmTarget,
        down: KbmTarget,
        left: KbmTarget,
        right: KbmTarget,
        /// Stick displacement (0.0-1.0 from center) beyond which a
        /// direction counts as "pressed." DS4Windows-typical default is
        /// a moderate deadzone-like threshold, not a hair-trigger one,
        /// so small stick drift/noise doesn't cause spurious key spam.
        threshold: f64,
    },
}

impl Default for StickKbmMode {
    fn default() -> Self {
        // Left stick defaults to WASD-style digital directions, matching
        // the most common convention for "movement" in KBM-mapped games;
        // right stick's default is set separately in KbmConfig::default
        // since mouse-look is the more common right-stick convention.
        StickKbmMode::Digital {
            up: KbmTarget::Key(KEY_W),
            down: KbmTarget::Key(KEY_S),
            left: KbmTarget::Key(KEY_A),
            right: KbmTarget::Key(KEY_D),
            threshold: 0.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbmConfig {
    pub cross: KbmTarget,
    pub circle: KbmTarget,
    pub triangle: KbmTarget,
    pub square: KbmTarget,
    pub l1: KbmTarget,
    pub r1: KbmTarget,
    pub l2: KbmTarget,
    pub r2: KbmTarget,
    pub l3: KbmTarget,
    pub r3: KbmTarget,
    pub share: KbmTarget,
    pub options: KbmTarget,
    pub ps: KbmTarget,
    pub touchpad_click: KbmTarget,
    pub dpad_up: KbmTarget,
    pub dpad_down: KbmTarget,
    pub dpad_left: KbmTarget,
    pub dpad_right: KbmTarget,
    pub left_stick: StickKbmMode,
    pub right_stick: StickKbmMode,
    /// Analog trigger displacement (0.0-1.0) beyond which L2/R2 count as
    /// "pressed" when mapped to a KbmTarget. Separate from stick
    /// threshold since trigger travel and stick displacement aren't
    /// necessarily tuned the same way.
    pub trigger_threshold: f64,
}

impl Default for KbmConfig {
    fn default() -> Self {
        KbmConfig {
            cross: KbmTarget::Key(KEY_SPACE),
            circle: KbmTarget::Key(KEY_LEFTCTRL),
            triangle: KbmTarget::Key(KEY_E),
            square: KbmTarget::Key(KEY_R),
            l1: KbmTarget::Key(KEY_Q),
            r1: KbmTarget::MouseRight,
            l2: KbmTarget::None, // reserved for aim-down-sights conventions; off by default
            r2: KbmTarget::MouseLeft,
            l3: KbmTarget::Key(KEY_LEFTSHIFT),
            r3: KbmTarget::None,
            share: KbmTarget::Key(KEY_TAB),
            options: KbmTarget::Key(KEY_ESC),
            ps: KbmTarget::None,
            touchpad_click: KbmTarget::None,
            dpad_up: KbmTarget::Key(KEY_1),
            dpad_down: KbmTarget::None,
            dpad_left: KbmTarget::None,
            dpad_right: KbmTarget::None,
            left_stick: StickKbmMode::default(),
            right_stick: StickKbmMode::Mouse { sensitivity: 8.0 },
            trigger_threshold: 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PressedKey {
    Key(u16),
    MouseLeft,
    MouseRight,
}

impl KbmTarget {
    fn to_pressed_key(self) -> Option<PressedKey> {
        match self {
            KbmTarget::None => None,
            KbmTarget::Key(k) => Some(PressedKey::Key(k)),
            KbmTarget::MouseLeft => Some(PressedKey::MouseLeft),
            KbmTarget::MouseRight => Some(PressedKey::MouseRight),
        }
    }
}

/// Tracks which KbmTargets are currently "pressed" so the caller can emit
/// only the deltas (press/release edges) rather than resending every
/// key's state every report -- matches how the rest of the daemon already
/// treats emit_key as idempotent-per-change, and avoids flooding uinput
/// with redundant repeated key-down events every ~4ms.
#[derive(Debug, Default)]
pub struct KbmState {
    pub prev_pressed: std::collections::HashSet<PressedKey>,
}

/// One frame's worth of KBM output: which keys/mouse-buttons should now
/// be held down (the complete set, not a delta -- the caller computes
/// press/release edges by diffing against `KbmState::prev_pressed`).
pub struct KbmFrame {
    pub held: std::collections::HashSet<PressedKey>,
    pub mouse_dx: i32,
    pub mouse_dy: i32,
}

/// Computes this frame's full "held" set from the current pad state and
/// config. Digital stick directions and triggers are threshold-gated;
/// buttons map directly from PadState's existing bools.
pub fn compute_frame(state: &PadState, cfg: &KbmConfig, kbm_state: &mut KbmState) -> KbmFrame {
    let mut held = std::collections::HashSet::new();
    let mut add = |target: KbmTarget| {
        if let Some(pk) = target.to_pressed_key() {
            held.insert(pk);
        }
    };

    if state.cross {
        add(cfg.cross);
    }
    if state.circle {
        add(cfg.circle);
    }
    if state.triangle {
        add(cfg.triangle);
    }
    if state.square {
        add(cfg.square);
    }
    if state.l1 {
        add(cfg.l1);
    }
    if state.r1 {
        add(cfg.r1);
    }
    if state.l3 {
        add(cfg.l3);
    }
    if state.r3 {
        add(cfg.r3);
    }
    if state.share {
        add(cfg.share);
    }
    if state.options {
        add(cfg.options);
    }
    if state.ps {
        add(cfg.ps);
    }
    if state.touchpad_click {
        add(cfg.touchpad_click);
    }

    let trigger_thresh_u8 = (cfg.trigger_threshold * 255.0) as u8;
    if state.l2_analog >= trigger_thresh_u8 {
        add(cfg.l2);
    }
    if state.r2_analog >= trigger_thresh_u8 {
        add(cfg.r2);
    }

    // Dpad: byte value 0=N,1=NE,2=E,3=SE,4=S,5=SW,6=W,7=NW,8=released
    // (confirmed back in Milestone 3). Diagonals map to both adjacent
    // digital directions, matching how a physical dpad diagonal press
    // naturally reads as "both keys."
    match state.dpad {
        0 => add(cfg.dpad_up),
        1 => {
            add(cfg.dpad_up);
            add(cfg.dpad_right);
        }
        2 => add(cfg.dpad_right),
        3 => {
            add(cfg.dpad_down);
            add(cfg.dpad_right);
        }
        4 => add(cfg.dpad_down),
        5 => {
            add(cfg.dpad_down);
            add(cfg.dpad_left);
        }
        6 => add(cfg.dpad_left),
        7 => {
            add(cfg.dpad_up);
            add(cfg.dpad_left);
        }
        _ => {}
    }

    let mut mouse_dx = 0i32;
    let mut mouse_dy = 0i32;

    apply_stick(&cfg.left_stick, state.lx, state.ly, &mut add, &mut mouse_dx, &mut mouse_dy);
    apply_stick(&cfg.right_stick, state.rx, state.ry, &mut add, &mut mouse_dx, &mut mouse_dy);

    KbmFrame {
        held,
        mouse_dx,
        mouse_dy,
    }
}

/// Handles one stick's contribution: either digital direction keys, or
/// mouse movement deltas, depending on its configured mode.
fn apply_stick(
    mode: &StickKbmMode,
    x: u8,
    y: u8,
    add: &mut impl FnMut(KbmTarget),
    mouse_dx: &mut i32,
    mouse_dy: &mut i32,
) {
    match mode {
        StickKbmMode::Digital {
            up,
            down,
            left,
            right,
            threshold,
        } => {
            // Normalize to [-1.0, 1.0] the same way blend_and_clamp does,
            // so `threshold` means the same thing here as it would for
            // gyro/touchpad deadzones elsewhere in the project.
            const CENTER: f64 = 128.0;
            const RADIUS: f64 = 127.0;
            let nx = (x as f64 - CENTER) / RADIUS;
            let ny = (y as f64 - CENTER) / RADIUS;

            if ny < -threshold {
                add(*up);
            }
            if ny > *threshold {
                add(*down);
            }
            if nx < -threshold {
                add(*left);
            }
            if nx > *threshold {
                add(*right);
            }
        }
        StickKbmMode::Mouse { sensitivity } => {
            // Unlike the touchpad (which reports absolute finger
            // position -- diffing consecutive samples makes sense
            // there), a gamepad stick reports displacement FROM CENTER
            // and stays held off-center while being pushed. Treating
            // stick position as a delta-source (diff vs previous frame)
            // would mean holding the stick steady off-center produces
            // zero mouse movement after the first frame -- not how a
            // joystick-mouse is supposed to feel. Instead, stick
            // offset-from-center is used directly as a continuous
            // per-frame velocity: held further from center = faster
            // continuous cursor movement, matching real joystick-mouse
            // drivers (e.g. X11's jstick/joymouse behavior).
            const CENTER: f64 = 128.0;
            const RADIUS: f64 = 127.0;
            let nx = (x as f64 - CENTER) / RADIUS; // -1.0..1.0
            let ny = (y as f64 - CENTER) / RADIUS;

            // Small deadzone so a stick at rest (tiny noise around
            // center) doesn't cause slow cursor creep.
            const DEADZONE: f64 = 0.08;
            let nx = if nx.abs() < DEADZONE { 0.0 } else { nx };
            let ny = if ny.abs() < DEADZONE { 0.0 } else { ny };

            *mouse_dx += (nx * sensitivity).round() as i32;
            *mouse_dy += (ny * sensitivity).round() as i32;
        }
    }
}
