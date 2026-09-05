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

// Verified keyboard key codes (see module doc for source): the FULL
// standard keyboard, not just a curated common subset -- every constant
// below was cross-checked against the actual kernel header
// (include/uapi/linux/input-event-codes.h) via multiple independent
// mirrors, not recalled from memory alone, given how easy a single
// wrong offset would be to get subtly wrong and hard to notice (a
// remapped key that's "close" to correct, e.g. off by one row on the
// numpad, is a worse failure mode than an obviously broken one).
pub const KEY_ESC: u16 = 1;
pub const KEY_1: u16 = 2;
pub const KEY_2: u16 = 3;
pub const KEY_3: u16 = 4;
pub const KEY_4: u16 = 5;
pub const KEY_5: u16 = 6;
pub const KEY_6: u16 = 7;
pub const KEY_7: u16 = 8;
pub const KEY_8: u16 = 9;
pub const KEY_9: u16 = 10;
pub const KEY_0: u16 = 11;
pub const KEY_MINUS: u16 = 12;
pub const KEY_EQUAL: u16 = 13;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_Q: u16 = 16;
pub const KEY_W: u16 = 17;
pub const KEY_E: u16 = 18;
pub const KEY_R: u16 = 19;
pub const KEY_T: u16 = 20;
pub const KEY_Y: u16 = 21;
pub const KEY_U: u16 = 22;
pub const KEY_I: u16 = 23;
pub const KEY_O: u16 = 24;
pub const KEY_P: u16 = 25;
pub const KEY_LEFTBRACE: u16 = 26;
pub const KEY_RIGHTBRACE: u16 = 27;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_A: u16 = 30;
pub const KEY_S: u16 = 31;
pub const KEY_D: u16 = 32;
pub const KEY_F: u16 = 33;
pub const KEY_G: u16 = 34;
pub const KEY_H: u16 = 35;
pub const KEY_J: u16 = 36;
pub const KEY_K: u16 = 37;
pub const KEY_L: u16 = 38;
pub const KEY_SEMICOLON: u16 = 39;
pub const KEY_APOSTROPHE: u16 = 40;
pub const KEY_GRAVE: u16 = 41;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_BACKSLASH: u16 = 43;
pub const KEY_Z: u16 = 44;
pub const KEY_X: u16 = 45;
pub const KEY_C: u16 = 46;
pub const KEY_V: u16 = 47;
pub const KEY_B: u16 = 48;
pub const KEY_N: u16 = 49;
pub const KEY_M: u16 = 50;
pub const KEY_COMMA: u16 = 51;
pub const KEY_DOT: u16 = 52;
pub const KEY_SLASH: u16 = 53;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_KPASTERISK: u16 = 55;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_F1: u16 = 59;
pub const KEY_F2: u16 = 60;
pub const KEY_F3: u16 = 61;
pub const KEY_F4: u16 = 62;
pub const KEY_F5: u16 = 63;
pub const KEY_F6: u16 = 64;
pub const KEY_F7: u16 = 65;
pub const KEY_F8: u16 = 66;
pub const KEY_F9: u16 = 67;
pub const KEY_F10: u16 = 68;
pub const KEY_NUMLOCK: u16 = 69;
pub const KEY_SCROLLLOCK: u16 = 70;
pub const KEY_KP7: u16 = 71;
pub const KEY_KP8: u16 = 72;
pub const KEY_KP9: u16 = 73;
pub const KEY_KPMINUS: u16 = 74;
pub const KEY_KP4: u16 = 75;
pub const KEY_KP5: u16 = 76;
pub const KEY_KP6: u16 = 77;
pub const KEY_KPPLUS: u16 = 78;
pub const KEY_KP1: u16 = 79;
pub const KEY_KP2: u16 = 80;
pub const KEY_KP3: u16 = 81;
pub const KEY_KP0: u16 = 82;
pub const KEY_KPDOT: u16 = 83;
pub const KEY_102ND: u16 = 86;
pub const KEY_F11: u16 = 87;
pub const KEY_F12: u16 = 88;
pub const KEY_KPENTER: u16 = 96;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_KPSLASH: u16 = 98;
pub const KEY_SYSRQ: u16 = 99; // Print Screen
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_HOME: u16 = 102;
pub const KEY_UP: u16 = 103;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_END: u16 = 107;
pub const KEY_DOWN: u16 = 108;
pub const KEY_PAGEDOWN: u16 = 109;
pub const KEY_INSERT: u16 = 110;
pub const KEY_DELETE: u16 = 111;
pub const KEY_MUTE: u16 = 113;
pub const KEY_VOLUMEDOWN: u16 = 114;
pub const KEY_VOLUMEUP: u16 = 115;
pub const KEY_PAUSE: u16 = 119;
pub const KEY_LEFTMETA: u16 = 125; // "Windows"/"Super" key
pub const KEY_RIGHTMETA: u16 = 126;
pub const KEY_COMPOSE: u16 = 127; // "Menu"/context-menu key
pub const KEY_F13: u16 = 183;
pub const KEY_F14: u16 = 184;
pub const KEY_F15: u16 = 185;
pub const KEY_F16: u16 = 186;
pub const KEY_F17: u16 = 187;
pub const KEY_F18: u16 = 188;
pub const KEY_F19: u16 = 189;
pub const KEY_F20: u16 = 190;
pub const KEY_F21: u16 = 191;
pub const KEY_F22: u16 = 192;
pub const KEY_F23: u16 = 193;
pub const KEY_F24: u16 = 194;

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
    /// Up to 4 keys held simultaneously -- e.g. Ctrl+Shift+Esc or
    /// Ctrl+Alt+Delete. Unused slots are `None`; a Combo with all four
    /// slots `None` behaves identically to `KbmTarget::None` itself.
    ///
    /// Fixed-size `[Option<u16>; 4]` rather than `Vec<u16>` on purpose:
    /// a Vec would force KbmTarget to drop `#[derive(Copy)]`, which a
    /// large amount of existing code (this module's `compute_frame`/
    /// `apply_stick`, and the GUI's entire KBM tab) was written
    /// assuming holds -- that ripple would touch many call sites this
    /// project can't currently compile-check. Four simultaneous keys
    /// covers every realistic keyboard shortcut without paying that
    /// cost; if someone genuinely needs a 5+ key combo, that's a real
    /// but narrow limitation worth revisiting later, not a design flaw
    /// discovered by accident.
    Combo([Option<u16>; 4]),
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
    /// Returns every PressedKey this target should hold down this
    /// frame -- 0 for None, 1 for everything except Combo, up to 4 for
    /// Combo. Renamed from the original `to_pressed_key` (singular,
    /// `Option<PressedKey>`) now that a target can expand to more than
    /// one held key at once.
    fn to_pressed_keys(self) -> Vec<PressedKey> {
        match self {
            KbmTarget::None => vec![],
            KbmTarget::Key(k) => vec![PressedKey::Key(k)],
            KbmTarget::MouseLeft => vec![PressedKey::MouseLeft],
            KbmTarget::MouseRight => vec![PressedKey::MouseRight],
            KbmTarget::Combo(keys) => keys.into_iter().flatten().map(PressedKey::Key).collect(),
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
pub fn compute_frame(state: &PadState, cfg: &KbmConfig, _kbm_state: &mut KbmState) -> KbmFrame {
    let mut held = std::collections::HashSet::new();
    let mut add = |target: KbmTarget| {
        for pk in target.to_pressed_keys() {
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
    // naturally reads as "both keys." Decomposition logic now lives in
    // PadState::dpad_directions (ds4_input.rs) -- factored out once
    // gamepad_remap.rs needed the exact same table, rather than a
    // second copy of this match drifting from this one over time.
    let (dpad_up, dpad_down, dpad_left, dpad_right) = state.dpad_directions();
    if dpad_up {
        add(cfg.dpad_up);
    }
    if dpad_down {
        add(cfg.dpad_down);
    }
    if dpad_left {
        add(cfg.dpad_left);
    }
    if dpad_right {
        add(cfg.dpad_right);
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
