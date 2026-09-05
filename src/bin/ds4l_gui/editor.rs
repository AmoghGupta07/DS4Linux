//! GTK4 profile editor: create/edit/delete profiles stored as plain TOML
//! under `~/.config/ds4l/profiles/` (same files ds4l_daemon reads --
//! see ds4l::profile). Editing here never talks to a running daemon by
//! itself; "Save & Apply" additionally sends SWITCH_PROFILE over the
//! control socket (ds4l::ipc) so edits can be tried live without
//! restarting the daemon.
//!
//! UNVERIFIED -- same caveat as tray.rs: written without the ability to
//! compile against the actual GTK4 crate in this environment. Before
//! trusting this day to day, build it (`cargo build --release --features
//! gui --bin ds4l_gui`), fix whatever the compiler flags (widget method
//! names/signatures are the most likely mismatch -- gtk4-rs's API
//! tracks upstream GTK4 fairly closely release to release, but I could
//! not check the exact resolved version here), and click through every
//! field once against a real profile file to confirm it round-trips
//! (edit a value, Save, quit, reopen, confirm the value stuck -- the
//! same "verify the primitive" approach used throughout this project).
//!
//! Deliberately NOT included this pass (documented gaps, not oversights):
//!   - No live validation feedback beyond what GTK's SpinButton ranges
//!     already enforce (e.g. RGB spins clamped 0-255 by their adjustment,
//!     so out-of-range values are structurally impossible rather than
//!     caught after the fact).
//!   - Rename/Duplicate don't check for a name collision with an
//!     existing profile before saving -- they'll just silently
//!     overwrite a same-named file, same as Save always has. Worth a
//!     confirm-before-overwrite prompt as a follow-up, same reasoning
//!     as the Delete confirmation this pass added.

use ds4l::gamepad_remap::{
    AxisDirection, GamepadButton, GamepadRemapConfig, GamepadTarget, OutputStick, OutputTrigger,
    StickAnalogSource, StickDigitalConfig, TriggerAnalogSource,
};
use ds4l::gyro_stick::{GateButton, GyroMode, GyroStickConfig};
use ds4l::kbm::{self, KbmConfig, KbmTarget, StickKbmMode};
use ds4l::profile::{self, Ds4FeedbackConfig, OutputMode, Profile};
use ds4l::touchpad::{TouchpadConfig, TouchpadMode};
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Builds the (initially hidden) editor window. `set_hide_on_close(true)`
/// means clicking the window's close button hides it rather than
/// destroying it or quitting the app -- the tray's "Edit Profiles..."
/// item re-shows the same window via `.present()` later, matching
/// typical "lives in the tray" desktop-app behavior.
pub fn build(app: &gtk4::Application) -> gtk4::ApplicationWindow {
    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("ds4l Profile Editor")
        .default_width(680)
        .default_height(660)
        .build();
    window.set_hide_on_close(true);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    root.set_margin_top(10);
    root.set_margin_bottom(10);
    root.set_margin_start(10);
    root.set_margin_end(10);

    // -- Row 1: profile picker + new/rename/duplicate/delete --
    let top_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let profile_combo = gtk4::ComboBoxText::new();
    let new_button = gtk4::Button::with_label("New...");
    let rename_button = gtk4::Button::with_label("Rename...");
    let duplicate_button = gtk4::Button::with_label("Duplicate...");
    let delete_button = gtk4::Button::with_label("Delete...");
    top_bar.append(&profile_combo);
    top_bar.append(&new_button);
    top_bar.append(&rename_button);
    top_bar.append(&duplicate_button);
    top_bar.append(&delete_button);
    root.append(&top_bar);

    // -- Row 2: which connected controller this profile targets, plus
    // that controller's LIVE status -- kept as its own row (rather than
    // buried in the bottom action bar) since "which controller" is
    // ongoing context for the whole editing session, not just a detail
    // of the Save & Apply action. --
    let controller_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let controller_bar_label = gtk4::Label::new(Some("Controller:"));
    let apply_target_combo = gtk4::ComboBoxText::new();
    apply_target_combo.set_tooltip_text(Some("Which connected controller to apply this profile to"));
    let refresh_controllers_button = gtk4::Button::with_label("\u{27f3}"); // ⟳
    refresh_controllers_button.set_tooltip_text(Some("Refresh connected controller list and status"));
    let controller_status_label = gtk4::Label::new(None);
    controller_status_label.set_xalign(0.0);
    controller_bar.append(&controller_bar_label);
    controller_bar.append(&apply_target_combo);
    controller_bar.append(&refresh_controllers_button);
    controller_bar.append(&controller_status_label);
    root.append(&controller_bar);

    // -- Form area, rebuilt fresh each time the selected profile
    // changes (see populate_form) rather than trying to keep a fixed
    // set of widgets in sync with two different profiles -- simpler
    // and, since this is a GUI form, the cost of rebuilding is
    // irrelevant. --
    let form = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_child(Some(&form));
    scroller.set_vexpand(true);
    root.append(&scroller);

    // -- Bottom bar: save actions + action-result status line --
    let bottom_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let save_button = gtk4::Button::with_label("Save");
    let save_apply_button = gtk4::Button::with_label("Save & Apply Live");
    let status_label = gtk4::Label::new(None);
    status_label.set_xalign(0.0);
    bottom_bar.append(&save_button);
    bottom_bar.append(&save_apply_button);
    bottom_bar.append(&status_label);
    root.append(&bottom_bar);

    window.set_child(Some(&root));

    refresh_controller_list(&apply_target_combo);
    refresh_controller_status_label(&apply_target_combo, &controller_status_label);
    {
        let apply_target_combo = apply_target_combo.clone();
        let controller_status_label = controller_status_label.clone();
        refresh_controllers_button.connect_clicked(move |_| {
            refresh_controller_list(&apply_target_combo);
            refresh_controller_status_label(&apply_target_combo, &controller_status_label);
        });
    }
    {
        let controller_status_label = controller_status_label.clone();
        apply_target_combo.connect_changed(move |combo| {
            refresh_controller_status_label(combo, &controller_status_label);
        });
    }
    // Live-ish auto-refresh while the window is actually visible (every
    // 2s -- frequent enough to feel current, infrequent enough that a
    // human wouldn't notice the polling cost even if they were watching
    // for it). Skipped while hidden in the tray so this doesn't
    // needlessly round-trip the control socket in the background for a
    // label nobody can see.
    {
        let apply_target_combo = apply_target_combo.clone();
        let controller_status_label = controller_status_label.clone();
        let window_for_check = window.clone();
        gtk4::glib::timeout_add_local(std::time::Duration::from_secs(2), move || {
            if window_for_check.is_visible() {
                refresh_controller_status_label(&apply_target_combo, &controller_status_label);
            }
            gtk4::glib::ControlFlow::Continue
        });
    }

    // Shared editing state: the profile currently loaded into the form.
    // Starts as a fresh Default profile named "Default" and gets
    // replaced wholesale whenever the profile_combo selection changes
    // or "New..."/"Duplicate..." creates one.
    let profile_rc: Rc<RefCell<Profile>> = Rc::new(RefCell::new(Profile::default()));

    refresh_profile_list(&profile_combo);
    populate_form(&form, &profile_rc, &status_label);

    // -- Wiring --

    // Shared load logic, used both by the combo's "changed" signal AND
    // once explicitly right after refresh_profile_list below.
    //
    // BUGFIX: an earlier version relied ONLY on the "changed" signal to
    // load the initially-selected profile's values into the form. But
    // refresh_profile_list() (which calls combo.set_active(Some(0)))
    // ran BEFORE connect_changed() was wired up below -- so the very
    // first selection never triggered a load, and the form silently
    // stayed populated with Profile::default() until the person
    // manually picked a different item in the dropdown. Symptom
    // matched exactly: reopening the GUI never showed the saved TOML's
    // actual values. Fix: extract the load into its own function and
    // call it explicitly once, right after the list is populated,
    // rather than trusting a signal to fire for a selection that
    // happened before anything was listening for it.
    fn load_profile_into(
        name: &str,
        profile_rc: &Rc<RefCell<Profile>>,
        form: &gtk4::Box,
        status_label: &gtk4::Label,
    ) {
        match profile::load(name) {
            Ok(loaded) => {
                *profile_rc.borrow_mut() = loaded;
                populate_form(form, profile_rc, status_label);
                status_label.set_text("");
            }
            Err(e) => {
                status_label.set_text(&format!("Failed to load \"{name}\": {e}"));
            }
        }
    }

    {
        let profile_rc = profile_rc.clone();
        let form = form.clone();
        let status_label = status_label.clone();
        profile_combo.connect_changed(move |combo| {
            let Some(name) = combo.active_text() else { return };
            load_profile_into(&name, &profile_rc, &form, &status_label);
        });
    }

    // Explicitly load whichever profile refresh_profile_list() selected
    // above -- see the BUGFIX note on load_profile_into for why this
    // can't be left to the "changed" signal alone.
    if let Some(name) = profile_combo.active_text() {
        load_profile_into(&name, &profile_rc, &form, &status_label);
    }

    {
        let profile_combo = profile_combo.clone();
        let profile_rc = profile_rc.clone();
        let form = form.clone();
        let status_label = status_label.clone();
        let window_for_dialog = window.clone();
        new_button.connect_clicked(move |_| {
            show_text_input_dialog(&window_for_dialog, "New Profile", "Profile name", None, {
                let profile_combo = profile_combo.clone();
                let profile_rc = profile_rc.clone();
                let form = form.clone();
                let status_label = status_label.clone();
                move |name| {
                    let new_profile = Profile {
                        name: name.clone(),
                        ..Profile::default()
                    };
                    match profile::save(&new_profile) {
                        Ok(()) => {
                            *profile_rc.borrow_mut() = new_profile;
                            refresh_profile_list(&profile_combo);
                            profile_combo.set_active_id(Some(&name));
                            populate_form(&form, &profile_rc, &status_label);
                            status_label.set_text(&format!("Created \"{name}\"."));
                        }
                        Err(e) => {
                            status_label.set_text(&format!("Failed to create \"{name}\": {e}"));
                        }
                    }
                }
            });
        });
    }

    {
        let profile_combo = profile_combo.clone();
        let profile_rc = profile_rc.clone();
        let form = form.clone();
        let status_label = status_label.clone();
        let window_for_dialog = window.clone();
        rename_button.connect_clicked(move |_| {
            let old_name = profile_rc.borrow().name.clone();
            if old_name.is_empty() {
                return;
            }
            show_text_input_dialog(
                &window_for_dialog,
                "Rename Profile",
                "New name",
                Some(&old_name),
                {
                    let profile_combo = profile_combo.clone();
                    let profile_rc = profile_rc.clone();
                    let form = form.clone();
                    let status_label = status_label.clone();
                    let old_name = old_name.clone();
                    move |new_name| {
                        if new_name == old_name {
                            return; // no-op rename, nothing to do
                        }
                        // Renaming is: save the CURRENT in-memory edits
                        // under the new name (not a fresh reload from
                        // disk -- rename shouldn't discard unsaved
                        // changes the person was mid-editing), then
                        // remove the old file. Save happens FIRST so a
                        // failure (e.g. invalid filename) never leaves
                        // the profile in a state where both files are
                        // gone.
                        let mut renamed = profile_rc.borrow().clone();
                        renamed.name = new_name.clone();
                        match profile::save(&renamed) {
                            Ok(()) => {
                                if let Ok(dir) = profile::profiles_dir() {
                                    let old_path = dir.join(format!("{old_name}.toml"));
                                    let _ = std::fs::remove_file(&old_path); // best-effort cleanup
                                }
                                *profile_rc.borrow_mut() = renamed;
                                refresh_profile_list(&profile_combo);
                                profile_combo.set_active_id(Some(&new_name));
                                populate_form(&form, &profile_rc, &status_label);
                                status_label.set_text(&format!(
                                    "Renamed \"{old_name}\" to \"{new_name}\"."
                                ));
                            }
                            Err(e) => {
                                status_label.set_text(&format!(
                                    "Failed to rename \"{old_name}\" to \"{new_name}\": {e}"
                                ));
                            }
                        }
                    }
                },
            );
        });
    }

    {
        let profile_combo = profile_combo.clone();
        let profile_rc = profile_rc.clone();
        let form = form.clone();
        let status_label = status_label.clone();
        let window_for_dialog = window.clone();
        duplicate_button.connect_clicked(move |_| {
            let current_name = profile_rc.borrow().name.clone();
            if current_name.is_empty() {
                return;
            }
            let suggested = format!("{current_name} copy");
            show_text_input_dialog(
                &window_for_dialog,
                "Duplicate Profile",
                "New profile name",
                Some(&suggested),
                {
                    let profile_combo = profile_combo.clone();
                    let profile_rc = profile_rc.clone();
                    let form = form.clone();
                    let status_label = status_label.clone();
                    move |new_name| {
                        // Duplicates the CURRENT in-memory edits (not a
                        // fresh reload from disk), so unsaved changes
                        // carry into the copy too -- matches "duplicate
                        // what I'm looking at right now," which is the
                        // more useful reading of the button for someone
                        // mid-edit. The original profile's own file is
                        // untouched either way.
                        let mut duplicate = profile_rc.borrow().clone();
                        duplicate.name = new_name.clone();
                        match profile::save(&duplicate) {
                            Ok(()) => {
                                *profile_rc.borrow_mut() = duplicate;
                                refresh_profile_list(&profile_combo);
                                profile_combo.set_active_id(Some(&new_name));
                                populate_form(&form, &profile_rc, &status_label);
                                status_label.set_text(&format!("Duplicated as \"{new_name}\"."));
                            }
                            Err(e) => {
                                status_label.set_text(&format!(
                                    "Failed to duplicate as \"{new_name}\": {e}"
                                ));
                            }
                        }
                    }
                },
            );
        });
    }

    {
        let profile_combo = profile_combo.clone();
        let status_label = status_label.clone();
        let window_for_dialog = window.clone();
        delete_button.connect_clicked(move |_| {
            let Some(name) = profile_combo.active_text() else { return };
            show_confirm_dialog(
                &window_for_dialog,
                "Delete Profile",
                &format!("Delete profile \"{name}\"? This cannot be undone."),
                "Delete",
                {
                    let profile_combo = profile_combo.clone();
                    let status_label = status_label.clone();
                    move || {
                        match profile::profiles_dir() {
                            Ok(dir) => {
                                let path = dir.join(format!("{name}.toml"));
                                match std::fs::remove_file(&path) {
                                    Ok(()) => {
                                        status_label.set_text(&format!("Deleted \"{name}\"."));
                                        refresh_profile_list(&profile_combo);
                                    }
                                    Err(e) => {
                                        status_label.set_text(&format!(
                                            "Failed to delete \"{name}\": {e}"
                                        ));
                                    }
                                }
                            }
                            Err(e) => status_label.set_text(&format!("{e}")),
                        }
                    }
                },
            );
        });
    }

    {
        let profile_rc = profile_rc.clone();
        let status_label = status_label.clone();
        save_button.connect_clicked(move |_| {
            let result = profile::save(&profile_rc.borrow());
            match result {
                Ok(()) => status_label.set_text(&format!(
                    "Saved \"{}\".",
                    profile_rc.borrow().name
                )),
                Err(e) => status_label.set_text(&format!("Save failed: {e}")),
            }
        });
    }

    {
        let profile_rc = profile_rc.clone();
        let status_label = status_label.clone();
        let apply_target_combo = apply_target_combo.clone();
        let controller_status_label = controller_status_label.clone();
        save_apply_button.connect_clicked(move |_| {
            let name = profile_rc.borrow().name.clone();
            // Empty string is the sentinel used by refresh_controller_list
            // for its "(no controllers connected)"/"(daemon not running)"
            // placeholder entries -- filter it out so those don't get
            // treated as a real controller id.
            let controller_id = apply_target_combo
                .active_id()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());

            match profile::save(&profile_rc.borrow()) {
                Ok(()) => match controller_id {
                    Some(id) => match ds4l::ipc::switch_profile(&id, &name) {
                        Ok(()) => {
                            status_label.set_text(&format!("Saved and applied \"{name}\" live to {id}."));
                            refresh_controller_status_label(&apply_target_combo, &controller_status_label);
                        }
                        Err(e) => {
                            status_label.set_text(&format!(
                                "Saved \"{name}\", but could not apply live to {id}: {e}"
                            ));
                        }
                    },
                    None => {
                        status_label.set_text(&format!(
                            "Saved \"{name}\", but no controller is connected to apply it to \
                             (is ds4l_daemon running with a controller plugged in?)."
                        ));
                    }
                },
                Err(e) => status_label.set_text(&format!("Save failed: {e}")),
            }
        });
    }

    window
}

