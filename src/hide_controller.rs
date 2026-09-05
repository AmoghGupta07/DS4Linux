//! "Origin controller hider" -- DS4Windows calls this "Hide DS4
//! Controller." On Windows this needs a kernel filter driver (HidHide/
//! HidGuardian) because Windows HID handles are exclusive-lockable. On
//! Linux, the equivalent effect is achieved much more simply: restrict
//! read/write permission on the real controller's device nodes
//! (`/dev/hidrawN`, `/dev/input/jsN`, `/dev/input/eventN`) so other
//! processes (Steam, games, `jstest`, etc.) can no longer open them --
//! while our own daemon keeps working, because a file descriptor opened
//! before permissions change stays valid; Linux doesn't revoke already-
//! open fds when chmod changes a node's mode bits.
//!
//! NATIVE PASSTHROUGH EXCLUSIONS: the DS4's kernel driver (hid-sony /
//! hid-playstation) doesn't register just one input device for the
//! whole controller -- it registers up to THREE separate ones, each
//! with a distinct name: the base gamepad (buttons/sticks), one
//! suffixed " Touchpad", and one suffixed " Motion Sensors" (gyro/
//! accel) -- confirmed directly against hid-sony.c kernel source
//! (`struct input_dev *touchpad; struct input_dev *sensor_dev;`,
//! `TOUCHPAD_SUFFIX` appended to the base device name) and cross-
//! checked against real-world Xorg config referencing the exact
//! product-name strings "...Wireless Controller Touchpad" and
//! "...Wireless Controller Motion Sensors". This means hiding the
//! controller doesn't have to be all-or-nothing: `hide()` can leave
//! specific sibling devices (by name suffix) untouched, so e.g. the
//! kernel's OWN already-correct touchpad or motion-sensor device stays
//! usable by other software even while everything else about the
//! controller is hidden from them -- see ds4l_daemon.rs's use of
//! `exclude_suffixes` for how TouchpadMode::Passthrough and
//! gyro_passthrough drive this.
//!
//! Scoped as REQUESTED: a runtime-only lock, not a persistent udev rule.
//! The controller is only hidden while this daemon is alive; permissions
//! are restored on normal exit AND on SIGINT/SIGTERM/SIGHUP (Ctrl+C,
//! `systemctl stop`, or a plain `kill <pid>`), so only `kill -9` (SIGKILL,
//! which cannot be caught by any process) is a way to leave the
//! controller in a hidden state outside our control -- and even then,
//! only until next reboot/replug, since these are just device-node
//! permission bits, not a persistent system rule.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Original permissions for one device node, saved so they can be
/// restored exactly rather than guessing a "default" mode to restore to.
struct SavedPermissions {
    path: PathBuf,
    original_mode: u32,
}

/// Holds the set of device nodes currently hidden and their original
/// permissions. Dropping this (via Rust's normal Drop, OR by calling
/// `restore()` explicitly from a signal handler before the process
/// exits) restores every node to its original mode.
pub struct HiddenController {
    saved: Vec<SavedPermissions>,
}

