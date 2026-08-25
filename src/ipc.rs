//! Local control-socket protocol between the running daemon (server) and
//! any client that wants to inspect or change its state without a
//! restart -- primarily `ds4l_gui`'s tray icon, but usable from a shell
//! too (`socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/ds4l/control.sock`), since
//! the protocol is deliberately plain newline-delimited text rather than
//! a binary format -- matching this project's existing preference for
//! things a person can inspect/debug by hand (see profile.rs's choice of
//! plain TOML for the same reasoning).
//!
//! NOT YET VERIFIED against a live daemon/GUI pair the way every other
//! module in this project was checked against real hardware before being
//! trusted -- this is new, untested protocol code. Test the primitive
//! first with a raw `socat`/`nc -U` session before relying on ds4l_gui
//! working end-to-end, the same "verify the primitive before building on
//! it" discipline used for gyro calibration and the BT CRC work.
//!
//! Wire protocol: one command per line in, one response line out,
//! connection closes after the response -- no persistent session or
//! subscription. A client that wants live status polls periodically
//! instead, which keeps the server side dead simple: accept, read one
//! line, reply one line, close.
//!
//! Commands (client -> server), one per connection:
//!   PING                    -> "OK PONG"
//!   STATUS                  -> "OK profile=<name> mode=<Gamepad|Kbm> connection=<USB|Bluetooth> hidden=<true|false>"
//!   LIST_PROFILES           -> "OK <name1>,<name2>,..." (comma-separated; "OK " with nothing after if none exist)
//!   SWITCH_PROFILE <name>   -> "OK" on success, "ERR <message>" on failure
//!
//! A failed SWITCH_PROFILE always leaves the daemon running its PREVIOUS
//! profile completely unchanged -- see ds4l_daemon.rs's
//! `apply_profile_switch`, which builds everything the new profile needs
//! before touching any existing state, so nothing is ever left half
//! migrated between two profiles.
//!
//! Any unrecognized command, or a line that fails to parse, gets
//! "ERR unknown command" back.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Snapshot of daemon state exposed via STATUS, kept up to date by
/// main() at startup and after every successful profile switch. Plain
/// strings rather than re-using ds4l::profile::OutputMode / the
/// daemon-local Connection enum directly, so this module stays a
/// self-contained protocol definition that doesn't need to know about
/// either of those types' representations.
#[derive(Debug, Clone, Default)]
pub struct StatusSnapshot {
    pub profile_name: String,
    pub output_mode: String, // "Gamepad" or "Kbm"
    pub connection: String,  // "USB" or "Bluetooth"
    pub hidden: bool,
}

/// One command relayed from a control-socket connection to the daemon's
/// main loop, because SWITCH_PROFILE needs to touch live state (virtual
/// devices, gyro/touchpad/kbm session state, controller-hiding) that
/// only the main loop's thread should mutate -- the accept-thread that
/// parses the command runs on a separate OS thread per connection and
/// must not reach into that state directly. `reply` lets the main loop
/// report success/failure back to whichever client asked, before that
/// client's connection closes.
pub enum PendingCommand {
    SwitchProfile {
        name: String,
        reply: Sender<Result<(), String>>,
    },
}

/// Resolves the control socket's path: `$XDG_RUNTIME_DIR/ds4l/control.sock`
/// when available (the normal case under systemd-managed sessions, where
/// that directory is already private to the user, mode 0700), falling
/// back to `/tmp/ds4l-<uid>/control.sock` on systems without
/// XDG_RUNTIME_DIR set (minimal/container setups). The fallback is
/// namespaced by uid so two users on the same machine can't collide --
/// or, worse, one user's GUI accidentally controlling another user's
/// daemon -- and `start()` below explicitly tightens that fallback
/// directory to 0700 since /tmp itself offers no such protection by
/// default.
pub fn socket_path() -> PathBuf {
    let dir = if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("ds4l")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/ds4l-{uid}"))
    };
    dir.join("control.sock")
}

/// Starts the control-socket server: binds the socket, spawns a
/// detached accept-loop thread (one short-lived thread per connection),
/// and returns a channel the caller's main loop should poll
/// (non-blockingly, via `try_recv`) for SWITCH_PROFILE requests. PING/
/// STATUS/LIST_PROFILES are answered entirely within the accept thread
/// and never reach this channel, since they don't need to touch
/// main-loop-owned state.
///
/// Returns an error (rather than panicking) if binding fails, so the
/// caller can treat "no control socket this run" as a warning, not a
/// fatal condition -- the daemon is fully usable without it, just not
/// remotely controllable.
pub fn start(status: Arc<Mutex<StatusSnapshot>>) -> std::io::Result<Receiver<PendingCommand>> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        // Best-effort tighten to 0700; not fatal if this fails (e.g. the
        // directory already existed with different ownership from
        // something else) -- not worth failing daemon startup over.
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    // Clean up a stale socket file left behind by a previous, uncleanly
    // terminated daemon (kill -9 -- see hide_controller.rs's doc comment
    // on the same failure mode). Try connecting first: if something IS
    // listening, this is a real conflict (two daemons somehow running)
    // and we bail rather than silently stealing another running
    // daemon's socket out from under it.
    if path.exists() {
        match UnixStream::connect(&path) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "another ds4l_daemon appears to already be running \
                         (control socket {} is live)",
                        path.display()
                    ),
                ));
            }
            Err(_) => {
                // Nobody home -- stale file, safe to remove and rebind.
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    let listener = UnixListener::bind(&path)?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            let tx = tx.clone();
            let status = status.clone();
            // One thread per connection: connections are one-shot
            // (single command, single response, close), so this never
            // accumulates -- each thread exits as soon as it's replied.
            std::thread::spawn(move || {
                handle_connection(stream, &tx, &status);
            });
        }
    });

    Ok(rx)
}

