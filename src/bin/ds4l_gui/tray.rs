//! System tray icon: shows daemon status, lets the user switch profiles
//! with one click, and has a menu item to raise the profile editor
//! window.
//!
//! UNVERIFIED -- no way to build/run this in the environment this was
//! written in (no network access to fetch the `ksni`/`gtk4` crates, and
//! likely no GTK4 dev headers or a D-Bus session either). Unlike every
//! other module in this project, which was checked against real
//! hardware (or, for ipc.rs, at least designed to be checked with
//! `socat` before trusting it) before being called done, this file has
//! only been read over for logical correctness, not compiled. Before
//! relying on it:
//!   1. `cargo build --release --features gui --bin ds4l_gui` and read
//!      every compiler error closely -- ksni's exact trait shape
//!      (`Tray::menu`'s signature, `MenuItem`/`StandardItem`/
//!      `RadioItem` field names, the `activate` closure's exact
//!      signature) can differ between minor versions, and I could not
//!      check any of that against the version Cargo actually resolves.
//!      `cargo doc --open -p ksni --features blocking` (or docs.rs for
//!      the resolved version) is the fastest way to reconcile any
//!      mismatch.
//!   2. Confirm a tray icon actually appears at all -- StatusNotifierItem
//!      support varies by desktop environment (GNOME needs an
//!      AppIndicator extension installed; KDE/Xfce support it natively).
//!      If nothing shows up, that's very likely your DE, not this code.
//!   3. Click through every menu item once against a real running
//!      ds4l_daemon before trusting it day-to-day.

use ksni::blocking::TrayMethods;
use ksni::menu::{RadioGroup, RadioItem, StandardItem};
use ksni::{Icon, MenuItem, Tray};
use std::sync::mpsc::Sender;

/// Sent from the tray's D-Bus event thread to the GTK main thread
/// whenever the person clicks "Edit Profiles..." -- the tray itself
/// can't touch GTK widgets directly (wrong thread), so it just asks
/// main.rs to marshal the request over via glib::MainContext::invoke.
pub enum TrayEvent {
    ShowEditor,
}

pub struct Ds4lTray {
    show_editor_tx: Sender<TrayEvent>,
}

impl Ds4lTray {
    pub fn new(show_editor_tx: Sender<TrayEvent>) -> Self {
        Ds4lTray { show_editor_tx }
    }
}

impl Tray for Ds4lTray {
    fn id(&self) -> String {
        "ds4l".into()
    }

    fn icon_name(&self) -> String {
        // Generic, always-available icon names rather than a custom
        // bundled icon file -- avoids needing an install step to place
        // an icon somewhere the theme/DE can find it. "input-gaming" is
        // the closest standard freedesktop icon-naming-spec name for a
        // game controller; falls back gracefully to whatever the theme
        // substitutes if it's missing, same as any other app requesting
        // an icon name the current theme doesn't have.
        if ds4l::ipc::ping() {
            "input-gaming".into()
        } else {
            // Distinct icon when the daemon isn't reachable, so a glance
            // at the tray answers "is it even running?" without opening
            // the menu.
            "dialog-warning".into()
        }
    }