/// Updates the live controller-status label based on whichever
/// controller is currently selected in `combo`. Called on initial
/// build, on manual refresh, on selection change, on a successful Save
/// & Apply, and every 2s while the window is visible (see build()).
fn refresh_controller_status_label(combo: &gtk4::ComboBoxText, label: &gtk4::Label) {
    let controller_id = combo.active_id().map(|s| s.to_string()).filter(|s| !s.is_empty());
    match controller_id {
        None => label.set_text(""),
        Some(id) => match ds4l::ipc::status(&id) {
            Ok(raw) => label.set_text(&format_controller_status(&raw)),
            Err(_) => label.set_text("(unreachable)"),
        },
    }
}

/// Turns a raw "OK profile=X mode=Y connection=Z hidden=W battery=N
/// charging=B" STATUS line into a short display string. Mirrors
/// tray.rs's format_status_line but doesn't repeat the profile name
/// (the editor already shows that in profile_combo right above) --
/// just mode/connection/hidden/battery, the parts that actually add
/// information here.
fn format_controller_status(raw: &str) -> String {
    let mode = ds4l::ipc::parse_status_field(raw, "mode");
    let connection = ds4l::ipc::parse_status_field(raw, "connection");
    let hidden = ds4l::ipc::parse_status_field(raw, "hidden").as_deref() == Some("true");
    let battery = ds4l::ipc::parse_status_field(raw, "battery");
    let charging = ds4l::ipc::parse_status_field(raw, "charging").as_deref() == Some("true");

    match (mode, connection) {
        (Some(m), Some(c)) => {
            let mut parts = vec![m, c];
            if let Some(pct) = battery {
                parts.push(if charging {
                    format!("{pct}% \u{26a1}")
                } else {
                    format!("{pct}%")
                });
            }
            if hidden {
                parts.push("hidden from other apps".to_string());
            }
            parts.join(", ")
        }
        _ => raw.to_string(),
    }
}

fn refresh_controller_list(combo: &gtk4::ComboBoxText) {
    let previously_selected = combo.active_id().map(|s| s.to_string());
    combo.remove_all();
    match ds4l::ipc::list_controllers() {
        Ok(ids) if !ids.is_empty() => {
            for id in &ids {
                combo.append(Some(id), id);
            }
            // Keep whatever was selected before if it's still present
            // (e.g. after a Save that doesn't otherwise touch this
            // dropdown), otherwise default to the first controller.
            match previously_selected {
                Some(id) if ids.contains(&id) => {
                    combo.set_active_id(Some(&id));
                }
                _ => {
                    combo.set_active(Some(0));
                }
            };
        }
        Ok(_) => {
            combo.append(Some(""), "(no controllers connected)");
            combo.set_active(Some(0));
        }
        Err(_) => {
            combo.append(Some(""), "(daemon not running)");
            combo.set_active(Some(0));
        }
    }
}

fn refresh_profile_list(combo: &gtk4::ComboBoxText) {
    combo.remove_all();
    match profile::list_profile_names() {
        Ok(names) => {
            for name in names {
                combo.append(Some(&name.clone()), &name);
            }
        }
        Err(e) => {
            eprintln!("Failed to list profiles: {e}");
        }
    }
    if combo.active().is_none() {
        combo.set_active(Some(0));
    }
}

