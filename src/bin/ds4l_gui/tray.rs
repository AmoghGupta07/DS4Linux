//! System tray icon: shows one submenu per connected controller (each
//! with its own live status and profile picker), lets the user switch
//! any controller's profile with one click, and has a menu item to
//! raise the profile editor window.
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
use ksni::menu::{RadioGroup, RadioItem, StandardItem, SubMenu};
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
        // status per controller are never stale by more than "however
        // long the menu happened to be closed." ipc calls here are
        // one-shot blocking round-trips over a local unix socket, so
        // this is fast (sub-millisecond typically) even with one extra
        // STATUS call per controller -- see ipc.rs.
        //
        // MULTI-CONTROLLER: one submenu per controller id, each with its
        // own profile radio group -- see ipc.rs's protocol v2 doc
        // comment for why STATUS/SWITCH_PROFILE now take a controller id
        // (this daemon can drive more than one DS4 at once, each
        // independently profiled).
        let mut items: Vec<MenuItem<Self>> = Vec::new();

        let controller_ids = match ds4l::ipc::list_controllers() {
            Ok(ids) if !ids.is_empty() => ids,
            Ok(_) => {
                items.push(disabled_item("Daemon running, but no controllers connected"));
                items.push(MenuItem::Separator);
                push_footer(&mut items, &self.show_editor_tx);
                return items;
            }
            Err(_) => {
                items.push(disabled_item("Daemon not running"));
                items.push(MenuItem::Separator);
                push_footer(&mut items, &self.show_editor_tx);
                return items;
            }
        };

        // Profile list is global (filesystem-based), so it's the same
        // set of choices offered under every controller's submenu --
        // only WHICH one is currently selected differs per controller.
        let profile_names = ds4l::ipc::list_profiles().unwrap_or_default();

        for controller_id in controller_ids {
            let status_line = ds4l::ipc::status(&controller_id).ok();
            let active_profile = status_line.as_deref().and_then(|s| ds4l::ipc::parse_status_field(s, "profile"));

            let submenu_label = match &status_line {
                Some(s) => format!("{controller_id} \u{2014} {}", format_status_line(s)),
                None => format!("{controller_id} \u{2014} (unreachable)"),
            };

            let mut submenu_items: Vec<MenuItem<Self>> = Vec::new();
            if profile_names.is_empty() {
                submenu_items.push(disabled_item("No profiles found"));
            } else {
                let selected = active_profile
                    .as_ref()
                    .and_then(|active| profile_names.iter().position(|n| n == active))
                    .unwrap_or(usize::MAX);

                let options: Vec<RadioItem> = profile_names
                    .iter()
                    .map(|name| RadioItem {
                        label: name.clone(),
                        ..Default::default()
                    })
                    .collect();

                let names_for_closure = profile_names.clone();
                let id_for_closure = controller_id.clone();
                submenu_items.push(
                    RadioGroup {
                        selected,
                        options,
                        select: Box::new(move |_this: &mut Self, index: usize| {
                            if let Some(name) = names_for_closure.get(index) {
                                if let Err(e) = ds4l::ipc::switch_profile(&id_for_closure, name) {
                                    eprintln!(
                                        "Failed to switch profile for {id_for_closure} from tray: {e}"
                                    );
                                }
                            }
                        }),
                    }
                    .into(),
                );
            }

            items.push(
                SubMenu {
                    label: submenu_label,
                    submenu: submenu_items,
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        push_footer(&mut items, &self.show_editor_tx);
        items
    }
}

fn disabled_item<T: Tray>(label: &str) -> MenuItem<T> {
    StandardItem {
        label: label.to_string(),
        enabled: false,
        ..Default::default()
    }
    .into()
}

/// Appends the "Edit Profiles..." and "Quit" items shared by every menu
/// state (normal, daemon-unreachable, no-controllers) -- factored out
/// so those early-return branches above don't have to duplicate them.
fn push_footer(items: &mut Vec<MenuItem<Ds4lTray>>, show_editor_tx: &Sender<TrayEvent>) {
    let tx = show_editor_tx.clone();
    items.push(
        StandardItem {
            label: "Edit Profiles...".into(),
            activate: Box::new(move |_this: &mut Ds4lTray| {
                let _ = tx.send(TrayEvent::ShowEditor);
            }),
            ..Default::default()
        }
        .into(),
    );

    items.push(
        StandardItem {
            label: "Quit".into(),
            activate: Box::new(|_this: &mut Ds4lTray| {
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
}

/// Turns the raw "OK profile=X mode=Y connection=Z hidden=W" status
/// line into a short, human-readable menu label. Falls back to showing
/// Turns the raw "OK profile=X mode=Y connection=Z hidden=W battery=N
/// charging=B" status line into a short, human-readable menu label.
/// Falls back to showing the raw line if the format ever doesn't match
/// what's expected, rather than hiding information the person might
/// want to see when debugging.
fn format_status_line(raw: &str) -> String {
    let profile = ds4l::ipc::parse_status_field(raw, "profile");
    let mode = ds4l::ipc::parse_status_field(raw, "mode");
    let connection = ds4l::ipc::parse_status_field(raw, "connection");
    let battery = ds4l::ipc::parse_status_field(raw, "battery");
    let charging = ds4l::ipc::parse_status_field(raw, "charging").as_deref() == Some("true");

    match (profile, mode, connection) {
        (Some(p), Some(m), Some(c)) => match battery {
            Some(pct) if charging => format!("{p} ({m}, {c}, {pct}% \u{26a1}charging)"),
            Some(pct) => format!("{p} ({m}, {c}, {pct}%)"),
            None => format!("{p} ({m}, {c})"),
        },
        _ => raw.to_string(),
    }
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
