//! ds4l_gui: profile editor + tray icon for ds4l_daemon.
//!
//! Two things run side by side for the life of this process:
//!   - a GTK4 Application with one window (editor.rs) -- hidden, not
//!     destroyed, when closed via the window's own close button, so it
//!     can be re-shown from the tray instead of needing a relaunch.
//!   - a ksni tray icon (tray.rs) -- runs on its own D-Bus event
//!     thread(s), talks to a running ds4l_daemon over the local control
//!     socket (ds4l::ipc) to show live status and switch profiles.
//!
//! The tray thread can't touch GTK widgets directly (wrong thread for
//! GTK/GLib's single-threaded UI model), so "Edit Profiles..." sends a
//! `TrayEvent::ShowEditor` over an mpsc channel, and this file forwards
//! that onto the GTK main loop via `glib::MainContext::invoke` --
//! the standard, safe way to schedule work onto GLib's main loop from
//! another thread.
//!
//! UNVERIFIED -- see the top-of-file doc comments in tray.rs and
//! editor.rs for what specifically hasn't been build-checked and what
//! to confirm before relying on this daily.

mod editor;
mod tray;

use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;

const APP_ID: &str = "com.ds4l.gui";

fn main() {
    let app = gtk4::Application::builder().application_id(APP_ID).build();

    // GTK application would normally quit once its last window closes.
    // We don't want that here -- the whole point of the tray is that
    // the app keeps running (and the tray keeps working) after the
    // editor window is closed/hidden. `hold()` keeps the application's
    // reference count above zero for as long as this guard lives, i.e.
    // for the whole process lifetime, since we simply never drop it.
    let _hold_guard = app.hold();

    let (show_editor_tx, show_editor_rx) = mpsc::channel::<tray::TrayEvent>();
    // `connect_activate`'s closure must be `Fn` (GTK can in principle
    // call activate more than once), so a plain moved-in `Receiver`
    // (not `Clone`) can't be handed straight to the one-shot inner
    // `timeout_add_local` closure from inside an `Fn` closure. Wrapping
    // it in `Rc<RefCell<Option<_>>>` and `.take()`-ing it the first
    // time activate fires sidesteps that -- a second activate call (if
    // it ever happens) just finds `None` and skips re-installing the
    // poll, which is correct: only one poll loop should ever run.
    let show_editor_rx = Rc::new(RefCell::new(Some(show_editor_rx)));

    app.connect_activate(move |app| {
        let window = editor::build(app);
        // Shown once at startup so the person immediately sees the
        // editor rather than only a tray icon with no visible
        // indication anything launched -- matches how most tray-based
        // desktop apps behave on first run. Closing it afterward hides
        // it (see editor::build's set_hide_on_close), and the tray's
        // "Edit Profiles..." brings it back.
        window.present();

        // Forward ShowEditor events from the tray thread onto this GTK
        // main-loop iteration. Polling an std::sync::mpsc channel via a
        // repeating glib::timeout_add_local is a simple, if slightly
        // inelegant, way to bridge it into GLib's main loop without
        // pulling in a second async runtime -- consistent with this
        // project's general preference for plain threads/blocking
        // channels over async elsewhere (see ipc.rs).
        if let Some(rx) = show_editor_rx.borrow_mut().take() {
            let window_for_poll = window.clone();
            gtk4::glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
                while let Ok(tray::TrayEvent::ShowEditor) = rx.try_recv() {
                    window_for_poll.present();
                }
                gtk4::glib::ControlFlow::Continue
            });
        }
    });

    // Start the tray on its own thread(s) (ksni's blocking API manages
    // this internally -- see tray.rs's start()). Not joined: it runs
    // for the life of the process, same as the control-socket accept
    // thread in ipc.rs does on the daemon side.
    match tray::start(show_editor_tx) {
        Ok(_handle) => {
            // _handle is intentionally leaked/held for the process
            // lifetime rather than stored and explicitly shut down --
            // process exit (Quit menu item, or the window manager
            // closing this app) tears the tray down along with
            // everything else. If ksni's Handle type turns out to need
            // an explicit shutdown() call to unregister cleanly from
            // the StatusNotifierWatcher (rather than the D-Bus
            // connection just dropping), that's the first thing to
            // check if a stale/ghost tray icon is ever seen lingering
            // after quitting -- see this file's top doc comment.
            std::mem::forget(_handle);
        }
        Err(e) => {
            eprintln!(
                "Warning: failed to start tray icon: {e}\n\
                 Continuing with the profile editor only -- check that your desktop \
                 environment supports the StatusNotifierItem protocol (GNOME needs an \
                 AppIndicator extension; KDE/Xfce support it natively)."
            );
        }
    }

    app.run();
}