/// Rebuilds the entire form area from scratch to reflect `profile_rc`'s
/// current contents. Called on initial load and every time the
/// selected/loaded profile changes. Each field's change callback
/// mutates `profile_rc` in place; nothing here writes to disk (that
/// only happens on Save / Save & Apply).
fn populate_form(form: &gtk4::Box, profile_rc: &Rc<RefCell<Profile>>, status_label: &gtk4::Label) {
    while let Some(child) = form.first_child() {
        form.remove(&child);
    }

    let notebook = gtk4::Notebook::new();
    notebook.append_page(&build_general_tab(profile_rc), Some(&gtk4::Label::new(Some("General"))));
    notebook.append_page(&build_gyro_tab(profile_rc), Some(&gtk4::Label::new(Some("Gyro"))));
    notebook.append_page(&build_touchpad_tab(profile_rc), Some(&gtk4::Label::new(Some("Touchpad"))));
    notebook.append_page(&build_feedback_tab(profile_rc), Some(&gtk4::Label::new(Some("Feedback"))));
    notebook.append_page(&build_kbm_tab(profile_rc), Some(&gtk4::Label::new(Some("KBM Mapping"))));
    notebook.append_page(
        &build_gamepad_remap_tab(profile_rc),
        Some(&gtk4::Label::new(Some("Gamepad Remap"))),
    );
    form.append(&notebook);

    let _ = status_label; // reserved for future inline field-level validation messages
}

fn section_box() -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    b.set_margin_top(10);
    b.set_margin_bottom(10);
    b.set_margin_start(10);
    b.set_margin_end(10);
    b
}

fn labeled_row(label: &str, widget: &impl IsA<gtk4::Widget>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let lbl = gtk4::Label::new(Some(label));
    lbl.set_width_chars(16);
    lbl.set_xalign(0.0);
    row.append(&lbl);
    row.append(widget);
    row
}

fn spin(min: f64, max: f64, step: f64, value: f64) -> gtk4::SpinButton {
    let s = gtk4::SpinButton::with_range(min, max, step);
    s.set_value(value);
    s.set_digits(if step < 1.0 { 2 } else { 0 });
    s
}

// ---- General tab ----

fn build_general_tab(profile_rc: &Rc<RefCell<Profile>>) -> gtk4::Box {
    let b = section_box();

    let mode_combo = gtk4::ComboBoxText::new();
    mode_combo.append(Some("gamepad"), "Gamepad (native DS4)");
    mode_combo.append(Some("xbox360"), "Xbox 360 (Xinput-compatible)");
    mode_combo.append(Some("kbm"), "Keyboard + Mouse");
    mode_combo.set_active_id(Some(match profile_rc.borrow().output_mode {
        OutputMode::Gamepad => "gamepad",
        OutputMode::Xbox360 => "xbox360",
        OutputMode::Kbm => "kbm",
    }));
    {
        let profile_rc = profile_rc.clone();
        mode_combo.connect_changed(move |c| {
            profile_rc.borrow_mut().output_mode = match c.active_id().as_deref() {
                Some("kbm") => OutputMode::Kbm,
                Some("xbox360") => OutputMode::Xbox360,
                _ => OutputMode::Gamepad,
            };
        });
    }
    b.append(&labeled_row("Output mode", &mode_combo));

    let hide_check = gtk4::CheckButton::with_label("Hide real controller from other apps while running");
    hide_check.set_active(profile_rc.borrow().hide_real_controller);
    {
        let profile_rc = profile_rc.clone();
        hide_check.connect_toggled(move |c| {
            profile_rc.borrow_mut().hide_real_controller = c.is_active();
        });
    }
    b.append(&hide_check);

    b
}

// ---- Gyro tab ----

/// (dropdown id, display label, GateButton) for every button gyro
/// activation can be gated on. Any DS4 button works here -- unlike the
/// KBM key options above, this isn't limited to a curated subset,
/// since GateButton itself covers every digital button on the pad (see
/// gyro_stick.rs).
const GATE_BUTTON_OPTIONS: &[(&str, &str, GateButton)] = &[
    ("l1", "L1", GateButton::L1),
    ("r1", "R1", GateButton::R1),
    ("l2", "L2 (analog threshold)", GateButton::L2),
    ("r2", "R2 (analog threshold)", GateButton::R2),
    ("l2_digital", "L2 (digital click)", GateButton::L2Digital),
    ("r2_digital", "R2 (digital click)", GateButton::R2Digital),
    ("l3", "L3 (stick click)", GateButton::L3),
    ("r3", "R3 (stick click)", GateButton::R3),
    ("cross", "Cross (X)", GateButton::Cross),
    ("circle", "Circle (O)", GateButton::Circle),
    ("square", "Square", GateButton::Square),
    ("triangle", "Triangle", GateButton::Triangle),
    ("share", "Share", GateButton::Share),
    ("options", "Options", GateButton::Options),
    ("ps", "PS button", GateButton::Ps),
    ("touchpad_click", "Touchpad click", GateButton::TouchpadClick),
];

fn gate_button_id(button: GateButton) -> &'static str {
    GATE_BUTTON_OPTIONS
        .iter()
        .find(|(_, _, b)| *b == button)
        .map(|(id, _, _)| *id)
        .unwrap_or("l2")
}

fn gate_button_from_id(id: &str) -> GateButton {
    GATE_BUTTON_OPTIONS
        .iter()
        .find(|(option_id, _, _)| *option_id == id)
        .map(|(_, _, b)| *b)
        .unwrap_or(GateButton::L2)
}

fn build_gyro_tab(profile_rc: &Rc<RefCell<Profile>>) -> gtk4::Box {
    let b = section_box();
    let cfg: GyroStickConfig = profile_rc.borrow().gyro;

    let mode_combo = gtk4::ComboBoxText::new();
    mode_combo.append(Some("always_on"), "Always On");
    mode_combo.append(Some("toggle"), "Toggle (press L2)");
    mode_combo.append(Some("hold"), "Hold (hold L2)");
    mode_combo.append(Some("disabled"), "Disabled (no gyro-to-stick)");
    mode_combo.set_active_id(Some(match cfg.mode {
        GyroMode::AlwaysOn => "always_on",
        GyroMode::Toggle => "toggle",
        GyroMode::Hold => "hold",
        GyroMode::Disabled => "disabled",
    }));
    {
        let profile_rc = profile_rc.clone();
        mode_combo.connect_changed(move |c| {
            profile_rc.borrow_mut().gyro.mode = match c.active_id().as_deref() {
                Some("always_on") => GyroMode::AlwaysOn,
                Some("toggle") => GyroMode::Toggle,
                Some("disabled") => GyroMode::Disabled,
                _ => GyroMode::Hold,
            };
        });
    }
    b.append(&labeled_row("Gate mode", &mode_combo));

    let gate_combo = gtk4::ComboBoxText::new();
    for (id, label, _) in GATE_BUTTON_OPTIONS {
        gate_combo.append(Some(id), label);
    }
    gate_combo.set_active_id(Some(gate_button_id(cfg.gate_button)));
    {
        let profile_rc = profile_rc.clone();
        gate_combo.connect_changed(move |c| {
            if let Some(id) = c.active_id() {
                profile_rc.borrow_mut().gyro.gate_button = gate_button_from_id(&id);
            }
        });
    }
    b.append(&labeled_row("Gate button", &gate_combo));

    let sens = spin(10.0, 1000.0, 5.0, cfg.deg_per_sec_at_full_stick);
    {
        let profile_rc = profile_rc.clone();
        sens.connect_value_changed(move |s| {
            profile_rc.borrow_mut().gyro.deg_per_sec_at_full_stick = s.value();
        });
    }
    b.append(&labeled_row("deg/s for full stick", &sens));

    let smoothing = spin(0.0, 1.0, 0.05, cfg.smoothing_alpha);
    {
        let profile_rc = profile_rc.clone();
        smoothing.connect_value_changed(move |s| {
            profile_rc.borrow_mut().gyro.smoothing_alpha = s.value();
        });
    }
    b.append(&labeled_row("Smoothing (0=smooth, 1=raw)", &smoothing));

    let deadzone = spin(0.0, 20.0, 0.5, cfg.deadzone_deg_s);
    {
        let profile_rc = profile_rc.clone();
        deadzone.connect_value_changed(move |s| {
            profile_rc.borrow_mut().gyro.deadzone_deg_s = s.value();
        });
    }
    b.append(&labeled_row("Deadzone (deg/s)", &deadzone));

    let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    b.append(&separator);

    let passthrough_check = gtk4::CheckButton::with_label(
        "Also expose real gyro/accel to other apps (native passthrough)",
    );
    passthrough_check.set_active(profile_rc.borrow().gyro_passthrough);
    {
        let profile_rc = profile_rc.clone();
        passthrough_check.connect_toggled(move |c| {
            profile_rc.borrow_mut().gyro_passthrough = c.is_active();
        });
    }
    b.append(&passthrough_check);

    let passthrough_note = gtk4::Label::new(Some(
        "Only matters if \"Hide real controller\" (General tab) is also on -- otherwise the \
         kernel's own Motion Sensors device is already visible to everything regardless. \
         Independent of gyro-to-stick above: this doesn't affect it, and helps tools that read \
         the kernel's own gyro device directly (e.g. some emulator DSU relays) -- most SDL2 \
         games read gyro their own way and won't see a difference either way.",
    ));
    passthrough_note.set_xalign(0.0);
    passthrough_note.set_wrap(true);
    passthrough_note.add_css_class("dim-label");
    b.append(&passthrough_note);

    b
}

// ---- Touchpad tab ----

