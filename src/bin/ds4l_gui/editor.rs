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
//!   - No delete confirmation dialog -- "Delete" removes the profile's
//!     TOML file immediately. Fine for a first pass; a confirm dialog
//!     is an easy, low-risk follow-up.
//!   - No live validation feedback beyond what GTK's SpinButton ranges
//!     already enforce (e.g. RGB spins clamped 0-255 by their adjustment,
//!     so out-of-range values are structurally impossible rather than
//!     caught after the fact).

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
        .default_height(640)
        .build();
    window.set_hide_on_close(true);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    root.set_margin_top(10);
    root.set_margin_bottom(10);
    root.set_margin_start(10);
    root.set_margin_end(10);

    // -- Top bar: profile picker + new/delete --
    let top_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let profile_combo = gtk4::ComboBoxText::new();
    let new_button = gtk4::Button::with_label("New...");
    let delete_button = gtk4::Button::with_label("Delete");
    top_bar.append(&profile_combo);
    top_bar.append(&new_button);
    top_bar.append(&delete_button);
    root.append(&top_bar);

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

    // -- Bottom bar: save actions + status line --
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

    // Shared editing state: the profile currently loaded into the form.
    // Starts as a fresh Default profile named "Default" and gets
    // replaced wholesale whenever the profile_combo selection changes
    // or "New..." creates one.
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
            show_new_profile_dialog(&window_for_dialog, {
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
        let status_label = status_label.clone();
        delete_button.connect_clicked(move |_| {
            let Some(name) = profile_combo.active_text() else { return };
            match profile::profiles_dir() {
                Ok(dir) => {
                    let path = dir.join(format!("{name}.toml"));
                    match std::fs::remove_file(&path) {
                        Ok(()) => {
                            status_label.set_text(&format!("Deleted \"{name}\"."));
                            refresh_profile_list(&profile_combo);
                        }
                        Err(e) => {
                            status_label.set_text(&format!("Failed to delete \"{name}\": {e}"));
                        }
                    }
                }
                Err(e) => status_label.set_text(&format!("{e}")),
            }
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
        save_apply_button.connect_clicked(move |_| {
            let name = profile_rc.borrow().name.clone();
            match profile::save(&profile_rc.borrow()) {
                Ok(()) => match ds4l::ipc::switch_profile(&name) {
                    Ok(()) => {
                        status_label.set_text(&format!("Saved and applied \"{name}\" live."));
                    }
                    Err(e) => {
                        status_label.set_text(&format!(
                            "Saved \"{name}\", but could not apply live (is ds4l_daemon \
                             running?): {e}"
                        ));
                    }
                },
                Err(e) => status_label.set_text(&format!("Save failed: {e}")),
            }
        });
    }

    window
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
    mode_combo.append(Some("gamepad"), "Gamepad");
    mode_combo.append(Some("kbm"), "Keyboard + Mouse");
    mode_combo.set_active_id(Some(match profile_rc.borrow().output_mode {
        OutputMode::Gamepad => "gamepad",
        OutputMode::Kbm => "kbm",
    }));
    {
        let profile_rc = profile_rc.clone();
        mode_combo.connect_changed(move |c| {
            profile_rc.borrow_mut().output_mode = match c.active_id().as_deref() {
                Some("kbm") => OutputMode::Kbm,
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
    ("l2", "L2", GateButton::L2),
    ("r2", "R2", GateButton::R2),
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
    mode_combo.set_active_id(Some(match cfg.mode {
        GyroMode::AlwaysOn => "always_on",
        GyroMode::Toggle => "toggle",
        GyroMode::Hold => "hold",
    }));
    {
        let profile_rc = profile_rc.clone();
        mode_combo.connect_changed(move |c| {
            profile_rc.borrow_mut().gyro.mode = match c.active_id().as_deref() {
                Some("always_on") => GyroMode::AlwaysOn,
                Some("toggle") => GyroMode::Toggle,
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

    b
}

// ---- Touchpad tab ----

fn build_touchpad_tab(profile_rc: &Rc<RefCell<Profile>>) -> gtk4::Box {
    let b = section_box();
    let cfg: TouchpadConfig = profile_rc.borrow().touchpad;

    let mode_combo = gtk4::ComboBoxText::new();
    mode_combo.append(Some("passthrough"), "Passthrough (native touchpad)");
    mode_combo.append(Some("mouse_remap"), "Mouse Remap");
    mode_combo.set_active_id(Some(match cfg.mode {
        TouchpadMode::Passthrough => "passthrough",
        TouchpadMode::MouseRemap => "mouse_remap",
    }));
    {
        let profile_rc = profile_rc.clone();
        mode_combo.connect_changed(move |c| {
            profile_rc.borrow_mut().touchpad.mode = match c.active_id().as_deref() {
                Some("mouse_remap") => TouchpadMode::MouseRemap,
                _ => TouchpadMode::Passthrough,
            };
        });
    }
    b.append(&labeled_row("Mode", &mode_combo));

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

/// (display label, evdev KEY_* code) for every key kbm.rs currently
/// exposes -- deliberately the same limited, verified-code-only set as
/// kbm.rs itself (see that module's doc comment on why it's WASD/common
/// keys rather than an exhaustive keyboard), not a superset invented
/// for this editor.
const KEY_OPTIONS: &[(&str, u16)] = &[
    ("Esc", kbm::KEY_ESC),
    ("1", kbm::KEY_1),
    ("Tab", kbm::KEY_TAB),
    ("Q", kbm::KEY_Q),
    ("W", kbm::KEY_W),
    ("E", kbm::KEY_E),
    ("R", kbm::KEY_R),
    ("T", kbm::KEY_T),
    ("Enter", kbm::KEY_ENTER),
    ("Left Ctrl", kbm::KEY_LEFTCTRL),
    ("A", kbm::KEY_A),
    ("S", kbm::KEY_S),
    ("D", kbm::KEY_D),
    ("F", kbm::KEY_F),
    ("G", kbm::KEY_G),
    ("Left Shift", kbm::KEY_LEFTSHIFT),
    ("Z", kbm::KEY_Z),
    ("X", kbm::KEY_X),
    ("C", kbm::KEY_C),
    ("V", kbm::KEY_V),
    ("B", kbm::KEY_B),
    ("Right Shift", kbm::KEY_RIGHTSHIFT),
    ("Left Alt", kbm::KEY_LEFTALT),
    ("Space", kbm::KEY_SPACE),
];

fn kbm_target_id(t: KbmTarget) -> String {
    match t {
        KbmTarget::None => "none".to_string(),
        KbmTarget::MouseLeft => "mouseleft".to_string(),
        KbmTarget::MouseRight => "mouseright".to_string(),
        KbmTarget::Key(k) => format!("key_{k}"),
    }
}

fn kbm_target_from_id(id: &str) -> KbmTarget {
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

/// Builds one KbmTarget dropdown, wired to call `setter` on change.
/// `setter` takes the whole Profile (via profile_rc) rather than just a
/// KbmTarget so each call site can point at whichever KbmConfig field
/// it's editing without this helper needing 20 near-identical variants.
fn kbm_target_combo(
    current: KbmTarget,
    profile_rc: &Rc<RefCell<Profile>>,
    setter: fn(&mut KbmConfig, KbmTarget),
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
        if let Some(id) = c.active_id() {
            setter(&mut profile_rc.borrow_mut().kbm, kbm_target_from_id(&id));
        }
    });
    combo
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

/// Small modal-ish window (not a true GTK Dialog, to avoid depending on
/// an exact gtk4::Dialog/gtk4::AlertDialog API shape I couldn't verify
/// here) asking for a new profile name, calling `on_confirm(name)` and
/// closing itself when the person clicks Create.
fn show_new_profile_dialog(parent: &gtk4::ApplicationWindow, on_confirm: impl Fn(String) + 'static) {
    let dialog = gtk4::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("New Profile")
        .default_width(320)
        .build();

    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    b.set_margin_top(10);
    b.set_margin_bottom(10);
    b.set_margin_start(10);
    b.set_margin_end(10);

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Profile name"));
    b.append(&entry);

    let create_button = gtk4::Button::with_label("Create");
    b.append(&create_button);

    dialog.set_child(Some(&b));

    {
        let dialog = dialog.clone();
        let entry = entry.clone();
        create_button.connect_clicked(move |_| {
            let name = entry.text().to_string();
            let name = name.trim();
            if !name.is_empty() {
                on_confirm(name.to_string());
                dialog.close();
            }
        });
    }

    dialog.present();
}