fn handle_connection(stream: UnixStream, tx: &Sender<PendingCommand>, status: &Arc<Mutex<StatusSnapshot>>) {
    let mut reader = match stream.try_clone() {
        Ok(s) => BufReader::new(s),
        Err(_) => return,
    };
    let mut writer = stream;

    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return; // client disconnected without sending anything
    }
    let line = line.trim();

    let response = match parse_command(line) {
        Command::Ping => "OK PONG".to_string(),
        Command::Status => {
            let s = status.lock().unwrap();
            format!(
                "OK profile={} mode={} connection={} hidden={}",
                s.profile_name, s.output_mode, s.connection, s.hidden
            )
        }
        Command::ListProfiles => match crate::profile::list_profile_names() {
            Ok(names) => format!("OK {}", names.join(",")),
            Err(e) => format!("ERR failed to list profiles: {e}"),
        },
        Command::SwitchProfile(name) => {
            let (reply_tx, reply_rx) = mpsc::channel();
            if tx
                .send(PendingCommand::SwitchProfile { name, reply: reply_tx })
                .is_err()
            {
                "ERR daemon main loop is not accepting commands (shutting down?)".to_string()
            } else {
                // Bounded wait: a switch can take up to ~1s in the worst
                // case (uinput device recreation + an optional rumble
                // pulse, see apply_profile_switch's doc comment in
                // ds4l_daemon.rs), so 5s leaves comfortable headroom
                // without leaving a client hanging indefinitely if the
                // main loop is somehow stuck.
                match reply_rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(Ok(())) => "OK".to_string(),
                    Ok(Err(e)) => format!("ERR {e}"),
                    Err(_) => "ERR timed out waiting for daemon to apply the switch".to_string(),
                }
            }
        }
        Command::Unknown => "ERR unknown command".to_string(),
    };

    let _ = writeln!(writer, "{response}");
}

enum Command {
    Ping,
    Status,
    ListProfiles,
    SwitchProfile(String),
    Unknown,
}

fn parse_command(line: &str) -> Command {
    if line == "PING" {
        Command::Ping
    } else if line == "STATUS" {
        Command::Status
    } else if line == "LIST_PROFILES" {
        Command::ListProfiles
    } else if let Some(name) = line.strip_prefix("SWITCH_PROFILE ") {
        let name = name.trim();
        if name.is_empty() {
            Command::Unknown
        } else {
            Command::SwitchProfile(name.to_string())
        }
    } else {
        Command::Unknown
    }
}

// ---- Client-side helpers (used by ds4l_gui) ----

/// Sends one command and returns the single response line, trimmed.
/// Blocking, one-shot: connects, writes the command, reads one line,
/// drops the connection. A 6s read timeout guards against a hung/
/// unresponsive daemon leaving the GUI blocked forever.
fn send_command(cmd: &str) -> std::io::Result<String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)?;
    stream.write_all(cmd.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.set_read_timeout(Some(Duration::from_secs(6)))?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response.trim().to_string())
}

/// Returns true only if a daemon is listening and responded correctly --
/// any I/O error (no daemon running, stale/missing socket, permission
/// issue) is treated as "not available" rather than propagated, since
/// callers use this purely to decide what to show in a tray menu.
pub fn ping() -> bool {
    send_command("PING")
        .map(|r| r == "OK PONG")
        .unwrap_or(false)
}

/// Returns the raw "OK ..." status line, or an error if no daemon is
/// reachable. Left unparsed (rather than returning a StatusSnapshot)
/// since the GUI only needs a couple of these fields for display and
/// re-parsing "key=value" pairs at the call site is simpler than adding
/// a second serialization format just for this.
pub fn status() -> std::io::Result<String> {
    send_command("STATUS")
}

pub fn list_profiles() -> std::io::Result<Vec<String>> {
    let resp = send_command("LIST_PROFILES")?;
    if let Some(rest) = resp.strip_prefix("OK ") {
        Ok(rest
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect())
    } else if resp == "OK" {
        Ok(Vec::new())
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::Other, resp))
    }
}

pub fn switch_profile(name: &str) -> std::io::Result<()> {
    let resp = send_command(&format!("SWITCH_PROFILE {name}"))?;
    if resp == "OK" {
        Ok(())
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::Other, resp))
    }
}