fn build_touchpad_tab(profile_rc: &Rc<RefCell<Profile>>) -> gtk4::Box {
    let b = section_box();
    let cfg: TouchpadConfig = profile_rc.borrow().touchpad;

    let mode_combo = gtk4::ComboBoxText::new();
    mode_combo.append(Some("passthrough"), "Passthrough (kernel's own native touchpad device)");
    mode_combo.append(Some("mouse_remap"), "Mouse Remap (relative)");
    mode_combo.append(Some("absolute_mouse"), "Absolute Mouse (tablet-style)");
    mode_combo.append(Some("disabled"), "Disabled (fully suppressed)");
    mode_combo.set_active_id(Some(match cfg.mode {
        TouchpadMode::Passthrough => "passthrough",
        TouchpadMode::MouseRemap => "mouse_remap",
        TouchpadMode::AbsoluteMouse => "absolute_mouse",
        TouchpadMode::Disabled => "disabled",
    }));
    {
        let profile_rc = profile_rc.clone();
        mode_combo.connect_changed(move |c| {
            profile_rc.borrow_mut().touchpad.mode = match c.active_id().as_deref() {
                Some("mouse_remap") => TouchpadMode::MouseRemap,
                Some("absolute_mouse") => TouchpadMode::AbsoluteMouse,
                Some("disabled") => TouchpadMode::Disabled,
                _ => TouchpadMode::Passthrough,
            };
        });
    }
    b.append(&labeled_row("Mode", &mode_combo));

    let passthrough_note = gtk4::Label::new(Some(
        "Passthrough emulates nothing on this project's end -- it relies entirely on the \
         Linux kernel's own separate Touchpad device for the controller (already visible to \
         everything unless \"Hide real controller\" on the General tab is also on, in which \
         case Passthrough specifically keeps that one device excluded from hiding). Disabled \
         is the same except it does NOT keep that device excluded -- if hiding is on, the \
         touchpad is hidden along with everything else.",
    ));
    passthrough_note.set_xalign(0.0);
    passthrough_note.set_wrap(true);
    passthrough_note.add_css_class("dim-label");
    b.append(&passthrough_note);

    let mouse_sens = spin(0.0, 5.0, 0.05, cfg.mouse_sensitivity);
    {
        let profile_rc = profile_rc.clone();
        mouse_sens.connect_value_changed(move |s| {
            profile_rc.borrow_mut().touchpad.mouse_sensitivity = s.value();
        });
    }
    b.append(&labeled_row("Mouse sensitivity", &mouse_sens));

    let scroll_sens = spin(0.0, 1.0, 0.01, cfg.scroll_sensitivity);
    {
        let profile_rc = profile_rc.clone();
        scroll_sens.connect_value_changed(move |s| {
            profile_rc.borrow_mut().touchpad.scroll_sensitivity = s.value();
        });
    }
    b.append(&labeled_row("Scroll sensitivity", &scroll_sens));

    let absolute_note = gtk4::Label::new(Some(
        "(Sensitivity settings above only apply to Mouse Remap -- Absolute Mouse maps the \
         touchpad directly to the screen with no scaling.)",
    ));
    absolute_note.set_xalign(0.0);
    absolute_note.set_wrap(true);
    absolute_note.add_css_class("dim-label");
    b.append(&absolute_note);

    b
}

// ---- Feedback tab ----

fn build_feedback_tab(profile_rc: &Rc<RefCell<Profile>>) -> gtk4::Box {
    let b = section_box();
    let cfg: Ds4FeedbackConfig = profile_rc.borrow().feedback;

    let red = spin(0.0, 255.0, 1.0, cfg.lightbar.red as f64);
    let green = spin(0.0, 255.0, 1.0, cfg.lightbar.green as f64);
    let blue = spin(0.0, 255.0, 1.0, cfg.lightbar.blue as f64);
    for (spin_button, setter) in [
        (&red, set_lightbar_red as fn(&Rc<RefCell<Profile>>, u8)),
        (&green, set_lightbar_green),
        (&blue, set_lightbar_blue),
    ] {
        let profile_rc = profile_rc.clone();
        spin_button.connect_value_changed(move |s| {
            setter(&profile_rc, s.value().round() as u8);
        });
    }
    b.append(&labeled_row("Lightbar red", &red));
    b.append(&labeled_row("Lightbar green", &green));
    b.append(&labeled_row("Lightbar blue", &blue));

    let rumble_check = gtk4::CheckButton::with_label("Pulse rumble briefly when profile loads");
    rumble_check.set_active(cfg.rumble_on_load);
    {
        let profile_rc = profile_rc.clone();
        rumble_check.connect_toggled(move |c| {
            profile_rc.borrow_mut().feedback.rumble_on_load = c.is_active();
        });
    }
    b.append(&rumble_check);

    let battery_flash_check = gtk4::CheckButton::with_label(
        "Flash lightbar red when battery is low (\u{2264}20%) and not charging",
    );
    battery_flash_check.set_active(cfg.low_battery_flash);
    {
        let profile_rc = profile_rc.clone();
        battery_flash_check.connect_toggled(move |c| {
            profile_rc.borrow_mut().feedback.low_battery_flash = c.is_active();
        });
    }
    b.append(&battery_flash_check);

    let rainbow_check = gtk4::CheckButton::with_label(
        "Rainbow: cycle lightbar through colors instead of the static color above",
    );
    rainbow_check.set_active(cfg.rainbow);
    {
        let profile_rc = profile_rc.clone();
        rainbow_check.connect_toggled(move |c| {
            profile_rc.borrow_mut().feedback.rainbow = c.is_active();
        });
    }
    b.append(&rainbow_check);

    let rainbow_note = gtk4::Label::new(Some(
        "If both this and the low-battery flash above are on, the battery warning takes \
         priority while it's active.",
    ));
    rainbow_note.set_xalign(0.0);
    rainbow_note.set_wrap(true);
    rainbow_note.add_css_class("dim-label");
    b.append(&rainbow_note);

    b
}

fn set_lightbar_red(p: &Rc<RefCell<Profile>>, v: u8) {
    p.borrow_mut().feedback.lightbar.red = v;
}
fn set_lightbar_green(p: &Rc<RefCell<Profile>>, v: u8) {
    p.borrow_mut().feedback.lightbar.green = v;
}
fn set_lightbar_blue(p: &Rc<RefCell<Profile>>, v: u8) {
    p.borrow_mut().feedback.lightbar.blue = v;
}

// ---- KBM mapping tab ----

/// (display label, evdev KEY_* code) for every key kbm.rs now exposes --
/// the FULL keyboard (letters, numbers, F1-F24, navigation cluster,
/// numpad, both Ctrl/Shift/Alt/Meta, media keys), not the earlier
/// curated common-keys subset. Matches kbm.rs's constants 1:1; extend
/// both together if a key is ever missing from either.
const KEY_OPTIONS: &[(&str, u16)] = &[
    ("Esc", kbm::KEY_ESC),
    ("1", kbm::KEY_1),
    ("2", kbm::KEY_2),
    ("3", kbm::KEY_3),
    ("4", kbm::KEY_4),
    ("5", kbm::KEY_5),
    ("6", kbm::KEY_6),
    ("7", kbm::KEY_7),
    ("8", kbm::KEY_8),
    ("9", kbm::KEY_9),
    ("0", kbm::KEY_0),
    ("Minus (-)", kbm::KEY_MINUS),
    ("Equal (=)", kbm::KEY_EQUAL),
    ("Backspace", kbm::KEY_BACKSPACE),
    ("Tab", kbm::KEY_TAB),
    ("Q", kbm::KEY_Q),
    ("W", kbm::KEY_W),
    ("E", kbm::KEY_E),
    ("R", kbm::KEY_R),
    ("T", kbm::KEY_T),
    ("Y", kbm::KEY_Y),
    ("U", kbm::KEY_U),
    ("I", kbm::KEY_I),
    ("O", kbm::KEY_O),
    ("P", kbm::KEY_P),
    ("[ (Left Brace)", kbm::KEY_LEFTBRACE),
    ("] (Right Brace)", kbm::KEY_RIGHTBRACE),
    ("Enter", kbm::KEY_ENTER),
    ("Left Ctrl", kbm::KEY_LEFTCTRL),
    ("A", kbm::KEY_A),
    ("S", kbm::KEY_S),
    ("D", kbm::KEY_D),
    ("F", kbm::KEY_F),
    ("G", kbm::KEY_G),
    ("H", kbm::KEY_H),
    ("J", kbm::KEY_J),
    ("K", kbm::KEY_K),
    ("L", kbm::KEY_L),
    ("; (Semicolon)", kbm::KEY_SEMICOLON),
    ("' (Apostrophe)", kbm::KEY_APOSTROPHE),
    ("` (Grave)", kbm::KEY_GRAVE),
    ("Left Shift", kbm::KEY_LEFTSHIFT),
    ("\\ (Backslash)", kbm::KEY_BACKSLASH),
    ("Z", kbm::KEY_Z),
    ("X", kbm::KEY_X),
    ("C", kbm::KEY_C),
    ("V", kbm::KEY_V),
    ("B", kbm::KEY_B),
    ("N", kbm::KEY_N),
    ("M", kbm::KEY_M),
    (", (Comma)", kbm::KEY_COMMA),
    (". (Period)", kbm::KEY_DOT),
    ("/ (Slash)", kbm::KEY_SLASH),
    ("Right Shift", kbm::KEY_RIGHTSHIFT),
    ("Numpad *", kbm::KEY_KPASTERISK),
    ("Left Alt", kbm::KEY_LEFTALT),
    ("Space", kbm::KEY_SPACE),
    ("Caps Lock", kbm::KEY_CAPSLOCK),
    ("F1", kbm::KEY_F1),
    ("F2", kbm::KEY_F2),
    ("F3", kbm::KEY_F3),
    ("F4", kbm::KEY_F4),
    ("F5", kbm::KEY_F5),
    ("F6", kbm::KEY_F6),
    ("F7", kbm::KEY_F7),
    ("F8", kbm::KEY_F8),
    ("F9", kbm::KEY_F9),
    ("F10", kbm::KEY_F10),
    ("Num Lock", kbm::KEY_NUMLOCK),
    ("Scroll Lock", kbm::KEY_SCROLLLOCK),
    ("Numpad 7", kbm::KEY_KP7),
    ("Numpad 8", kbm::KEY_KP8),
    ("Numpad 9", kbm::KEY_KP9),
    ("Numpad -", kbm::KEY_KPMINUS),
    ("Numpad 4", kbm::KEY_KP4),
    ("Numpad 5", kbm::KEY_KP5),
    ("Numpad 6", kbm::KEY_KP6),
    ("Numpad +", kbm::KEY_KPPLUS),
    ("Numpad 1", kbm::KEY_KP1),
    ("Numpad 2", kbm::KEY_KP2),
    ("Numpad 3", kbm::KEY_KP3),
    ("Numpad 0", kbm::KEY_KP0),
    ("Numpad .", kbm::KEY_KPDOT),
    ("102nd key (\\|, ISO keyboards)", kbm::KEY_102ND),
    ("F11", kbm::KEY_F11),
    ("F12", kbm::KEY_F12),
    ("Numpad Enter", kbm::KEY_KPENTER),
    ("Right Ctrl", kbm::KEY_RIGHTCTRL),
    ("Numpad /", kbm::KEY_KPSLASH),
    ("Print Screen", kbm::KEY_SYSRQ),
    ("Right Alt", kbm::KEY_RIGHTALT),
    ("Home", kbm::KEY_HOME),
    ("Up Arrow", kbm::KEY_UP),
    ("Page Up", kbm::KEY_PAGEUP),
    ("Left Arrow", kbm::KEY_LEFT),
    ("Right Arrow", kbm::KEY_RIGHT),
    ("End", kbm::KEY_END),
    ("Down Arrow", kbm::KEY_DOWN),
    ("Page Down", kbm::KEY_PAGEDOWN),
    ("Insert", kbm::KEY_INSERT),
    ("Delete", kbm::KEY_DELETE),
    ("Mute", kbm::KEY_MUTE),
    ("Volume Down", kbm::KEY_VOLUMEDOWN),
    ("Volume Up", kbm::KEY_VOLUMEUP),
    ("Pause/Break", kbm::KEY_PAUSE),
    ("Left Meta (Super/Win)", kbm::KEY_LEFTMETA),
    ("Right Meta (Super/Win)", kbm::KEY_RIGHTMETA),
    ("Menu/Compose", kbm::KEY_COMPOSE),
    ("F13", kbm::KEY_F13),
    ("F14", kbm::KEY_F14),
    ("F15", kbm::KEY_F15),
    ("F16", kbm::KEY_F16),
    ("F17", kbm::KEY_F17),
    ("F18", kbm::KEY_F18),
    ("F19", kbm::KEY_F19),
    ("F20", kbm::KEY_F20),
    ("F21", kbm::KEY_F21),
    ("F22", kbm::KEY_F22),
    ("F23", kbm::KEY_F23),
    ("F24", kbm::KEY_F24),
];

