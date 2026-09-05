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
//!   PING                              -> "OK PONG"
//!   LIST_CONTROLLERS                  -> "OK <id1>,<id2>,..." (comma-separated; "OK" alone if none)
//!   STATUS <controller_id>            -> "OK profile=<name> mode=<Gamepad|Kbm> connection=<USB|Bluetooth> hidden=<true|false> battery=<0-100> charging=<true|false>"
//!   LIST_PROFILES                     -> "OK <name1>,<name2>,..." (global, filesystem-based -- not per controller)
//!   SWITCH_PROFILE <controller_id> <name> -> "OK" on success, "ERR <message>" on failure
//!
//! PROTOCOL v2: STATUS and SWITCH_PROFILE gained a leading <controller_id>
//! argument, and LIST_CONTROLLERS is new -- this daemon can now run
//! several controllers at once (see ds4l_daemon.rs's per-controller
//! threads), each independently profiled, so every command that used to
//! implicitly mean "the one controller this daemon drives" now has to
//! say which one. Breaking the wire format is fine at this stage: this
//! protocol was only just introduced and (per the note above) hasn't
//! been relied on by anyone yet.
//!
//! For SWITCH_PROFILE, everything after the controller id and ONE space
//! is taken as the profile name verbatim (so names containing spaces,
//! e.g. "My Racing Profile", work correctly) -- see parse_command.
//!
//! A failed SWITCH_PROFILE always leaves that controller running its
//! PREVIOUS profile completely unchanged -- see ds4l_daemon.rs's
//! `apply_profile_switch`, which builds everything the new profile needs
//! before touching any existing state, so nothing is ever left half
//! migrated between two profiles.
//!
//! Any unrecognized command, or a line that fails to parse, gets
//! "ERR unknown command" back. STATUS/SWITCH_PROFILE against a
//! controller id that isn't currently registered gets
//! "ERR unknown controller \"<id>\"".

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Snapshot of one controller's state exposed via STATUS, kept up to
/// date by that controller's own thread in ds4l_daemon.rs -- at startup
/// and after every successful profile switch. Plain strings rather than
/// re-using ds4l::profile::OutputMode / the daemon-local Connection enum
/// directly, so this module stays a self-contained protocol definition
/// that doesn't need to know about either of those types' representations.
#[derive(Debug, Clone, Default)]
pub struct StatusSnapshot {
    pub profile_name: String,
    pub output_mode: String, // "Gamepad" or "Kbm"
    pub connection: String,  // "USB" or "Bluetooth"
    pub hidden: bool,
    /// 0-100, updated continuously (every report, not just on profile
    /// switch) by the controller's own thread -- unlike the other
    /// fields here, this changes on its own over time independent of
    /// anything the person does.
    pub battery_percent: u8,
    pub battery_charging: bool,
}

/// One command relayed from a control-socket connection to a SPECIFIC
/// controller's thread (looked up in the Registry by controller id
/// before sending), because SWITCH_PROFILE needs to touch that
/// controller's live state (virtual devices, gyro/touchpad/kbm session
/// state, controller-hiding) that only its own thread should mutate.
/// `reply` lets that thread report success/failure back to whichever
/// client asked, before that client's connection closes.
pub enum PendingCommand {
    SwitchProfile {
        name: String,
        reply: Sender<Result<(), String>>,
    },
}

/// Everything the control socket's accept-thread needs to reach ONE
/// running controller: a channel into its thread (for SWITCH_PROFILE)
/// and a shared, continuously-updated status snapshot (for STATUS).
/// Inserted into the Registry by that controller's thread right after
/// it starts, and never removed in this pass -- see ds4l_daemon.rs's
/// doc comment on why hot-unplug isn't handled yet.
pub struct ControllerHandle {
    pub cmd_tx: Sender<PendingCommand>,
    pub status: Arc<Mutex<StatusSnapshot>>,
}

/// Shared map of controller id -> handle, populated by ds4l_daemon.rs
/// as each controller's thread starts, read by the control socket's
/// accept-thread to route STATUS/SWITCH_PROFILE/LIST_CONTROLLERS.
pub type Registry = Arc<Mutex<HashMap<String, ControllerHandle>>>;

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

/// Starts the control socket: binds it and spawns a detached accept-loop
/// thread (one short-lived thread per connection). All routing happens
/// through `registry`, which the caller populates as controller threads
/// start -- this function itself doesn't need to know how many
/// controllers exist or will exist, only where to look them up.
///
/// Returns an error (rather than panicking) if binding fails, so the
/// caller can treat "no control socket this run" as a warning, not a
/// fatal condition -- every controller's own thread is still fully
/// functional without it, just not remotely controllable.
pub fn start(registry: Registry) -> std::io::Result<()> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
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
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    let listener = UnixListener::bind(&path)?;

    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            let registry = registry.clone();
            std::thread::spawn(move || {
                handle_connection(stream, &registry);
            });
        }
    });

    Ok(())
}