    fn title(&self) -> String {
        "ds4l".into()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        // Queried fresh on every menu open (StatusNotifierItem clients
        // call AboutToShow before rendering) -- this is deliberately a
        // live, synchronous round-trip to the daemon's control socket
        // each time, not a cached snapshot, so the checked profile and
        // status line are never stale by more than "however long the
        // menu happened to be closed." ipc::status()/list_profiles()
        // are one-shot blocking calls over a local unix socket, so this
        // is fast (sub-millisecond typically) -- see ipc.rs.
        let mut items: Vec<MenuItem<Self>> = Vec::new();

        match ds4l::ipc::status() {
            Ok(status_line) => {
                items.push(
                    StandardItem {
                        label: format_status_line(&status_line),
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                );
            }
            Err(_) => {
                items.push(
                    StandardItem {
                        label: "Daemon not running".into(),
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }

        let active_profile = ds4l::ipc::status()
            .ok()
            .and_then(|s| parse_field(&s, "profile"));

        match ds4l::ipc::list_profiles() {
            Ok(names) if !names.is_empty() => {
                let selected = active_profile
                    .as_ref()
                    .and_then(|active| names.iter().position(|n| n == active))
                    .unwrap_or(usize::MAX);

                let options: Vec<RadioItem> = names
                    .iter()
                    .map(|name| RadioItem {
                        label: name.clone(),
                        ..Default::default()
                    })
                    .collect();

                items.push(
                    RadioGroup {
                        selected,
                        options,
                        select: Box::new(move |_this: &mut Self, index: usize| {
                            if let Some(name) = names.get(index) {
                                if let Err(e) = ds4l::ipc::switch_profile(name) {
                                    eprintln!("Failed to switch profile from tray: {e}");
                                }
                            }
                        }),
                    }
                    .into(),
                );
            }
            Ok(_) => {
                items.push(
                    StandardItem {
                        label: "No profiles found".into(),
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                );
            }
            Err(_) => {
                // Listing profiles reads the filesystem directly (see
                // ipc::list_profiles -> profile::list_profile_names),
                // independent of whether the daemon is running, so this
                // branch means something more fundamental is wrong
                // (e.g. can't read ~/.config/ds4l/profiles/) rather than
                // "daemon not running", which is already reported above.
                items.push(
                    StandardItem {
                        label: "Could not read profiles directory".into(),
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }

        items.push(ksni::menu::MenuItem::Separator);

        let tx = self.show_editor_tx.clone();
        items.push(
            StandardItem {
                label: "Edit Profiles...".into(),
                activate: Box::new(move |_this: &mut Self| {
                    let _ = tx.send(TrayEvent::ShowEditor);
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|_this: &mut Self| {
                    // Quits the GUI (tray + editor) only -- deliberately
                    // does NOT stop ds4l_daemon, which is a separate
                    // process the tray merely talks to over the control
                    // socket. Stopping the actual gamepad daemon from a
                    // "Quit" click on a status-icon menu would be a
                    // surprising, easy-to-trigger-by-accident action;
                    // `systemctl stop ds4l` (once systemd packaging
                    // exists) or Ctrl+C on the daemon's own terminal are
                    // the deliberate ways to do that.
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

/// Turns the raw "OK profile=X mode=Y connection=Z hidden=W" status
/// line into a short, human-readable menu label. Falls back to showing
/// the raw line if the format ever doesn't match what's expected,
/// rather than hiding information the person might want to see when
/// debugging.
fn format_status_line(raw: &str) -> String {
    let profile = parse_field(raw, "profile");
    let mode = parse_field(raw, "mode");
    let connection = parse_field(raw, "connection");
    match (profile, mode, connection) {
        (Some(p), Some(m), Some(c)) => format!("{p} ({m}, {c})"),
        _ => raw.to_string(),
    }
}

fn parse_field(status_line: &str, key: &str) -> Option<String> {
    status_line
        .split_whitespace()
        .find_map(|token| token.strip_prefix(&format!("{key}=")))
        .map(str::to_string)
}

/// Starts the tray icon on a background thread/task managed internally
/// by ksni's blocking API, returning a Handle the caller can use to
/// shut it down. `spawn` (from `ksni::blocking::TrayMethods`, pulled in
/// via the `blocking` feature in Cargo.toml) is expected to block until
/// the tray is registered with the StatusNotifierWatcher and then
/// return -- if that's not how the resolved ksni version's blocking API
/// actually behaves, this is the first place to check (see this file's
/// top doc comment).
pub fn start(show_editor_tx: Sender<TrayEvent>) -> Result<ksni::blocking::Handle<Ds4lTray>, ksni::Error> {
    let tray = Ds4lTray::new(show_editor_tx);
    tray.spawn()
}

/// Placeholder icon data, unused while icon_name() (theme-provided
/// icons) is in effect -- kept only so a future custom-icon path has an
/// obvious place to add one via `Tray::icon_pixmap` without restructuring
/// this file. Not referenced anywhere yet.
#[allow(dead_code)]
fn _unused_icon_placeholder() -> Icon {
    Icon {
        width: 0,
        height: 0,
        data: Vec::new(),
    }
}