impl HiddenController {
    /// Finds and locks down every device node belonging to the same
    /// physical controller as the given hidraw path -- the hidraw node
    /// itself, plus any `js*`/`event*` nodes found under that same HID
    /// device's `input/inputN/` subdirectory in sysfs (these are what
    /// SDL/joydev-based games read directly, bypassing hidraw entirely)
    /// -- EXCEPT any sibling whose registered device name ends with one
    /// of `exclude_suffixes` (e.g. `" Touchpad"`, `" Motion Sensors"`),
    /// which is left completely untouched. The hidraw node itself is
    /// never excluded -- it's the daemon's own low-level read channel
    /// and hiding it is unrelated to which higher-level sibling devices
    /// other software should still be able to use.
    pub fn hide(hidraw_path: &Path, exclude_suffixes: &[&str]) -> Result<Self, String> {
        let mut saved = Vec::new();
        let mut skipped = Vec::new();

        for node in find_sibling_input_nodes(hidraw_path)? {
            if let Some(name) = sibling_device_name(&node) {
                if let Some(matched) = exclude_suffixes.iter().find(|suffix| name.ends_with(**suffix)) {
                    skipped.push(format!("{} ({name}, matched \"{matched}\")", node.display()));
                    continue;
                }
            }
            match lock_down(&node) {
                Ok(original_mode) => saved.push(SavedPermissions { path: node, original_mode }),
                Err(e) => {
                    // Roll back whatever we already hid before returning
                    // an error, so a partial failure doesn't leave some
                    // nodes hidden with no way to restore them (the
                    // caller won't get a HiddenController to call
                    // restore() on if this function errors).
                    let partial = HiddenController { saved };
                    partial.restore();
                    return Err(format!("failed to hide {}: {e}", node.display()));
                }
            }
        }

        // Hidraw itself is never excluded -- always locked down last so
        // a failure here still triggers the same rollback-and-error
        // path as any sibling failing above.
        match lock_down(hidraw_path) {
            Ok(original_mode) => saved.push(SavedPermissions {
                path: hidraw_path.to_path_buf(),
                original_mode,
            }),
            Err(e) => {
                let partial = HiddenController { saved };
                partial.restore();
                return Err(format!("failed to hide {}: {e}", hidraw_path.display()));
            }
        }

        println!(
            "Hid {} device node(s) from other processes: {}",
            saved.len(),
            saved.iter().map(|s| s.path.display().to_string()).collect::<Vec<_>>().join(", ")
        );
        if !skipped.is_empty() {
            println!("Left visible for native passthrough: {}", skipped.join(", "));
        }

        Ok(HiddenController { saved })
    }

    /// Restores every hidden node to its original permissions. Safe to
    /// call more than once (idempotent).
    pub fn restore(&self) {
        for s in &self.saved {
            if let Err(e) = restore_mode(&s.path, s.original_mode) {
                eprintln!(
                    "Warning: failed to restore permissions on {}: {e}\n\
                     (if this isn't a setcap/CAP_FOWNER issue, you may need to \
                     unplug/replug the controller or fix permissions manually: \
                     chmod {:o} {})",
                    s.path.display(),
                    s.original_mode,
                    s.path.display()
                );
            }
        }
    }
}

impl Drop for HiddenController {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Removes group/other read+write access to a device node, keeping
/// owner access intact so our already-open fd is unaffected and so the
/// node isn't left in a totally inaccessible state that would need root
/// to fix if restore() somehow didn't run.
///
/// NOTE: chmod-ing a node this process doesn't own requires either
/// running as that owner (normally root, for /dev/hidrawN and
/// /dev/input/{event,js}N) or holding CAP_FOWNER -- group membership
/// from a udev rule (which is enough to *open* these nodes) does NOT
/// grant permission to change their mode bits. So this binary needs
/// `sudo setcap cap_fowner,cap_dac_override+ep <path-to-binary>` once
/// per build (capabilities attach to the binary file itself and do not
/// survive a rebuild, so this must be re-run after every `cargo build`/
/// `cargo clean`). See permission_error_hint() below for the message
/// surfaced when this hasn't been done.
fn lock_down(path: &Path) -> Result<u32, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let original_mode = metadata.permissions().mode();

    // Keep owner bits, strip all group/other bits (read/write/execute).
    let hidden_mode = original_mode & 0o700;
    let perms = std::fs::Permissions::from_mode(hidden_mode);
    std::fs::set_permissions(path, perms).map_err(|e| permission_error_hint(&e))?;

    Ok(original_mode)
}

fn restore_mode(path: &Path, mode: u32) -> Result<(), String> {
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms).map_err(|e| permission_error_hint(&e))
}

/// Turns a bare permission-denied OS error into an actionable message
/// pointing at the actual fix, instead of leaving the person to
/// rediscover "chmod on a file you don't own needs CAP_FOWNER" from
/// scratch. Other error kinds (e.g. NotFound if the controller was
/// unplugged) pass through unchanged since the setcap hint wouldn't
/// apply to them.
fn permission_error_hint(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        format!(
            "{e} -- changing permissions on a device node this process \
             doesn't own requires CAP_FOWNER (and CAP_DAC_OVERRIDE to \
             restore it afterward), not just udev group membership. \
             Run once per build: \
             `sudo setcap cap_fowner,cap_dac_override+ep <path to ds4l_daemon binary>` \
             -- capabilities are attached to the binary file itself and \
             are lost on every rebuild, so re-run this after `cargo build`/`cargo clean`."
        )
    } else {
        e.to_string()
    }
}

