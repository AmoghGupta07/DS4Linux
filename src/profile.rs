//! Profile loading/saving.
//!
//! A profile is just the existing `GyroStickConfig` and `TouchpadConfig`
//! structs (already `Serialize`/`Deserialize`) plus a name, written to
//! `~/.config/ds4l/profiles/<name>.toml`. This was the intended payoff of
//! designing those structs profile-shaped from the start (see their doc
//! comments back in Milestone 3.5/4) -- no restructuring needed, just
//! derives and a thin load/save layer.
//!
//! Scope for this milestone: load-on-startup and save-to-disk. Runtime
//! profile *switching* (hotkey, tray icon) is a GUI-layer concern for
//! later; this module is structured so that's an additive change (call
//! `load` again with a different name), not a redesign.

use crate::gamepad_remap::GamepadRemapConfig;
use crate::gyro_stick::GyroStickConfig;
use crate::kbm::KbmConfig;
use crate::touchpad::TouchpadConfig;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

/// Whole-profile output mode: drives the virtual DS4 gamepad (existing
/// behavior since Milestone 3), drives keyboard/mouse output instead
/// (kbm.rs) -- DS4Windows's "Controller" vs "Controls" distinction,
/// confirmed against DS4Windows documentation before building this --
/// or drives a virtual Xbox 360 pad (uinput_x360.rs) for software that
/// specifically identifies controllers by VID/PID rather than using
/// generic SDL button prompts (Proton/Wine XInput-only games being the
/// main case). Scoped per-profile rather than per-button, matching how
/// this was deliberately scoped: create a separate profile per output
/// type rather than mixing them within one profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputMode {
    Gamepad,
    Kbm,
    Xbox360,
}

impl Default for OutputMode {
    fn default() -> Self {
        OutputMode::Gamepad
    }
}

/// RGB lightbar color, 0-255 per channel -- matches DS4's native color
/// range directly (no scaling needed when writing to the output report).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LightbarColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Default for LightbarColor {
    fn default() -> Self {
        // Blue: DS4's own factory-default color when no profile has set
        // one, so a fresh install "looks like a normal DS4" rather than
        // an unexpected color.
        LightbarColor { red: 0, green: 0, blue: 255 }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Ds4FeedbackConfig {
    pub lightbar: LightbarColor,
    /// Whether to send a brief rumble pulse when this profile is loaded,
    /// confirming to the person (without needing to look at the screen)
    /// that the daemon connected and picked up the right profile.
    #[serde(default = "default_true")]
    pub rumble_on_load: bool,
    /// Whether the daemon should flash the lightbar (alternating red /
    /// off, overriding the configured color) once battery drops to or
    /// below LOW_BATTERY_THRESHOLD_PERCENT (see ds4l_daemon.rs) while
    /// not charging. Off by default -- same opt-in reasoning as
    /// hide_real_controller: a behavior that visibly overrides the
    /// person's chosen lightbar color should be something they turn on
    /// deliberately, not a surprise the first time their battery gets
    /// low. `#[serde(default)]` (false) so profiles saved before this
    /// field existed keep their exact prior behavior on upgrade.
    #[serde(default)]
    pub low_battery_flash: bool,
    /// Whether the lightbar continuously cycles through the color wheel
    /// instead of showing the static `lightbar` color -- DS4Windows's
    /// "Rainbow" option. Off by default, same opt-in reasoning as
    /// low_battery_flash (visibly overrides the configured color, so it
    /// should be a deliberate choice). Takes lower priority than
    /// low_battery_flash: a low-battery warning is safety-relevant
    /// information and shouldn't be visually competed with by a color
    /// cycle -- see update_lightbar_effects in ds4l_daemon.rs for the
    /// precedence logic.
    #[serde(default)]
    pub rainbow: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Ds4FeedbackConfig {
    fn default() -> Self {
        Ds4FeedbackConfig {
            lightbar: LightbarColor::default(),
            rumble_on_load: true,
            low_battery_flash: false,
            rainbow: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Display name, also used to derive the filename. Not read from the
    /// filename itself so renaming the field doesn't require a file
    /// rename, and so profiles could later be listed/renamed via GUI
    /// without touching the filesystem layer's assumptions.
    pub name: String,
    pub gyro: GyroStickConfig,
    pub touchpad: TouchpadConfig,
    /// `#[serde(default)]` so profile files saved before this milestone
    /// (no `[feedback]` section) still parse correctly instead of
    /// failing to load -- missing field falls back to
    /// Ds4FeedbackConfig::default() rather than erroring.
    #[serde(default)]
    pub feedback: Ds4FeedbackConfig,
    /// Same backward-compatibility reasoning: profiles saved before this
    /// milestone default to Gamepad mode (unchanged existing behavior)
    /// rather than failing to load.
    #[serde(default)]
    pub output_mode: OutputMode,
    #[serde(default)]
    pub kbm: KbmConfig,
    /// Full button/stick/trigger remapping for Gamepad/Xbox360 output
    /// modes (see gamepad_remap.rs). `#[serde(default)]` reconstructs
    /// today's pre-remap 1:1 behavior exactly, so profiles saved before
    /// this field existed are completely unaffected on upgrade -- see
    /// GamepadRemapConfig::default()'s own doc comment.
    #[serde(default)]
    pub gamepad_remap: GamepadRemapConfig,
    /// Whether the real controller's device nodes should be hidden from
    /// other processes while the daemon runs (see hide_controller.rs).
    /// Defaults to false: hiding is opt-in, not automatic, since it's a
    /// permission-changing action on system device nodes and a person
    /// should choose it deliberately per profile rather than have it
    /// silently enabled by default.
    #[serde(default)]
    pub hide_real_controller: bool,
    /// Whether hide_real_controller (if also enabled) should EXCLUDE
    /// the real controller's kernel-registered Motion Sensors sibling
    /// device from being hidden, so other software reading gyro/accel
    /// via that already-correct kernel-exposed evdev device (not our
    /// own raw-HID reading) keeps working -- see hide_controller.rs's
    /// module doc for exactly which kernel devices this refers to.
    /// Independent of `gyro.mode` (gyro-to-stick blending): both can be
    /// on at once, since one is "let other software read raw gyro" and
    /// the other is "blend gyro onto our own output stick" -- unrelated
    /// concerns that happen to share the same physical sensor data.
    /// Has no effect at all if hide_real_controller is false, since
    /// nothing is hidden in the first place to exclude anything from.
    #[serde(default)]
    pub gyro_passthrough: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            name: "Default".to_string(),
            gyro: GyroStickConfig::default(),
            touchpad: TouchpadConfig::default(),
            feedback: Ds4FeedbackConfig::default(),
            output_mode: OutputMode::default(),
            kbm: KbmConfig::default(),
            gamepad_remap: GamepadRemapConfig::default(),
            hide_real_controller: false,
            gyro_passthrough: false,
        }
    }
}

#[derive(Debug)]
pub enum ProfileError {
    Io(io::Error),
    Toml(toml::de::Error),
    TomlWrite(toml::ser::Error),
    NoConfigDir,
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileError::Io(e) => write!(f, "I/O error: {e}"),
            ProfileError::Toml(e) => write!(f, "failed to parse profile TOML: {e}"),
            ProfileError::TomlWrite(e) => write!(f, "failed to serialize profile to TOML: {e}"),
            ProfileError::NoConfigDir => {
                write!(f, "could not determine user config directory (no $HOME?)")
            }
        }
    }
}