/// Human-readable name for a key code, via reverse lookup in
/// KEY_OPTIONS. Falls back to a raw numeric label for any code that
/// somehow isn't in that table (shouldn't normally happen, since every
/// code KbmTarget can hold either came from this same table or from the
/// combo-recording dialog, which only ever produces codes recognized
/// here) rather than panicking.
fn key_name(code: u16) -> String {
    KEY_OPTIONS
        .iter()
        .find(|(_, c)| *c == code)
        .map(|(name, _)| name.to_string())
        .unwrap_or_else(|| format!("Key {code}"))
}

/// Human-readable description of a whole KbmTarget, used both for the
/// synthetic combo entry injected into a row's dropdown and (via
/// key_name) built up from individual key names for Combo targets.
fn describe_kbm_target(t: KbmTarget) -> String {
    match t {
        KbmTarget::None => "(none)".to_string(),
        KbmTarget::MouseLeft => "Mouse Left Click".to_string(),
        KbmTarget::MouseRight => "Mouse Right Click".to_string(),
        KbmTarget::Key(k) => key_name(k),
        KbmTarget::Combo(keys) => {
            let names: Vec<String> = keys.iter().flatten().map(|c| key_name(*c)).collect();
            if names.is_empty() {
                "(none)".to_string()
            } else {
                format!("Combo: {}", names.join(" + "))
            }
        }
    }
}

fn kbm_target_id(t: KbmTarget) -> String {
    match t {
        KbmTarget::None => "none".to_string(),
        KbmTarget::MouseLeft => "mouseleft".to_string(),
        KbmTarget::MouseRight => "mouseright".to_string(),
        KbmTarget::Key(k) => format!("key_{k}"),
        // 0 (KEY_RESERVED in the evdev ABI -- literally defined as
        // "unused") is a safe sentinel for an empty combo slot, since
        // it can never be a real captured key.
        KbmTarget::Combo(keys) => format!(
            "combo_{}_{}_{}_{}",
            keys[0].unwrap_or(0),
            keys[1].unwrap_or(0),
            keys[2].unwrap_or(0),
            keys[3].unwrap_or(0)
        ),
    }
}

fn kbm_target_from_id(id: &str) -> KbmTarget {
    if let Some(rest) = id.strip_prefix("combo_") {
        let parts: Vec<u16> = rest.split('_').filter_map(|s| s.parse::<u16>().ok()).collect();
        let mut slots: [Option<u16>; 4] = [None; 4];
        for (i, &v) in parts.iter().take(4).enumerate() {
            if v != 0 {
                slots[i] = Some(v);
            }
        }
        return KbmTarget::Combo(slots);
    }
    match id {
        "none" => KbmTarget::None,
        "mouseleft" => KbmTarget::MouseLeft,
        "mouseright" => KbmTarget::MouseRight,
        other => other
            .strip_prefix("key_")
            .and_then(|n| n.parse::<u16>().ok())
            .map(KbmTarget::Key)
            .unwrap_or(KbmTarget::None),
    }
}

/// Guesses the evdev key code from GTK's raw hardware keycode.
///
/// GTK's raw `keycode` differs by windowing backend: X11 delivers
/// evdev_code + 8 (X11 reserves keycodes 0-7), Wayland delivers the raw
/// evdev code directly -- confirmed via XKB's own evdev keycode
/// ruleset, which documents `minimum = 8` specifically to accommodate
/// that X11 reservation. Detected here via the XDG_SESSION_TYPE
/// environment variable rather than querying GDK's display backend
/// directly (simpler, avoids an extra gdk4-x11/gdk4-wayland dependency
/// this project doesn't otherwise need) -- a heuristic, not a certainty
/// for every setup (XWayland edge cases exist), which is exactly why
/// show_combo_record_dialog displays each captured key's NAME live: a
/// wrong guess here is immediately visible as the wrong key name
/// appearing the moment you press a key, not a silently wrong binding
/// discovered later.
fn gdk_keycode_to_evdev(raw_keycode: u32) -> u16 {
    let is_x11 = std::env::var("XDG_SESSION_TYPE")
        .map(|s| s.eq_ignore_ascii_case("x11"))
        .unwrap_or(false);
    if is_x11 {
        raw_keycode.saturating_sub(8) as u16
    } else {
        raw_keycode as u16
    }
}

/// Opens a dialog that captures a key combination from the person's
/// PHYSICAL keyboard (not a dropdown) via GTK4's raw key-press/release
/// events. Calls `on_confirm([Option<u16>; 4])` with up to the first 4
/// distinct keys that were EVER simultaneously held during one
/// continuous press-then-fully-release session, then closes itself.
///
/// UNVERIFIED in the way most of this GUI layer is -- written without
/// the ability to compile against the actual gtk4 crate here. The
/// EventControllerKey API shape (connect_key_pressed/released
/// signatures, Propagation return type, Widget::root()) is based on
/// gtk4-rs's documented pattern but not confirmed against the exact
/// resolved crate version. Build and test this specifically: press a
/// single known key (say, W) alone first and confirm the live label
/// shows "W", not some other key -- that one check validates the
/// X11/Wayland keycode-offset heuristic above for your actual setup
/// before trusting anything more complex like a full combo.
fn show_combo_record_dialog(parent: &gtk4::ApplicationWindow, on_confirm: impl Fn([Option<u16>; 4]) + 'static) {
    let dialog = gtk4::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Record Key Combination")
        .default_width(360)
        .build();

    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    b.set_margin_top(12);
    b.set_margin_bottom(12);
    b.set_margin_start(12);
    b.set_margin_end(12);

    let instructions = gtk4::Label::new(Some(
        "Press and hold your key combination on your physical keyboard, then release all keys. \
         Up to 4 simultaneous keys are captured.",
    ));
    instructions.set_wrap(true);
    instructions.set_xalign(0.0);
    b.append(&instructions);

    let captured_label = gtk4::Label::new(Some("(waiting for keys...)"));
    captured_label.set_xalign(0.0);
    captured_label.add_css_class("heading");
    b.append(&captured_label);

    let cancel_button = gtk4::Button::with_label("Cancel");
    b.append(&cancel_button);

    dialog.set_child(Some(&b));

    // `currently_held` tracks what's down RIGHT NOW (to detect "fully
    // released" = recording done); `ever_held` is the ordered,
    // deduplicated record of everything that was EVER held during this
    // one session, which is the actual combo -- needed because releasing
    // keys in a different order than they were pressed (e.g. letting go
    // of Esc before Ctrl in a Ctrl+Shift+Esc combo) would otherwise lose
    // keys if only "what's held at the final release" were captured.
    let currently_held: Rc<RefCell<std::collections::HashSet<u16>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));
    let ever_held: Rc<RefCell<Vec<u16>>> = Rc::new(RefCell::new(Vec::new()));

    let controller = gtk4::EventControllerKey::new();

    {
        let currently_held = currently_held.clone();
        let ever_held = ever_held.clone();
        let captured_label = captured_label.clone();
        controller.connect_key_pressed(move |_controller, _keyval, keycode, _state| {
            let code = gdk_keycode_to_evdev(keycode);
            currently_held.borrow_mut().insert(code);
            {
                let mut ever = ever_held.borrow_mut();
                if !ever.contains(&code) {
                    ever.push(code);
                }
            }
            let names: Vec<String> = ever_held.borrow().iter().map(|c| key_name(*c)).collect();
            captured_label.set_text(&names.join(" + "));
            // Stop propagation so a key used in the combo -- including
            // Esc, which GTK dialogs often bind to "close" by default --
            // doesn't ALSO trigger that default behavior while recording.
            gtk4::glib::Propagation::Stop
        });
    }

    {
        let dialog = dialog.clone();
        controller.connect_key_released(move |_controller, _keyval, keycode, _state| {
            let code = gdk_keycode_to_evdev(keycode);
            currently_held.borrow_mut().remove(&code);
            let done = currently_held.borrow().is_empty() && !ever_held.borrow().is_empty();
            if done {
                let captured = ever_held.borrow();
                let mut slots: [Option<u16>; 4] = [None; 4];
                for (i, code) in captured.iter().take(4).enumerate() {
                    slots[i] = Some(*code);
                }
                on_confirm(slots);
                dialog.close();
            }
        });
    }

    dialog.add_controller(controller);

    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }

    dialog.present();
}

