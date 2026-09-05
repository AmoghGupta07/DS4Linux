//! Touchpad mode selection and mouse-remap math.
//!
//! Three modes: MouseRemap converts finger movement deltas into relative
//! mouse motion on a separate virtual mouse device; AbsoluteMouse maps
//! the touchpad surface directly onto the screen like a graphics
//! tablet/touchscreen -- touch a spot, the cursor jumps there
//! proportionally, rather than dragging from wherever it already was;
//! Passthrough does nothing at all on this project's end.
//!
//! REDESIGNED Passthrough: an earlier version re-emitted real touchpad
//! coordinates onto the virtual DS4's own multitouch axes -- our own
//! from-scratch reimplementation of something the Linux kernel's DS4
//! driver (hid-sony/hid-playstation) already does correctly on its own,
//! as a fully separate, already-registered evdev device (confirmed
//! against hid-sony.c kernel source -- see hide_controller.rs's module
//! doc). Re-emitting was redundant at best (when hide_real_controller
//! is off, which is the default, the kernel's own touchpad device is
//! ALREADY fully visible to everything, independent of anything this
//! project does) and duplicated effort on the single least-verified
//! parsing path in this whole project (see ds4_bt.rs's touchpad offset
//! history) for zero benefit over just leaving the kernel's own device
//! alone. Passthrough now means exactly that: emit nothing touchpad-
//! related on our own virtual device, and -- if hide_real_controller is
//! also on -- specifically exclude the kernel's Touchpad sibling device
//! from being hidden, so it stays usable (see hide_controller.rs's
//! `exclude_suffixes` and ds4l_daemon.rs's use of it). MouseRemap and
//! AbsoluteMouse still hide that sibling device when hide_real_controller
//! is on, deliberately: leaving it visible during those modes would let
//! the desktop's own libinput touchpad-as-cursor recognition fight with
//! this project's own synthetic pointer for control of the same cursor.
//!
//! Selected per config (stand-in for the future per-profile setting,
//! matching the same pattern as gyro_stick's GyroMode).
//!
//! MouseRemap's 2-finger behavior follows DS4Windows's confirmed
//! convention: 1 finger drags the cursor, 2 fingers scroll instead
//! (vertical), and clicking with 2 fingers down is a right-click instead
//! of left-click. This was verified against DS4Windows documentation/
//! guides before implementing, rather than assumed, since building the
//! wrong gesture mapping would mean redoing this later.
//!
//! AbsoluteMouse reuses the SAME 1-finger-left/2-finger-right click
//! convention as MouseRemap for consistency within this project, NOT
//! because it's independently confirmed against DS4Windows's own
//! absolute-mode click behavior specifically (unlike MouseRemap's
//! convention above, which was checked against DS4Windows docs before
//! building) -- worth testing by feel and adjusting if it doesn't match
//! expectations.

use crate::ds4_input::TouchFinger;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TouchpadMode {
    Passthrough,
    MouseRemap,
    AbsoluteMouse,
    /// Fully suppressed: no processing on our end (same as Passthrough
    /// in that regard), but UNLIKE Passthrough, does NOT exclude the
    /// kernel's Touchpad sibling device from hiding -- if
    /// hide_real_controller is also on, the touchpad is hidden along
    /// with everything else, genuinely unusable by anything. If
    /// hide_real_controller is off, this has the same practical effect
    /// as Passthrough (the kernel's device was always independently
    /// visible either way) -- Disabled only diverges from Passthrough
    /// when hiding is actually in play. Worth being upfront about: this
    /// isn't a stronger guarantee than that.
    Disabled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TouchpadConfig {
    pub mode: TouchpadMode,
    /// Mouse pixels of movement per unit of raw touchpad delta. DS4's
    /// touchpad is ~1920x942 units; this scale is a starting point for
    /// "moderate" mouse feel, tune by preference.
    pub mouse_sensitivity: f64,
    /// Scroll "clicks" per unit of raw touchpad Y delta during 2-finger
    /// drag. Separate from mouse_sensitivity since scroll and cursor
    /// speed are typically tuned independently by feel.
    pub scroll_sensitivity: f64,
}

impl Default for TouchpadConfig {
    fn default() -> Self {
        TouchpadConfig {
            mode: TouchpadMode::MouseRemap,
            mouse_sensitivity: 0.5,
            scroll_sensitivity: 0.05,
        }
    }
}

/// What the mouse-remap layer wants to happen this frame. Kept as a single
/// enum rather than several loosely-related Option<T> fields so the
/// caller can match exhaustively and can't accidentally act on stale
/// cursor-move data during a scroll frame, or vice versa.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseAction {
    None,
    Move { dx: i32, dy: i32 },
    Scroll { amount: i32 },
}