/// Given a hidraw device path like `/dev/hidraw3`, finds sibling
/// `js*`/`event*` nodes under `/dev/input/` that belong to the SAME
/// physical device, by walking sysfs: `/sys/class/hidraw/hidraw3/device`
/// is a symlink to the HID device's own directory in the device tree
/// (e.g. `/sys/devices/.../0003:054C:09CC.0001/`), and THAT directory
/// -- not its parent -- has an `input/inputN/` subdirectory (for
/// controllers that register as an input device, which the DS4 does)
/// containing `js*` and `event*` device entries.
///
/// FIXED: an earlier version walked up to `real_device_dir`'s PARENT
/// (the USB/BT interface node, e.g. `1-2:1.0`) and looked for an
/// `input*`-named entry there. That directory doesn't contain one --
/// `input/` hangs off the HID device directory itself, one level
/// *down* from where the old code was looking, not sideways from a
/// level up. The old code's `read_dir` on the wrong directory simply
/// found no `input*` entries and silently returned an empty `Vec` (not
/// an error), so `hide()` locked down only the hidraw node, printed
/// "Hid 1 device node(s)..." and returned `Ok` -- reporting success
/// while `/dev/input/eventN` (what `evtest`/SDL/joydev actually read)
/// stayed fully world-readable. Confirmed against the standard kernel
/// HID sysfs layout, not just theorized.
fn find_sibling_input_nodes(hidraw_path: &Path) -> Result<Vec<PathBuf>, String> {
    let hidraw_name = hidraw_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "invalid hidraw path".to_string())?;

    let device_dir = PathBuf::from(format!("/sys/class/hidraw/{hidraw_name}/device"));
    let real_device_dir = std::fs::canonicalize(&device_dir)
        .map_err(|e| format!("could not resolve {}: {e}", device_dir.display()))?;

    // input/ is a CHILD of the HID device directory itself, containing
    // one (usually) inputN subdirectory, which in turn contains the
    // js*/event* device entries.
    let input_parent = real_device_dir.join("input");

    let mut found = Vec::new();

    let entries = match std::fs::read_dir(&input_parent) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No input/ subdirectory at all -- this device doesn't
            // register as a joystick/evdev input device. Unexpected for
            // a DS4 (which always does), but don't hard-fail the whole
            // hide() over it: the hidraw node itself still gets locked
            // down by the caller either way.
            return Ok(found);
        }
        Err(e) => return Err(format!("could not read {}: {e}", input_parent.display())),
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("input") {
            continue;
        }
        // Found an inputN directory -- look inside it for js*/event*
        // subdirectories, each of which corresponds to a /dev node of
        // the same name.
        let input_dir = entry.path();
        if let Ok(sub_entries) = std::fs::read_dir(&input_dir) {
            for sub in sub_entries.flatten() {
                let sub_name = sub.file_name();
                let sub_name = sub_name.to_string_lossy();
                if sub_name.starts_with("js") || sub_name.starts_with("event") {
                    let dev_node = PathBuf::from(format!("/dev/input/{sub_name}"));
                    if dev_node.exists() {
                        found.push(dev_node);
                    }
                }
            }
        }
    }

    Ok(found)
}

/// Reads a `/dev/input/{event,js}N` node's registered device name from
/// sysfs (`/sys/class/input/<basename>/device/name`) -- e.g. "Sony
/// Interactive Entertainment Wireless Controller Touchpad" or "...
/// Motion Sensors" for the DS4's kernel-registered sibling devices (see
/// module doc). Returns `None` on any error (missing file, permission,
/// non-UTF8) rather than propagating it -- a name lookup failing just
/// means that node can't be matched against an exclusion suffix and
/// will be hidden like any other unmatched sibling, which is the safe
/// default (fail toward MORE hidden, not less).
fn sibling_device_name(node_path: &Path) -> Option<String> {
    let basename = node_path.file_name()?.to_str()?;
    let name_path = PathBuf::from(format!("/sys/class/input/{basename}/device/name"));
    std::fs::read_to_string(&name_path).ok().map(|s| s.trim().to_string())
}
