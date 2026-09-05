//! Full button/stick/trigger remapping for Gamepad and Xbox360 output
//! modes -- DS4Windows's "Controller" output customization (as opposed
//! to kbm.rs's "Controls"/keyboard-mouse customization, which already
//! had this kind of flexibility). Before this module, Gamepad/Xbox360
//! output was hardcoded 1:1 (Cross always drives Cross/A, sticks always
//! pass straight through) -- this module sits between a parsed PadState
//! and emit_gamepad_state/emit_x360_state, transforming one into a
//! GamepadFrame that either emit function can then just read straight
//! off, with zero awareness of remapping itself.
//!
//! DESIGN MODEL (the choices below are deliberate, not the only
//! possible design -- worth understanding before extending this):
//!
//! - Every DIGITAL input (17 buttons/dpad directions) maps to a
//!   GamepadTarget: no output at all, a button press, a full-deflection
//!   push on either output stick in one direction, or a full-press on
//!   either output trigger. Multiple physical inputs can target the
//!   same output (they OR-combine for buttons/triggers, and SUM as
//!   circularly-clamped deltas for stick pushes -- see
//!   `gyro_stick::blend_and_clamp`, reused here unchanged since "combine
//!   a base stick position with one or more directional pushes without
//!   exceeding the stick's real range" is exactly what that function
//!   already does for blending gyro onto a stick).
//!
//! - Each STICK has an independent analog passthrough target
//!   (StickAnalogSource: which output stick its raw analog value feeds,
//!   or none) AND an independent digital 4-direction breakdown
//!   (StickDigitalConfig, same shape as kbm.rs's StickKbmMode::Digital)
//!   whose directions are ALSO GamepadTargets. Both can be active
//!   simultaneously -- e.g. the left stick could analog-drive the
//!   output left stick AND have "left stick up" also trigger a button
//!   press. This is more flexibility than most people will ever use,
//!   but it composes for free from the two independent fields rather
//!   than needing a mutually-exclusive mode choice, and defaults to
//!   "digital breakdown entirely disabled" so nobody accidentally
//!   triggers it.
//!
//! - Each TRIGGER (L2/R2) has an independent analog passthrough target
//!   (TriggerAnalogSource) for its 0-255 analog value. Its DIGITAL click
//!   (the hardware's own l2_digital/r2_digital bit -- a real, distinct
//!   signal from "analog value past some threshold", not synthesized
//!   here) is just one of the 17 ordinary digital inputs, mapped like
//!   any button.
//!
//! - Gyro-to-stick blending (gyro_stick.rs) still always targets the
//!   FINAL output right stick, applied AFTER this module's remapping --
//!   so "gyro augments the right stick" keeps meaning exactly that
//!   regardless of what's been remapped onto that slot. This is a
//!   design choice, not a technical necessity: if a person swaps left/
//!   right stick sources, gyro follows the OUTPUT slot, not whichever
//!   physical stick happens to feed it. Documented here so it's an
//!   intentional decision on record, not a surprise discovered later.
//!
//! DEFAULT CONFIGURATION reconstructs today's pre-remap behavior
//! EXACTLY (every button targets itself, each stick's analog source is
//! itself, no digital stick breakdown, each trigger's analog source is
//! itself) -- so an existing profile with no `[gamepad_remap]` section
//! at all (via `#[serde(default)]` on Profile's new field) behaves
//! completely unchanged after upgrading. Nothing about this feature
//! forces anyone to use it.