/// Tracks per-touch state across reports so MouseRemap mode can compute
/// deltas (the touchpad reports absolute position, not relative motion)
/// and detect finger-count transitions (1->2 fingers mid-drag, etc).
#[derive(Debug, Default)]
pub struct TouchpadMouseState {
    prev_x: i32,
    prev_y: i32,
    /// 0, 1, or 2 -- finger count as of the last processed report. Used
    /// to detect transitions so a switch from 1 to 2 fingers (or back)
    /// resets the delta baseline instead of producing one large spurious
    /// jump from finger1's last position to finger2's current position.
    prev_finger_count: u8,
}

/// Determines what mouse action (if any) this frame's touch state should
/// produce, and updates `state` for the next call. This is the single
/// entry point mouse-remap mode needs -- caller doesn't need to reason
/// about finger-count branching itself.
pub fn compute_mouse_action(
    state: &mut TouchpadMouseState,
    cfg: &TouchpadConfig,
    finger1: &TouchFinger,
    finger2: &TouchFinger,
) -> MouseAction {
    let finger_count: u8 = finger1.touching as u8 + finger2.touching as u8;

    if finger_count == 0 {
        state.prev_finger_count = 0;
        return MouseAction::None;
    }

    // Use finger1's position as the tracked point in both 1-finger and
    // 2-finger modes -- finger1 is present whenever any finger is down
    // (the controller always fills finger1 before finger2), so this stays
    // consistent rather than needing to pick whichever finger is "active."
    let x = finger1.x as i32;
    let y = finger1.y as i32;

    // Finger count changed since last frame (0->1, 1->2, 2->1) -- reset
    // the delta baseline instead of diffing against a position that came
    // from a different touch context, which would otherwise produce one
    // large spurious jump/scroll on every transition.
    if finger_count != state.prev_finger_count {
        state.prev_x = x;
        state.prev_y = y;
        state.prev_finger_count = finger_count;
        return MouseAction::None;
    }

    let raw_dx = x - state.prev_x;
    let raw_dy = y - state.prev_y;
    state.prev_x = x;
    state.prev_y = y;

    if raw_dx == 0 && raw_dy == 0 {
        return MouseAction::None;
    }

    if finger_count == 1 {
        let dx = (raw_dx as f64 * cfg.mouse_sensitivity).round() as i32;
        let dy = (raw_dy as f64 * cfg.mouse_sensitivity).round() as i32;
        if dx == 0 && dy == 0 {
            MouseAction::None
        } else {
            MouseAction::Move { dx, dy }
        }
    } else {
        // 2 fingers: vertical drag -> scroll, matching DS4Windows's
        // "Two Finger Slide" = Scroll convention. Horizontal movement is
        // ignored for now (no horizontal scroll this milestone).
        let amount = (raw_dy as f64 * cfg.scroll_sensitivity).round() as i32;
        if amount == 0 {
            MouseAction::None
        } else {
            MouseAction::Scroll { amount }
        }
    }
}

/// Whether a touchpad click while `finger_count` fingers are down should
/// be a left or right click, per DS4Windows convention (1 finger =
/// left, 2 fingers = right). Returns None for 0 fingers (click with no
/// finger contact shouldn't normally happen, but handled defensively).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickButton {
    Left,
    Right,
}

pub fn click_button_for_finger_count(finger_count: u8) -> Option<ClickButton> {
    match finger_count {
        1 => Some(ClickButton::Left),
        2 => Some(ClickButton::Right),
        _ => None,
    }
}

/// What AbsoluteMouse mode wants to happen this frame -- much simpler
/// than MouseAction since absolute positioning needs no delta tracking
/// or state at all: the touchpad already reports absolute coordinates,
/// and the virtual pointer device (uinput_absmouse.rs) is set up with
/// its own ABS_X/ABS_Y range matching the touchpad's native resolution
/// exactly, so a touched position can be forwarded as-is with no
/// rescaling. `None` when no finger is touching -- the cursor simply
/// stays wherever it last was, same as lifting a stylus off a tablet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AbsoluteMouseAction {
    None,
    Move { x: i32, y: i32 },
}

/// Reads finger1's raw position directly -- no persistent state needed,
/// unlike compute_mouse_action, since there's no delta to track.
pub fn compute_absolute_mouse_action(finger1: &TouchFinger) -> AbsoluteMouseAction {
    if finger1.touching {
        AbsoluteMouseAction::Move {
            x: finger1.x as i32,
            y: finger1.y as i32,
        }
    } else {
        AbsoluteMouseAction::None
    }
}