/// Builds one KbmTarget row: a dropdown for simple targets (None/Mouse/
/// single key) plus a "Record Combo..." button for multi-key targets.
/// `setter` takes the whole Profile (via profile_rc) rather than just a
/// KbmTarget so each call site can point at whichever KbmConfig field
/// it's editing without this helper needing 18 near-identical variants.
fn kbm_target_combo(
    current: KbmTarget,
    profile_rc: &Rc<RefCell<Profile>>,
    setter: fn(&mut KbmConfig, KbmTarget),
) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);

    let combo = gtk4::ComboBoxText::new();
    combo.append(Some("none"), "(none)");
    combo.append(Some("mouseleft"), "Mouse Left Click");
    combo.append(Some("mouseright"), "Mouse Right Click");
    for (label, code) in KEY_OPTIONS {
        combo.append(Some(&format!("key_{code}")), label);
    }
    // If the current target is already a Combo, inject a synthetic
    // entry representing it -- rebuilt fresh here (and again whenever a
    // NEW combo is recorded for this row, below) rather than trying to
    // update an existing entry's text in place, which ComboBoxText
    // doesn't support.
    if matches!(current, KbmTarget::Combo(_)) {
        combo.append(Some(&kbm_target_id(current)), &describe_kbm_target(current));
    }
    combo.set_active_id(Some(&kbm_target_id(current)));

    {
        let profile_rc = profile_rc.clone();
        combo.connect_changed(move |c| {
            if let Some(id) = c.active_id() {
                setter(&mut profile_rc.borrow_mut().kbm, kbm_target_from_id(&id));
            }
        });
    }
    row.append(&combo);

    let record_button = gtk4::Button::with_label("Record Combo...");
    {
        let profile_rc = profile_rc.clone();
        let combo = combo.clone();
        record_button.connect_clicked(move |button| {
            // Widget::root() walks up to the toplevel window this
            // button lives in -- used instead of threading a `window`
            // reference all the way through populate_form/
            // build_kbm_tab/kbm_target_combo's call chain, several of
            // which are also invoked from contexts (profile-switch
            // handlers, etc.) that don't currently carry one.
            let Some(window) = button
                .root()
                .and_then(|r| r.downcast::<gtk4::ApplicationWindow>().ok())
            else {
                return;
            };
            let profile_rc = profile_rc.clone();
            let combo = combo.clone();
            show_combo_record_dialog(&window, move |slots| {
                let target = KbmTarget::Combo(slots);
                setter(&mut profile_rc.borrow_mut().kbm, target);
                let id = kbm_target_id(target);
                combo.append(Some(&id), &describe_kbm_target(target));
                combo.set_active_id(Some(&id));
            });
        });
    }
    row.append(&record_button);

    row
}

fn build_kbm_tab(profile_rc: &Rc<RefCell<Profile>>) -> gtk4::Box {
    let b = section_box();
    let cfg: KbmConfig = profile_rc.borrow().kbm.clone();

    let scroller = gtk4::ScrolledWindow::new();
    let grid = gtk4::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(10);
    scroller.set_child(Some(&grid));
    scroller.set_min_content_height(360);

    // Macro-free but repetitive-by-design table of (label, current
    // value, setter fn pointer) -- explicit over clever, matching this
    // project's general style, and avoids proc-macro/declarative-macro
    // machinery for what's fundamentally a fixed 18-row list.
    let rows: Vec<(&str, KbmTarget, fn(&mut KbmConfig, KbmTarget))> = vec![
        ("Cross (X)", cfg.cross, |c, v| c.cross = v),
        ("Circle (O)", cfg.circle, |c, v| c.circle = v),
        ("Triangle", cfg.triangle, |c, v| c.triangle = v),
        ("Square", cfg.square, |c, v| c.square = v),
        ("L1", cfg.l1, |c, v| c.l1 = v),
        ("R1", cfg.r1, |c, v| c.r1 = v),
        ("L2", cfg.l2, |c, v| c.l2 = v),
        ("R2", cfg.r2, |c, v| c.r2 = v),
        ("L3 (stick click)", cfg.l3, |c, v| c.l3 = v),
        ("R3 (stick click)", cfg.r3, |c, v| c.r3 = v),
        ("Share", cfg.share, |c, v| c.share = v),
        ("Options", cfg.options, |c, v| c.options = v),
        ("PS button", cfg.ps, |c, v| c.ps = v),
        ("Touchpad click", cfg.touchpad_click, |c, v| c.touchpad_click = v),
        ("D-pad Up", cfg.dpad_up, |c, v| c.dpad_up = v),
        ("D-pad Down", cfg.dpad_down, |c, v| c.dpad_down = v),
        ("D-pad Left", cfg.dpad_left, |c, v| c.dpad_left = v),
        ("D-pad Right", cfg.dpad_right, |c, v| c.dpad_right = v),
    ];

    for (row_index, (label, current, setter)) in rows.into_iter().enumerate() {
        let lbl = gtk4::Label::new(Some(label));
        lbl.set_xalign(0.0);
        let combo = kbm_target_combo(current, profile_rc, setter);
        grid.attach(&lbl, 0, row_index as i32, 1, 1);
        grid.attach(&combo, 1, row_index as i32, 1, 1);
    }

    b.append(&scroller);
    b.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    b.append(&build_stick_kbm_section("Left stick", &cfg.left_stick, profile_rc, true));
    b.append(&build_stick_kbm_section("Right stick", &cfg.right_stick, profile_rc, false));

    let threshold = spin(0.0, 1.0, 0.05, cfg.trigger_threshold);
    {
        let profile_rc = profile_rc.clone();
        threshold.connect_value_changed(move |s| {
            profile_rc.borrow_mut().kbm.trigger_threshold = s.value();
        });
    }
    b.append(&labeled_row("L2/R2 press threshold", &threshold));

    b
}

/// Builds one stick's KBM sub-form: a Mouse-vs-Digital mode toggle, then
/// either a sensitivity spin (Mouse) or four direction dropdowns plus a
/// threshold spin (Digital). Rebuilds its own inner content when the
/// mode toggle flips, same "rebuild rather than reconcile" approach as
/// populate_form uses for the whole window.
fn build_stick_kbm_section(
    title: &str,
    current: &StickKbmMode,
    profile_rc: &Rc<RefCell<Profile>>,
    is_left: bool,
) -> gtk4::Box {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    let title_label = gtk4::Label::new(Some(title));
    title_label.set_xalign(0.0);
    title_label.add_css_class("heading");
    outer.append(&title_label);

    let mode_combo = gtk4::ComboBoxText::new();
    mode_combo.append(Some("mouse"), "Mouse movement");
    mode_combo.append(Some("digital"), "Digital directions (WASD-style)");
    mode_combo.set_active_id(Some(match current {
        StickKbmMode::Mouse { .. } => "mouse",
        StickKbmMode::Digital { .. } => "digital",
    }));
    outer.append(&labeled_row("Mode", &mode_combo));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    outer.append(&inner);

    fill_stick_inner(&inner, current, profile_rc, is_left);

    {
        let profile_rc = profile_rc.clone();
        let inner = inner.clone();
        mode_combo.connect_changed(move |c| {
            let new_mode = match c.active_id().as_deref() {
                Some("digital") => StickKbmMode::Digital {
                    up: KbmTarget::None,
                    down: KbmTarget::None,
                    left: KbmTarget::None,
                    right: KbmTarget::None,
                    threshold: 0.5,
                },
                _ => StickKbmMode::Mouse { sensitivity: 8.0 },
            };
            {
                let mut p = profile_rc.borrow_mut();
                if is_left {
                    p.kbm.left_stick = new_mode.clone();
                } else {
                    p.kbm.right_stick = new_mode.clone();
                }
            }
            while let Some(child) = inner.first_child() {
                inner.remove(&child);
            }
            fill_stick_inner(&inner, &new_mode, &profile_rc, is_left);
        });
    }

    outer
}

fn fill_stick_inner(
    inner: &gtk4::Box,
    mode: &StickKbmMode,
    profile_rc: &Rc<RefCell<Profile>>,
    is_left: bool,
) {
    match mode {
        StickKbmMode::Mouse { sensitivity } => {
            let sens = spin(0.5, 30.0, 0.5, *sensitivity);
            let profile_rc = profile_rc.clone();
            sens.connect_value_changed(move |s| {
                let mut p = profile_rc.borrow_mut();
                let target = if is_left { &mut p.kbm.left_stick } else { &mut p.kbm.right_stick };
                if let StickKbmMode::Mouse { sensitivity } = target {
                    *sensitivity = s.value();
                }
            });
            inner.append(&labeled_row("Sensitivity", &sens));
        }
        StickKbmMode::Digital { up, down, left, right, threshold } => {
            let up_combo = stick_digital_combo(*up, profile_rc, is_left, StickField::Up);
            let down_combo = stick_digital_combo(*down, profile_rc, is_left, StickField::Down);
            let left_combo = stick_digital_combo(*left, profile_rc, is_left, StickField::Left);
            let right_combo = stick_digital_combo(*right, profile_rc, is_left, StickField::Right);

            inner.append(&labeled_row("Up", &up_combo));
            inner.append(&labeled_row("Down", &down_combo));
            inner.append(&labeled_row("Left", &left_combo));
            inner.append(&labeled_row("Right", &right_combo));

            let thresh = spin(0.05, 0.95, 0.05, *threshold);
            {
                let profile_rc = profile_rc.clone();
                thresh.connect_value_changed(move |s| {
                    let mut p = profile_rc.borrow_mut();
                    let target = if is_left { &mut p.kbm.left_stick } else { &mut p.kbm.right_stick };
                    if let StickKbmMode::Digital { threshold, .. } = target {
                        *threshold = s.value();
                    }
                });
            }
            inner.append(&labeled_row("Threshold", &thresh));
        }
    }
}

