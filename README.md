# ds4l — Milestone 1

Reads a DS4 v2 (CUH-ZCT2x) over **USB** via hidraw, loads factory gyro/accel
calibration, and prints live button/stick/gyro state to the terminal.
No output/remap/uinput yet — this milestone is purely "can I read the pad
correctly."

## Setup (run on your actual Linux machine, not this sandbox)

1. Install the udev rule so you don't need root to read the controller:

   ```
   sudo cp 70-ds4l.rules /etc/udev/rules.d/
   sudo udevadm control --reload-rules && sudo udevadm trigger
   sudo usermod -aG input $USER
   ```

   Log out and back in for the group change to apply. If your distro uses
   `plugdev` instead of `input` for device permissions, edit the rule's
   `GROUP=` field accordingly (check `ls -l /dev/hidraw*` after plugging in
   to see what group other hidraw devices use).

2. Plug the DS4 v2 in over **USB** (Bluetooth is a later milestone — the
   report format differs).

3. Build and run:

   ```
   cargo build --release
   ./target/release/ds4l
   ```

## What to expect

- On connect, it reads feature report `0x02` and prints the parsed
  calibration struct. If this fails, gyro output will be uncalibrated
  (it'll drift) but buttons/sticks are unaffected.
- Then it streams parsed state once per report at roughly the pad's native
  polling rate, overwriting one terminal line.

## If it doesn't find the controller

- Confirm it's USB, not Bluetooth, for this milestone.
- Run `lsusb | grep 054c` to confirm the PID. This code assumes DS4 v2
  (`09cc`). If you actually have a v1 (`05c4`), edit `DS4_V2_PID` in
  `main.rs` or ask me for a v1-specific build — the input report layout
  differs slightly.
- Run `ls -l /dev/hidraw*` and confirm your user's group matches — if the
  udev rule didn't apply, `hidapi::open` will fail with a permissions error.

## Known simplifications in this milestone

- USB only, no Bluetooth framing/CRC yet.
- Calibration math matches the standard DS4/hid-sony approach but hasn't
  been cross-validated against your specific unit's plus/minus symmetry —
  if gyro values look off (e.g. consistently offset even after calibration),
  tell me the raw `gyro_x/y/z` values while the pad is dead flat and still,
  and I'll help tune it.
- Touchpad finger positions and lightbar/rumble output are not read/written
  yet — those come in later milestones.

## Milestone 2: virtual DS4 output (circular sweep test)

Creates a `uinput` virtual gamepad that identifies itself with DS4 v2's
real VID (`054C`) and PID (`09CC`), then sweeps both sticks in a circle
continuously — no real controller involved, this isolates and verifies
the *output* side only.

We talk to `/dev/uinput` via raw ioctls (`src/uinput_ds4.rs`) rather than
a high-level crate, because setting a custom vendor/product ID — the
whole point, since that's how SDL2 recognizes the device as a DS4 — isn't
exposed by the popular `uinput` crate's builder API. All struct layouts
and ioctl numbers in that file were manually verified against the kernel
source (`include/uapi/linux/uinput.h`, `include/uapi/linux/input.h`) for
byte-for-byte correctness, since a mismatched struct size silently fails
the ioctl with `EINVAL`.

### Setup

1. Make sure `/dev/uinput` exists and is accessible:

   ```
   sudo modprobe uinput
   sudo cp 70-ds4l.rules /etc/udev/rules.d/   # re-run even if you did this for M1
   sudo udevadm control --reload-rules && sudo udevadm trigger
   ```

   You should already be in the `input` group from Milestone 1 setup. If
   `/dev/uinput` still isn't writable after this, check `ls -l /dev/uinput`
   — some distros load the uinput module without the static device node
   until first access; the udev rule's `OPTIONS+="static_node=uinput"`
   handles this on modern systemd/udev, but older setups may need
   `uinput` added to `/etc/modules-load.d/`.

2. Build and run:

   ```
   cargo build --release
   ./target/release/virtual_ds4_test
   ```

3. In another terminal, verify:

   ```
   evtest
   ```
   Pick "Sony Interactive Entertainment Wireless Controller" from the
   list — you should see `ABS_X`/`ABS_Y`/`ABS_RX`/`ABS_RY` values cycling
   smoothly through a circle. Left stick and right stick sweep in
   *opposite* directions on purpose, so it's obvious if X/Y or L/R got
   mixed up.

   Or, for a visual bar-graph view:
   ```
   jstest /dev/input/jsN   # find N via ls /dev/input/js*
   ```

   To confirm SDL2 itself recognizes it as a DS4 (if you have
   `sdl2-jstest` installed):
   ```
   sdl2-jstest --list
   ```
   should show vendor `054c`, product `09cc`.

### What to expect

A clean, smooth circle on both sticks, opposite phase between left and
right. If the circle is actually an oval or a line, the axis ranges are
probably off — but the ranges here (0-255, center 128) match DS4 exactly,
so this would indicate an environment issue rather than a code bug; let
me know what you see if so.

### Known simplifications in this milestone

- Buttons/triggers/dpad are wired up (enabled via `UI_SET_KEYBIT`/
  `UI_SET_ABSBIT`) but not exercised by this test — only the two sticks
  move. That's deliberate: isolate one thing at a time.
- No real controller involved yet — that's Milestone 3.

## Milestone 3: real passthrough (real DS4 -> virtual DS4, 1:1)

Wires Milestone 1's real-pad parsing directly into Milestone 2's virtual
pad, with no remapping — move the real stick, the virtual stick moves;
press a real button, the virtual button fires. This is the first
milestone that behaves like a (minimal) working replica.

The parsing/calibration logic was pulled out of the Milestone 1 tool into
`src/ds4_input.rs` so it's shared, not duplicated — `src/main.rs` (the
M1 tool) now just calls into that module and behaves identically to
before.

### Setup

Same udev rules as Milestones 1 and 2 (hidraw + uinput access) — nothing
new to install.

### Run

```
cargo build --release
./target/release/passthrough_test
```

With the real DS4 v2 plugged in over USB. It'll connect, read
calibration (unused this milestone, but loaded the same way as M1), spin
up the virtual pad, then stream real input straight through.

### Verify

1. `evtest` on the virtual "Sony Interactive Entertainment Wireless
   Controller" — move sticks, press buttons on the *real* pad, watch the
   *virtual* device's events in evtest.
2. `jstest /dev/input/jsN` for a visual check.
3. The real test: open **Steam Big Picture** or any DS4-aware game and
   confirm it's picked up as a DualShock 4 and responds correctly to
   input. This is the actual goal of the whole project, so it's worth
   doing even though it's informal compared to `evtest`.

### What to expect

Every button/stick/trigger on the real pad should mirror 1:1 on the
virtual one, including the d-pad's 8-way directions. If a specific button
fires the wrong virtual button (e.g. pressing Circle shows up as East vs
some other position in evtest), tell me which real button produced which
wrong virtual event and I'll check the bit mapping for that one.

### Known simplifications in this milestone

- Real pad only — no fallback/reconnect handling if you unplug and
  replug mid-run; the program will just start erroring on reads. Milestone
  3.x can add hotplug detection via the `udev` crate once this core loop
  is confirmed solid.
- Gyro is read (for calibration) but not used yet — right stick is driven
  purely by the real right stick, no gyro blend. That's the next milestone.
- Touchpad finger data isn't read or forwarded yet.
- LED/rumble aren't wired up yet.

## Next milestone

Milestone 3.5: gyro-to-right-stick blending (additive, clamped to the
stick's circular range), with always-on / toggle / hold modes selectable
per profile — building directly on this passthrough loop.