impl std::error::Error for ProfileError {}

/// Returns `~/.config/ds4l/profiles/`, creating it (and parents) if it
/// doesn't exist yet. Uses the `dirs` crate rather than hand-rolling
/// `$HOME/.config` so this respects `XDG_CONFIG_HOME` overrides and
/// works correctly on non-Linux platforms if this project ever expands
/// there.
pub fn profiles_dir() -> Result<PathBuf, ProfileError> {
    let base = dirs::config_dir().ok_or(ProfileError::NoConfigDir)?;
    let dir = base.join("ds4l").join("profiles");
    fs::create_dir_all(&dir).map_err(ProfileError::Io)?;
    Ok(dir)
}

fn profile_path(name: &str) -> Result<PathBuf, ProfileError> {
    Ok(profiles_dir()?.join(format!("{name}.toml")))
}

/// Loads a profile by name. If the profile doesn't exist yet AND the
/// requested name is "Default", creates and saves a fresh default profile
/// instead of erroring -- this is the first-run experience: a new
/// install has a sensible working profile without the user needing to
/// create one manually first.
pub fn load(name: &str) -> Result<Profile, ProfileError> {
    let path = profile_path(name)?;

    match fs::read_to_string(&path) {
        Ok(contents) => toml::from_str(&contents).map_err(ProfileError::Toml),
        Err(e) if e.kind() == io::ErrorKind::NotFound && name == "Default" => {
            let profile = Profile::default();
            save(&profile)?;
            Ok(profile)
        }
        Err(e) => Err(ProfileError::Io(e)),
    }
}

/// Serializes and writes a profile to `<profiles_dir>/<name>.toml`,
/// overwriting any existing file with that name.
pub fn save(profile: &Profile) -> Result<(), ProfileError> {
    let path = profile_path(&profile.name)?;
    let contents = toml::to_string_pretty(profile).map_err(ProfileError::TomlWrite)?;
    fs::write(&path, contents).map_err(ProfileError::Io)
}

/// Lists profile names available on disk (derived from `.toml` filenames
/// in the profiles directory), sorted alphabetically. Useful later for a
/// profile picker without needing to parse every file just to get names.
pub fn list_profile_names() -> Result<Vec<String>, ProfileError> {
    let dir = profiles_dir()?;
    let mut names = Vec::new();
    for entry in fs::read_dir(&dir).map_err(ProfileError::Io)? {
        let entry = entry.map_err(ProfileError::Io)?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}