#[derive(Clone, Copy)]
enum StickField {
    Up,
    Down,
    Left,
    Right,
}

/// One dropdown for one direction of one stick's Digital mode.
/// Deliberately its own function (rather than reusing `kbm_target_combo`,
/// which only knows how to reach top-level KbmConfig fields) since
/// reaching into `left_stick`/`right_stick`'s `Digital` variant needs
/// both which stick (`is_left`) and which direction (`field`) baked
/// into the closure -- `kbm_target_combo`'s `fn(&mut KbmConfig, ...)`
/// setter shape can't express that.
fn stick_digital_combo(
    current: KbmTarget,
    profile_rc: &Rc<RefCell<Profile>>,
    is_left: bool,
    field: StickField,
) -> gtk4::ComboBoxText {
    let combo = gtk4::ComboBoxText::new();
    combo.append(Some("none"), "(none)");
    combo.append(Some("mouseleft"), "Mouse Left Click");
    combo.append(Some("mouseright"), "Mouse Right Click");
    for (label, code) in KEY_OPTIONS {
        combo.append(Some(&format!("key_{code}")), label);
    }
    combo.set_active_id(Some(&kbm_target_id(current)));

    let profile_rc = profile_rc.clone();
    combo.connect_changed(move |c| {
        let Some(id) = c.active_id() else { return };
        let v = kbm_target_from_id(&id);
        let mut p = profile_rc.borrow_mut();
        let target = if is_left { &mut p.kbm.left_stick } else { &mut p.kbm.right_stick };
        if let StickKbmMode::Digital { up, down, left, right, .. } = target {
            match field {
                StickField::Up => *up = v,
                StickField::Down => *down = v,
                StickField::Left => *left = v,
                StickField::Right => *right = v,
            }
        }
    });
    combo
}

// ---- Gamepad Remap tab (Gamepad/Xbox360 output modes) ----

/// (dropdown id, display label, GamepadTarget) option table shared by
/// every remap dropdown in this tab -- the 17 button/dpad rows AND both
/// sticks' digital-breakdown rows all pick from this exact same set,
/// since gamepad_remap.rs's GamepadTarget means the same thing
/// regardless of which physical input is being configured.
fn gamepad_target_options() -> Vec<(String, String, GamepadTarget)> {
    use GamepadButton::*;
    let mut options = vec![("none".to_string(), "(none)".to_string(), GamepadTarget::None)];

    for (id, label, button) in [
        ("cross", "Cross (X)", Cross),
        ("circle", "Circle (O)", Circle),
        ("square", "Square", Square),
        ("triangle", "Triangle", Triangle),
        ("l1", "L1", L1),
        ("r1", "R1", R1),
        ("l2_digital", "L2 (digital click)", L2Digital),
        ("r2_digital", "R2 (digital click)", R2Digital),
        ("l3", "L3 (stick click)", L3),
        ("r3", "R3 (stick click)", R3),
        ("share", "Share", Share),
        ("options", "Options", Options),
        ("ps", "PS button", Ps),
        ("touchpad_click", "Touchpad click", TouchpadClick),
        ("dpad_up", "D-pad Up", DpadUp),
        ("dpad_down", "D-pad Down", DpadDown),
        ("dpad_left", "D-pad Left", DpadLeft),
        ("dpad_right", "D-pad Right", DpadRight),
    ] {
        options.push((format!("button_{id}"), format!("Button: {label}"), GamepadTarget::Button(button)));
    }

    for (stick_id, stick_label, stick) in [("left", "Left Stick", OutputStick::Left), ("right", "Right Stick", OutputStick::Right)] {
        for (dir_id, dir_label, dir) in [
            ("up", "Up", AxisDirection::Up),
            ("down", "Down", AxisDirection::Down),
            ("left", "Left", AxisDirection::Left),
            ("right", "Right", AxisDirection::Right),
        ] {
            options.push((
                format!("stickpush_{stick_id}_{dir_id}"),
                format!("Push: {stick_label} {dir_label}"),
                GamepadTarget::StickPush(stick, dir),
            ));
        }
    }

    for (id, label, trigger) in [("l2", "L2", OutputTrigger::L2), ("r2", "R2", OutputTrigger::R2)] {
        options.push((
            format!("triggerpush_{id}"),
            format!("Push: {label} (full)"),
            GamepadTarget::TriggerPush(trigger),
        ));
    }

    options
}

fn gamepad_target_id(target: GamepadTarget) -> String {
    use GamepadButton::*;
    match target {
        GamepadTarget::None => "none".to_string(),
        GamepadTarget::Button(b) => format!(
            "button_{}",
            match b {
                Cross => "cross",
                Circle => "circle",
                Square => "square",
                Triangle => "triangle",
                L1 => "l1",
                R1 => "r1",
                L2Digital => "l2_digital",
                R2Digital => "r2_digital",
                L3 => "l3",
                R3 => "r3",
                Share => "share",
                Options => "options",
                Ps => "ps",
                TouchpadClick => "touchpad_click",
                DpadUp => "dpad_up",
                DpadDown => "dpad_down",
                DpadLeft => "dpad_left",
                DpadRight => "dpad_right",
            }
        ),
        GamepadTarget::StickPush(stick, dir) => format!(
            "stickpush_{}_{}",
            match stick {
                OutputStick::Left => "left",
                OutputStick::Right => "right",
            },
            match dir {
                AxisDirection::Up => "up",
                AxisDirection::Down => "down",
                AxisDirection::Left => "left",
                AxisDirection::Right => "right",
            }
        ),
        GamepadTarget::TriggerPush(trigger) => format!(
            "triggerpush_{}",
            match trigger {
                OutputTrigger::L2 => "l2",
                OutputTrigger::R2 => "r2",
            }
        ),
    }
}

fn gamepad_target_from_id(id: &str, options: &[(String, String, GamepadTarget)]) -> GamepadTarget {
    options
        .iter()
        .find(|(option_id, _, _)| option_id == id)
        .map(|(_, _, target)| *target)
        .unwrap_or(GamepadTarget::None)
}

/// One GamepadTarget dropdown, wired to call `setter` on change. Same
/// "setter takes the whole Profile" pattern as kbm_target_combo, for
/// the same reason: avoids needing one near-identical closure per field.
fn gamepad_target_combo(
    current: GamepadTarget,
    profile_rc: &Rc<RefCell<Profile>>,
    setter: fn(&mut GamepadRemapConfig, GamepadTarget),
) -> gtk4::ComboBoxText {
    let options = gamepad_target_options();
    let combo = gtk4::ComboBoxText::new();
    for (id, label, _) in &options {
        combo.append(Some(id), label);
    }
    combo.set_active_id(Some(&gamepad_target_id(current)));

    let profile_rc = profile_rc.clone();
    combo.connect_changed(move |c| {
        if let Some(id) = c.active_id() {
            setter(&mut profile_rc.borrow_mut().gamepad_remap, gamepad_target_from_id(&id, &options));
        }
    });
    combo
}

fn build_gamepad_remap_tab(profile_rc: &Rc<RefCell<Profile>>) -> gtk4::Box {
    let b = section_box();

    let note = gtk4::Label::new(Some(
        "Only used in Gamepad and Xbox 360 output modes. Defaults reconstruct normal 1:1 \
         behavior exactly -- nothing here changes anything until you edit it.",
    ));
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.add_css_class("dim-label");
    b.append(&note);

    let cfg: GamepadRemapConfig = profile_rc.borrow().gamepad_remap.clone();

    let scroller = gtk4::ScrolledWindow::new();
    let grid = gtk4::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(10);
    scroller.set_child(Some(&grid));
    scroller.set_min_content_height(460);

    let rows: Vec<(&str, GamepadTarget, fn(&mut GamepadRemapConfig, GamepadTarget))> = vec![
        ("Cross (X)", cfg.cross, |c, v| c.cross = v),
        ("Circle (O)", cfg.circle, |c, v| c.circle = v),
        ("Triangle", cfg.triangle, |c, v| c.triangle = v),
        ("Square", cfg.square, |c, v| c.square = v),
        ("L1", cfg.l1, |c, v| c.l1 = v),
        ("R1", cfg.r1, |c, v| c.r1 = v),
        ("L2 (digital click)", cfg.l2_digital, |c, v| c.l2_digital = v),
        ("R2 (digital click)", cfg.r2_digital, |c, v| c.r2_digital = v),
        ("L3 (stick click)", cfg.l3, |c, v| c.l3 = v),
        ("R3 (stick click)", cfg.r3, |c, v| c.r3 = v),
        ("Share", cfg.share, |c, v| c.share = v),
        ("Options", cfg.options, |c, v| c.options = v),
        ("PS button", cfg.ps, |c, v| c.ps = v),
        ("Touchpad click", cfg.touchpad_click, |c, v| c.touchpad_click = v),
        ("D-pad Up", cfg.dpad_up, |c, v| c.dpad_up = v),
        ("D-pad Down", cfg.dpad_down, |c, v| c.dpad_down = v),
        ("D-pad Left", cfg.dpad_left, |c, v| c.dpad_left = v),
        ("D-pad Right", cfg.dpad_right, |c, v| c.dpad_right = v),
    ];

    for (row_index, (label, current, setter)) in rows.into_iter().enumerate() {
        let lbl = gtk4::Label::new(Some(label));
        lbl.set_xalign(0.0);
        let combo = gamepad_target_combo(current, profile_rc, setter);
        grid.attach(&lbl, 0, row_index as i32, 1, 1);
        grid.attach(&combo, 1, row_index as i32, 1, 1);
    }

    b.append(&scroller);
    b.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    b.append(&build_stick_remap_section("Left stick", &cfg, true, profile_rc));
    b.append(&build_stick_remap_section("Right stick", &cfg, false, profile_rc));
    b.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    b.append(&build_trigger_remap_section("L2", &cfg, true, profile_rc));
    b.append(&build_trigger_remap_section("R2", &cfg, false, profile_rc));

    b
}

