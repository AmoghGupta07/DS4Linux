#!/usr/bin/env bash
# Builds ds4l_daemon and applies the capabilities it needs for the
# hide_real_controller feature (see src/hide_controller.rs's doc
# comment on why CAP_FOWNER/CAP_DAC_OVERRIDE are required at all --
# changing permissions on a device node this process doesn't own needs
# them, and udev group membership alone isn't enough).
#
# Deliberately a plain script you run explicitly, NOT a cargo build.rs
# hook: a build script silently shelling out to `sudo` on every
# `cargo build` would mean privilege escalation happening as a hidden
# side effect of a command that should never need your password. This
# script makes the same one-command convenience available, but sudo
# stays visible and something you chose to run right now -- consistent
# with this project's general stance on permission changes (see
# hide_controller.rs and its extensive comments on runtime-only,
# explicit, restorable permission changes).
#
# IMPORTANT: capabilities set with setcap attach to the BINARY FILE
# itself and do NOT survive a rebuild -- every `cargo build` produces a
# new file at the same path, so this needs to run again after every
# build, not just once. That's the whole reason this script exists
# instead of a one-time setup instruction.
#
# Usage:
#   ./scripts/build.sh                    # debug build (matches `cargo build`)
#   ./scripts/build.sh --release          # release build (matches `cargo build --release`)
#   ./scripts/build.sh --release --gui    # also builds ds4l_gui (needs GTK4 dev headers)
#   ./scripts/build.sh --run              # build, setcap, then launch ds4l_daemon
#   ./scripts/build.sh --release --run -- --profile Racing --bluetooth
#                                          # everything after `--` is forwarded to
#                                          # ds4l_daemon verbatim (its own --profile/
#                                          # --bluetooth/--bt-feedback flags), same
#                                          # convention as `cargo run -- <args>`.
# --run execs (not just spawns) ds4l_daemon as this script's final step,
# so Ctrl+C in this same terminal reaches the daemon directly, and the
# daemon's own SIGINT/SIGTERM handling (see ds4l_daemon.rs's ctrlc setup)
# still applies exactly as if you'd typed `./target/.../ds4l_daemon`
# yourself -- this script gets out of the way rather than staying around
# as an extra layer to signal through.

set -euo pipefail

PROFILE="debug"
CARGO_ARGS=()
FEATURES_ARGS=()
RUN=false
DAEMON_ARGS=()

# Args after a literal `--` are collected verbatim as DAEMON_ARGS,
# regardless of what they look like -- this is what lets `--profile`,
# `--bluetooth`, etc. (ds4l_daemon's own flags, not this script's) pass
# through without this script needing to know about every flag
# ds4l_daemon supports.
COLLECTING_DAEMON_ARGS=false
for arg in "$@"; do
    if [ "$COLLECTING_DAEMON_ARGS" = true ]; then
        DAEMON_ARGS+=("$arg")
        continue
    fi
    case "$arg" in
        --)
            COLLECTING_DAEMON_ARGS=true
            ;;
        --release)
            PROFILE="release"
            CARGO_ARGS+=("--release")
            ;;
        --gui)
            FEATURES_ARGS+=("--features" "gui" "--bin" "ds4l_gui")
            ;;
        --run)
            RUN=true
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            echo "Usage: $0 [--release] [--gui] [--run] [-- <ds4l_daemon args>]" >&2
            exit 1
            ;;
    esac
done

echo "==> Building ds4l_daemon (${PROFILE})..."
cargo build "${CARGO_ARGS[@]}" --bin ds4l_daemon

BIN_PATH="target/${PROFILE}/ds4l_daemon"

if [ ! -f "$BIN_PATH" ]; then
    echo "error: expected binary at ${BIN_PATH} but it doesn't exist -- build may have failed silently." >&2
    exit 1
fi

echo "==> Applying capabilities to ${BIN_PATH} (will prompt for your password)..."
sudo setcap cap_fowner,cap_dac_override+ep "$BIN_PATH"

echo "==> Done. Verify with: getcap ${BIN_PATH}"
getcap "$BIN_PATH"

if [ "${#FEATURES_ARGS[@]}" -gt 0 ]; then
    echo "==> Building ds4l_gui..."
    cargo build "${CARGO_ARGS[@]}" "${FEATURES_ARGS[@]}"
    echo "(ds4l_gui was built but is not launched by this script -- run target/${PROFILE}/ds4l_gui yourself.)"
fi

if [ "$RUN" = true ]; then
    echo "==> Launching ${BIN_PATH} ${DAEMON_ARGS[*]:-}"
    exec "./${BIN_PATH}" "${DAEMON_ARGS[@]}"
fi