fn handle_connection(stream: UnixStream, registry: &Registry) {
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
        Command::ListControllers => {
            let ids: Vec<String> = registry.lock().unwrap().keys().cloned().collect();
            format!("OK {}", ids.join(","))
        }
        Command::Status(id) => {
            let snapshot = registry
                .lock()
                .unwrap()
                .get(&id)
                .map(|handle| handle.status.lock().unwrap().clone());
            match snapshot {
                Some(s) => format!(
                    "OK profile={} mode={} connection={} hidden={} battery={} charging={}",
                    s.profile_name, s.output_mode, s.connection, s.hidden, s.battery_percent, s.battery_charging
                ),
                None => format!("ERR unknown controller \"{id}\""),
            }
        }
        Command::ListProfiles => match crate::profile::list_profile_names() {
            Ok(names) => format!("OK {}", names.join(",")),
            Err(e) => format!("ERR failed to list profiles: {e}"),
        },
        Command::SwitchProfile(id, name) => {
            // Clone the sender and drop the registry lock BEFORE the
            // potentially multi-second blocking wait below -- holding
            // the lock that long would stall every OTHER client's
            // LIST_CONTROLLERS/STATUS call (they all go through the
            // same registry mutex) for as long as this one switch takes.
            let cmd_tx = registry.lock().unwrap().get(&id).map(|h| h.cmd_tx.clone());
            match cmd_tx {
                Some(tx) => {
                    let (reply_tx, reply_rx) = mpsc::channel();
                    if tx
                        .send(PendingCommand::SwitchProfile { name, reply: reply_tx })
                        .is_err()
                    {
                        "ERR that controller's thread is not accepting commands (shutting down?)"
                            .to_string()
                    } else {
                        // Bounded wait: a switch can take up to ~1s in
                        // the worst case (uinput device recreation + an
                        // optional rumble pulse, see
                        // apply_profile_switch's doc comment in
                        // ds4l_daemon.rs), so 5s leaves comfortable
                        // headroom without leaving a client hanging
                        // indefinitely if that controller's thread is
                        // somehow stuck.
                        match reply_rx.recv_timeout(Duration::from_secs(5)) {
                            Ok(Ok(())) => "OK".to_string(),
                            Ok(Err(e)) => format!("ERR {e}"),
                            Err(_) => {
                                "ERR timed out waiting for the controller to apply the switch"
                                    .to_string()
                            }
                        }
                    }
                }
                None => format!("ERR unknown controller \"{id}\""),
            }
        }
        Command::Unknown => "ERR unknown command".to_string(),
    };

    let _ = writeln!(writer, "{response}");
}

enum Command {
    Ping,
    ListControllers,
    Status(String),
    ListProfiles,
    SwitchProfile(String, String),
    Unknown,
}

fn parse_command(line: &str) -> Command {
    if line == "PING" {
        return Command::Ping;
    }
    if line == "LIST_CONTROLLERS" {
        return Command::ListControllers;
    }
    if line == "LIST_PROFILES" {
        return Command::ListProfiles;
    }
    if let Some(rest) = line.strip_prefix("STATUS ") {
        let id = rest.trim();
        return if id.is_empty() {
            Command::Unknown
        } else {
            Command::Status(id.to_string())
        };
    }
    if let Some(rest) = line.strip_prefix("SWITCH_PROFILE ") {
        // rest = "<id> <name...>" -- split on the FIRST space only, so a
        // profile name containing spaces (e.g. "My Racing Profile") is
        // preserved intact in the second half rather than truncated at
        // its first word.
        return match rest.split_once(' ') {
            Some((id, name)) if !id.trim().is_empty() && !name.trim().is_empty() => {
                Command::SwitchProfile(id.trim().to_string(), name.trim().to_string())
            }
            _ => Command::Unknown,
        };
    }
    Command::Unknown
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

pub fn list_controllers() -> std::io::Result<Vec<String>> {
    let resp = send_command("LIST_CONTROLLERS")?;
    parse_csv_ok(&resp)
}

/// Returns the raw "OK ..." status line for one controller, or an error
/// if no daemon is reachable or that controller id is unknown. Left
/// unparsed (rather than returning a StatusSnapshot) since callers only
/// need a couple of these fields for display and re-parsing "key=value"
/// pairs at the call site is simpler than adding a second serialization
/// format just for this.
pub fn status(controller_id: &str) -> std::io::Result<String> {
    send_command(&format!("STATUS {controller_id}"))
}

pub fn list_profiles() -> std::io::Result<Vec<String>> {
    let resp = send_command("LIST_PROFILES")?;
    parse_csv_ok(&resp)
}

fn parse_csv_ok(resp: &str) -> std::io::Result<Vec<String>> {
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
        Err(std::io::Error::new(std::io::ErrorKind::Other, resp.to_string()))
    }
}

pub fn switch_profile(controller_id: &str, name: &str) -> std::io::Result<()> {
    let resp = send_command(&format!("SWITCH_PROFILE {controller_id} {name}"))?;
    if resp == "OK" {
        Ok(())
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::Other, resp))
    }
}

/// Extracts one "key=value" field from a raw STATUS response line (e.g.
/// pulls "72" out of "...battery=72 charging=false..."). Shared by
/// every GUI surface that displays STATUS output (tray.rs's menu labels
/// and editor.rs's live controller status line) so the "key=value"
/// format only has one parser to keep in sync with the format string in
/// this file's own STATUS handling above, rather than two GUI binaries
/// each re-implementing the same split-on-whitespace logic.
pub fn parse_status_field(status_line: &str, key: &str) -> Option<String> {
    status_line
        .split_whitespace()
        .find_map(|token| token.strip_prefix(&format!("{key}=")))
        .map(str::to_string)
}