/// One stick's analog source dropdown plus its digital-breakdown
/// sub-form -- both independent fields (see gamepad_remap.rs's module
/// doc on why both can be active at once).
fn build_stick_remap_section(
    title: &str,
    cfg: &GamepadRemapConfig,
    is_left: bool,
    profile_rc: &Rc<RefCell<Profile>>,
) -> gtk4::Box {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    let title_label = gtk4::Label::new(Some(title));
    title_label.set_xalign(0.0);
    title_label.add_css_class("heading");
    outer.append(&title_label);

    let analog_combo = gtk4::ComboBoxText::new();
    analog_combo.append(Some("none"), "(disabled -- stays centered)");
    analog_combo.append(Some("left"), "Physical Left Stick");
    analog_combo.append(Some("right"), "Physical Right Stick");
    let current_analog = if is_left { cfg.left_stick_analog } else { cfg.right_stick_analog };
    analog_combo.set_active_id(Some(match current_analog {
        StickAnalogSource::None => "none",
        StickAnalogSource::Left => "left",
        StickAnalogSource::Right => "right",
    }));
    {
        let profile_rc = profile_rc.clone();
        analog_combo.connect_changed(move |c| {
            let source = match c.active_id().as_deref() {
                Some("left") => StickAnalogSource::Left,
                Some("right") => StickAnalogSource::Right,
                _ => StickAnalogSource::None,
            };
            let mut p = profile_rc.borrow_mut();
            if is_left {
                p.gamepad_remap.left_stick_analog = source;
            } else {
                p.gamepad_remap.right_stick_analog = source;
            }
        });
    }
    outer.append(&labeled_row("Analog source", &analog_combo));

    let digital: StickDigitalConfig = if is_left { cfg.left_stick_digital } else { cfg.right_stick_digital };

    let expander = gtk4::Expander::new(Some("Also treat as 4 digital directions"));
    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    inner.set_margin_top(6);
    inner.set_margin_start(12);

    let up_combo = gamepad_stick_digital_combo(digital.up, profile_rc, is_left, StickDigitalField::Up);
    let down_combo = gamepad_stick_digital_combo(digital.down, profile_rc, is_left, StickDigitalField::Down);
    let left_combo = gamepad_stick_digital_combo(digital.left, profile_rc, is_left, StickDigitalField::Left);
    let right_combo = gamepad_stick_digital_combo(digital.right, profile_rc, is_left, StickDigitalField::Right);
    inner.append(&labeled_row("Up", &up_combo));
    inner.append(&labeled_row("Down", &down_combo));
    inner.append(&labeled_row("Left", &left_combo));
    inner.append(&labeled_row("Right", &right_combo));

    let threshold = spin(0.05, 0.95, 0.05, digital.threshold);
    {
        let profile_rc = profile_rc.clone();
        threshold.connect_value_changed(move |s| {
            let mut p = profile_rc.borrow_mut();
            let target = if is_left {
                &mut p.gamepad_remap.left_stick_digital
            } else {
                &mut p.gamepad_remap.right_stick_digital
            };
            target.threshold = s.value();
        });
    }
    inner.append(&labeled_row("Threshold", &threshold));

    expander.set_child(Some(&inner));
    outer.append(&expander);

    outer
}

#[derive(Clone, Copy)]
enum StickDigitalField {
    Up,
    Down,
    Left,
    Right,
}

fn gamepad_stick_digital_combo(
    current: GamepadTarget,
    profile_rc: &Rc<RefCell<Profile>>,
    is_left: bool,
    field: StickDigitalField,
) -> gtk4::ComboBoxText {
    let options = gamepad_target_options();
    let combo = gtk4::ComboBoxText::new();
    for (id, label, _) in &options {
        combo.append(Some(id), label);
    }
    combo.set_active_id(Some(&gamepad_target_id(current)));

    let profile_rc = profile_rc.clone();
    combo.connect_changed(move |c| {
        let Some(id) = c.active_id() else { return };
        let target = gamepad_target_from_id(&id, &options);
        let mut p = profile_rc.borrow_mut();
        let digital = if is_left {
            &mut p.gamepad_remap.left_stick_digital
        } else {
            &mut p.gamepad_remap.right_stick_digital
        };
        match field {
            StickDigitalField::Up => digital.up = target,
            StickDigitalField::Down => digital.down = target,
            StickDigitalField::Left => digital.left = target,
            StickDigitalField::Right => digital.right = target,
        }
    });
    combo
}

fn build_trigger_remap_section(
    title: &str,
    cfg: &GamepadRemapConfig,
    is_l2: bool,
    profile_rc: &Rc<RefCell<Profile>>,
) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let lbl = gtk4::Label::new(Some(&format!("{title} analog source")));
    lbl.set_width_chars(20);
    lbl.set_xalign(0.0);
    row.append(&lbl);

    let combo = gtk4::ComboBoxText::new();
    combo.append(Some("none"), "(disabled -- stays at 0)");
    combo.append(Some("l2"), "Physical L2");
    combo.append(Some("r2"), "Physical R2");
    let current = if is_l2 { cfg.l2_analog_target } else { cfg.r2_analog_target };
    combo.set_active_id(Some(match current {
        TriggerAnalogSource::None => "none",
        TriggerAnalogSource::L2 => "l2",
        TriggerAnalogSource::R2 => "r2",
    }));
    {
        let profile_rc = profile_rc.clone();
        combo.connect_changed(move |c| {
            let source = match c.active_id().as_deref() {
                Some("l2") => TriggerAnalogSource::L2,
                Some("r2") => TriggerAnalogSource::R2,
                _ => TriggerAnalogSource::None,
            };
            let mut p = profile_rc.borrow_mut();
            if is_l2 {
                p.gamepad_remap.l2_analog_target = source;
            } else {
                p.gamepad_remap.r2_analog_target = source;
            }
        });
    }
    row.append(&combo);
    row
}


fn show_text_input_dialog(
    parent: &gtk4::ApplicationWindow,
    title: &str,
    placeholder: &str,
    prefill: Option<&str>,
    on_confirm: impl Fn(String) + 'static,
) {
    let dialog = gtk4::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .default_width(320)
        .build();

    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    b.set_margin_top(10);
    b.set_margin_bottom(10);
    b.set_margin_start(10);
    b.set_margin_end(10);

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some(placeholder));
    if let Some(text) = prefill {
        entry.set_text(text);
        // Select-all rather than leaving the cursor at the end -- for a
        // rename/duplicate prefill, the person most likely wants to
        // type a whole new name over the suggestion, not tack
        // characters onto it.
        entry.select_region(0, -1);
    }
    b.append(&entry);

    let confirm_button = gtk4::Button::with_label("OK");
    confirm_button.set_sensitive(prefill.map(|s| !s.trim().is_empty()).unwrap_or(false));
    b.append(&confirm_button);

    dialog.set_child(Some(&b));

    {
        let confirm_button = confirm_button.clone();
        entry.connect_changed(move |e| {
            confirm_button.set_sensitive(!e.text().trim().is_empty());
        });
    }

    // Wrapped in Rc<dyn Fn> so the confirm logic below (which needs to
    // be wired to BOTH the button's click and the entry's Enter-key
    // activate signal) can be cloned regardless of whether the caller's
    // `on_confirm` closure itself happens to be Clone -- an opaque
    // `impl Fn(String) + 'static` isn't guaranteed to be, even though
    // every actual call site in this file only captures Clone types
    // (Rc/gtk4 widgets) and would satisfy it in practice. Rc<dyn Fn>
    // sidesteps needing to rely on that at all.
    let on_confirm: Rc<dyn Fn(String)> = Rc::new(on_confirm);

    let confirm = {
        let dialog = dialog.clone();
        let entry = entry.clone();
        let on_confirm = on_confirm.clone();
        move || {
            let name = entry.text().to_string();
            let name = name.trim();
            if !name.is_empty() {
                on_confirm(name.to_string());
                dialog.close();
            }
        }
    };

    {
        let confirm = confirm.clone();
        confirm_button.connect_clicked(move |_| confirm());
    }
    {
        let confirm = confirm.clone();
        // Enter in the entry field confirms too, not just clicking OK --
        // small thing, but typing a name and reaching for the mouse
        // every time would be annoying for something used this often.
        entry.connect_activate(move |_| confirm());
    }

    dialog.present();
}

/// Confirmation dialog for destructive actions (currently just Delete).
/// `confirm_label` lets the caller phrase the affirmative button for
/// the specific action ("Delete" here) rather than a generic "OK",
/// since a generic label on a destructive confirm is an easy
/// misclick waiting to happen.
fn show_confirm_dialog(
    parent: &gtk4::ApplicationWindow,
    title: &str,
    message: &str,
    confirm_label: &str,
    on_confirm: impl Fn() + 'static,
) {
    let dialog = gtk4::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .default_width(360)
        .build();

    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    b.set_margin_top(10);
    b.set_margin_bottom(10);
    b.set_margin_start(10);
    b.set_margin_end(10);

    let message_label = gtk4::Label::new(Some(message));
    message_label.set_wrap(true);
    message_label.set_xalign(0.0);
    b.append(&message_label);

    let button_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let cancel_button = gtk4::Button::with_label("Cancel");
    let confirm_button = gtk4::Button::with_label(confirm_label);
    confirm_button.add_css_class("destructive-action");
    button_row.append(&cancel_button);
    button_row.append(&confirm_button);
    b.append(&button_row);

    dialog.set_child(Some(&b));

    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }
    {
        let dialog = dialog.clone();
        confirm_button.connect_clicked(move |_| {
            on_confirm();
            dialog.close();
        });
    }

    dialog.present();
}