use crate::ds4_input::PadState;
use crate::gyro_stick::blend_and_clamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GamepadButton {
    Cross,
    Circle,
    Square,
    Triangle,
    L1,
    R1,
    L2Digital,
    R2Digital,
    L3,
    R3,
    Share,
    Options,
    Ps,
    TouchpadClick,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AxisDirection {
    Up,
    Down,
    Left,
    Right,
}

impl AxisDirection {
    /// Normalized push delta for this direction, sign convention
    /// matching gyro_stick.rs's existing documented convention (DS4
    /// stick Y: up is negative, matching the raw byte range where
    /// 0=up, 255=down) so this composes correctly with blend_and_clamp.
    fn delta(self) -> (f64, f64) {
        match self {
            AxisDirection::Up => (0.0, -1.0),
            AxisDirection::Down => (0.0, 1.0),
            AxisDirection::Left => (-1.0, 0.0),
            AxisDirection::Right => (1.0, 0.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStick {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputTrigger {
    L2,
    R2,
}

/// What any DIGITAL-capable input (a button, a dpad direction, or one
/// direction of a stick's digital breakdown) can be remapped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GamepadTarget {
    None,
    Button(GamepadButton),
    /// Full deflection of the named output stick in this direction
    /// while the source input is active -- combined with that stick's
    /// analog base value (and any other simultaneous pushes onto it)
    /// via blend_and_clamp, same circular-clamping math gyro blending
    /// already uses.
    StickPush(OutputStick, AxisDirection),
    /// Full analog press (255) of the named output trigger while the
    /// source input is active -- combined with that trigger's analog
    /// base value via a simple max(), so a digital push never makes an
    /// already-pressed trigger read as LESS pressed.
    TriggerPush(OutputTrigger),
}

/// Which output stick (if any) a physical stick's raw ANALOG value
/// passes through to. Independent of that same physical stick's
/// digital breakdown (see module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StickAnalogSource {
    None,
    Left,
    Right,
}

/// Which output trigger (if any) a physical trigger's raw ANALOG value
/// (0-255) passes through to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TriggerAnalogSource {
    None,
    L2,
    R2,
}

/// A physical stick's optional 4-direction digital breakdown, same
/// shape as kbm.rs's StickKbmMode::Digital -- each direction is its own
/// GamepadTarget rather than a KbmTarget, since this module's targets
/// are gamepad outputs, not keyboard/mouse ones.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StickDigitalConfig {
    pub up: GamepadTarget,
    pub down: GamepadTarget,
    pub left: GamepadTarget,
    pub right: GamepadTarget,
    /// Stick displacement (0.0-1.0 from center) beyond which a
    /// direction counts as active. Same default/reasoning as kbm.rs's
    /// equivalent field.
    pub threshold: f64,
}

impl Default for StickDigitalConfig {
    fn default() -> Self {
        // All-None targets: inactive by default, exactly matching
        // "digital breakdown disabled" -- a fresh profile does nothing
        // extra here until someone deliberately configures it.
        StickDigitalConfig {
            up: GamepadTarget::None,
            down: GamepadTarget::None,
            left: GamepadTarget::None,
            right: GamepadTarget::None,
            threshold: 0.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GamepadRemapConfig {
    pub cross: GamepadTarget,
    pub circle: GamepadTarget,
    pub triangle: GamepadTarget,
    pub square: GamepadTarget,
    pub l1: GamepadTarget,
    pub r1: GamepadTarget,
    pub l2_digital: GamepadTarget,
    pub r2_digital: GamepadTarget,
    pub l3: GamepadTarget,
    pub r3: GamepadTarget,
    pub share: GamepadTarget,
    pub options: GamepadTarget,
    pub ps: GamepadTarget,
    pub touchpad_click: GamepadTarget,
    pub dpad_up: GamepadTarget,
    pub dpad_down: GamepadTarget,
    pub dpad_left: GamepadTarget,
    pub dpad_right: GamepadTarget,

    pub left_stick_analog: StickAnalogSource,
    #[serde(default)]
    pub left_stick_digital: StickDigitalConfig,
    pub right_stick_analog: StickAnalogSource,
    #[serde(default)]
    pub right_stick_digital: StickDigitalConfig,

    pub l2_analog_target: TriggerAnalogSource,
    pub r2_analog_target: TriggerAnalogSource,
}

impl Default for GamepadRemapConfig {
    /// Reconstructs today's pre-remap behavior EXACTLY -- see module
    /// doc's "DEFAULT CONFIGURATION" section. Every field here maps an
    /// input to itself.
    fn default() -> Self {
        use GamepadButton::*;
        GamepadRemapConfig {
            cross: GamepadTarget::Button(Cross),
            circle: GamepadTarget::Button(Circle),
            triangle: GamepadTarget::Button(Triangle),
            square: GamepadTarget::Button(Square),
            l1: GamepadTarget::Button(L1),
            r1: GamepadTarget::Button(R1),
            l2_digital: GamepadTarget::Button(L2Digital),
            r2_digital: GamepadTarget::Button(R2Digital),
            l3: GamepadTarget::Button(L3),
            r3: GamepadTarget::Button(R3),
            share: GamepadTarget::Button(Share),
            options: GamepadTarget::Button(Options),
            ps: GamepadTarget::Button(Ps),
            touchpad_click: GamepadTarget::Button(TouchpadClick),
            dpad_up: GamepadTarget::Button(DpadUp),
            dpad_down: GamepadTarget::Button(DpadDown),
            dpad_left: GamepadTarget::Button(DpadLeft),
            dpad_right: GamepadTarget::Button(DpadRight),
            left_stick_analog: StickAnalogSource::Left,
            left_stick_digital: StickDigitalConfig::default(),
            right_stick_analog: StickAnalogSource::Right,
            right_stick_digital: StickDigitalConfig::default(),
            l2_analog_target: TriggerAnalogSource::L2,
            r2_analog_target: TriggerAnalogSource::R2,
        }
    }
}

/// The fully remapped output for one report -- emit_gamepad_state and
/// emit_x360_state read straight off this, with no remapping knowledge
/// of their own. Sticks/triggers here are PRE-gyro: the caller still
/// applies gyro_stick blending onto right_x/right_y afterward, same as
/// before this module existed (see module doc's gyro section).
#[derive(Debug, Clone, Copy, Default)]
pub struct GamepadFrame {
    pub cross: bool,
    pub circle: bool,
    pub square: bool,
    pub triangle: bool,
    pub l1: bool,
    pub r1: bool,
    pub l2_digital: bool,
    pub r2_digital: bool,
    pub l3: bool,
    pub r3: bool,
    pub share: bool,
    pub options: bool,
    pub ps: bool,
    pub touchpad_click: bool,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub dpad_left: bool,
    pub dpad_right: bool,
    pub left_x: u8,
    pub left_y: u8,
    pub right_x: u8,
    pub right_y: u8,
    pub l2_analog: u8,
    pub r2_analog: u8,
}

impl GamepadFrame {
    fn set_button(&mut self, button: GamepadButton) {
        let field = match button {
            GamepadButton::Cross => &mut self.cross,
            GamepadButton::Circle => &mut self.circle,
            GamepadButton::Square => &mut self.square,
            GamepadButton::Triangle => &mut self.triangle,
            GamepadButton::L1 => &mut self.l1,
            GamepadButton::R1 => &mut self.r1,
            GamepadButton::L2Digital => &mut self.l2_digital,
            GamepadButton::R2Digital => &mut self.r2_digital,
            GamepadButton::L3 => &mut self.l3,
            GamepadButton::R3 => &mut self.r3,
            GamepadButton::Share => &mut self.share,
            GamepadButton::Options => &mut self.options,
            GamepadButton::Ps => &mut self.ps,
            GamepadButton::TouchpadClick => &mut self.touchpad_click,
            GamepadButton::DpadUp => &mut self.dpad_up,
            GamepadButton::DpadDown => &mut self.dpad_down,
            GamepadButton::DpadLeft => &mut self.dpad_left,
            GamepadButton::DpadRight => &mut self.dpad_right,
        };
        // OR-combine: multiple physical inputs can target the same
        // output button, and it should read pressed if ANY of them are.
        *field = true;
    }

    fn push_trigger(&mut self, trigger: OutputTrigger) {
        match trigger {
            OutputTrigger::L2 => self.l2_analog = self.l2_analog.max(255),
            OutputTrigger::R2 => self.r2_analog = self.r2_analog.max(255),
        }
    }
}

/// Applies one digital input's configured target to `frame`/the
/// relevant stick-push accumulators, IF the input is currently active.
/// Shared by every digital input site below (17 buttons/dpad directions
/// plus up to 8 stick-digital directions) so the "what does a
/// GamepadTarget actually do" logic exists in exactly one place.
#[allow(clippy::too_many_arguments)]
fn apply_target(
    active: bool,
    target: GamepadTarget,
    frame: &mut GamepadFrame,
    left_push: &mut (f64, f64),
    right_push: &mut (f64, f64),
) {
    if !active {
        return;
    }
    match target {
        GamepadTarget::None => {}
        GamepadTarget::Button(button) => frame.set_button(button),
        GamepadTarget::StickPush(stick, direction) => {
            let (dx, dy) = direction.delta();
            let accumulator = match stick {
                OutputStick::Left => &mut *left_push,
                OutputStick::Right => &mut *right_push,
            };
            accumulator.0 += dx;
            accumulator.1 += dy;
        }
        GamepadTarget::TriggerPush(trigger) => frame.push_trigger(trigger),
    }
}

/// Computes this report's fully remapped GamepadFrame. Pure function --
/// no persistent state needed, unlike kbm.rs's KbmState (which tracks
/// held keys for press/release diffing) or gyro_stick's
/// GyroStickState (which tracks smoothing/toggle latches): every field
/// here is fully determined by the current PadState and config alone.
pub fn compute_gamepad_frame(state: &PadState, cfg: &GamepadRemapConfig) -> GamepadFrame {
    let mut frame = GamepadFrame::default();

    // Stick pushes accumulate as (dx, dy) sums here, combined with each
    // stick's analog base value via blend_and_clamp at the very end --
    // NOT applied one push at a time, so multiple simultaneous pushes
    // onto the same stick combine correctly as one clamped vector
    // rather than each clamping independently and potentially
    // under-representing a diagonal combination.
    let mut left_push = (0.0_f64, 0.0_f64);
    let mut right_push = (0.0_f64, 0.0_f64);

    // -- The 17 ordinary digital inputs --
    apply_target(state.cross, cfg.cross, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.circle, cfg.circle, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.triangle, cfg.triangle, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.square, cfg.square, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.l1, cfg.l1, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.r1, cfg.r1, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.l2_digital, cfg.l2_digital, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.r2_digital, cfg.r2_digital, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.l3, cfg.l3, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.r3, cfg.r3, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.share, cfg.share, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.options, cfg.options, &mut frame, &mut left_push, &mut right_push);
    apply_target(state.ps, cfg.ps, &mut frame, &mut left_push, &mut right_push);
    apply_target(
        state.touchpad_click,
        cfg.touchpad_click,
        &mut frame,
        &mut left_push,
        &mut right_push,
    );

    let (dpad_up, dpad_down, dpad_left, dpad_right) = state.dpad_directions();
    apply_target(dpad_up, cfg.dpad_up, &mut frame, &mut left_push, &mut right_push);
    apply_target(dpad_down, cfg.dpad_down, &mut frame, &mut left_push, &mut right_push);
    apply_target(dpad_left, cfg.dpad_left, &mut frame, &mut left_push, &mut right_push);
    apply_target(dpad_right, cfg.dpad_right, &mut frame, &mut left_push, &mut right_push);

    // -- Each stick's digital breakdown (independent of its analog
    // passthrough below -- both can be active, see module doc) --
    let (lnx, lny) = normalized_stick(state.lx, state.ly);
    apply_stick_digital(&cfg.left_stick_digital, lnx, lny, &mut frame, &mut left_push, &mut right_push);
    let (rnx, rny) = normalized_stick(state.rx, state.ry);
    apply_stick_digital(&cfg.right_stick_digital, rnx, rny, &mut frame, &mut left_push, &mut right_push);

    // -- Analog stick passthrough: base position for each OUTPUT stick,
    // before combining with any pushes accumulated above --
    let left_base = analog_source_value(cfg.left_stick_analog, state.lx, state.ly, state.rx, state.ry);
    let right_base = analog_source_value(cfg.right_stick_analog, state.lx, state.ly, state.rx, state.ry);

    (frame.left_x, frame.left_y) = blend_and_clamp(left_base.0, left_base.1, left_push.0, left_push.1);
    (frame.right_x, frame.right_y) =
        blend_and_clamp(right_base.0, right_base.1, right_push.0, right_push.1);

    // -- Analog trigger passthrough --
    frame.l2_analog = frame
        .l2_analog
        .max(trigger_source_value(cfg.l2_analog_target, state.l2_analog, state.r2_analog));
    frame.r2_analog = frame
        .r2_analog
        .max(trigger_source_value(cfg.r2_analog_target, state.l2_analog, state.r2_analog));

    frame
}

fn normalized_stick(x: u8, y: u8) -> (f64, f64) {
    const CENTER: f64 = 128.0;
    const RADIUS: f64 = 127.0;
    ((x as f64 - CENTER) / RADIUS, (y as f64 - CENTER) / RADIUS)
}

fn apply_stick_digital(
    cfg: &StickDigitalConfig,
    nx: f64,
    ny: f64,
    frame: &mut GamepadFrame,
    left_push: &mut (f64, f64),
    right_push: &mut (f64, f64),
) {
    apply_target(ny < -cfg.threshold, cfg.up, frame, left_push, right_push);
    apply_target(ny > cfg.threshold, cfg.down, frame, left_push, right_push);
    apply_target(nx < -cfg.threshold, cfg.left, frame, left_push, right_push);
    apply_target(nx > cfg.threshold, cfg.right, frame, left_push, right_push);
}

/// Resolves which physical stick (if any) feeds a given OUTPUT stick's
/// analog base value, returning DS4-native (u8, u8) coordinates
/// (128=center) -- takes both physical sticks' raw values directly
/// rather than a single (x, y) pair, since a StickAnalogSource can name
/// EITHER physical stick regardless of which output slot is asking.
fn analog_source_value(source: StickAnalogSource, lx: u8, ly: u8, rx: u8, ry: u8) -> (u8, u8) {
    match source {
        StickAnalogSource::None => (128, 128),
        StickAnalogSource::Left => (lx, ly),
        StickAnalogSource::Right => (rx, ry),
    }
}

/// Resolves an output trigger's analog base value (0-255) from whichever
/// physical trigger (if any) its TriggerAnalogSource names.
fn trigger_source_value(source: TriggerAnalogSource, l2_analog: u8, r2_analog: u8) -> u8 {
    match source {
        TriggerAnalogSource::None => 0,
        TriggerAnalogSource::L2 => l2_analog,
        TriggerAnalogSource::R2 => r2_analog,
    }
}
