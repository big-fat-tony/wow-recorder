# WoW Recorder

A lightweight Mythic+ (and later raid) recorder for World of Warcraft.

It watches your WoW combat log for `CHALLENGE_MODE_START` / `CHALLENGE_MODE_END`
and records the run automatically — no memory reading, no process hooks. Each
recording gets a JSON sidecar with the dungeon, key level, affixes, result, and
the combat-log timestamps, so a run can later be aligned with its Warcraft Logs
report (pairs with [wcl-uploader](https://github.com/big-fat-tony/wcl-uploader)
and bdk-analyzer).

Runs in the system tray and stays out of the way.

## Status

- ✅ Combat-log tailing + Mythic+ detection (validated against live logs)
- ✅ Recording state machine + JSON metadata sidecars
- ✅ Tray app, config, single-instance
- ✅ OBS capture backend (libobs-recorder) — records the WoW window out-of-process
- ✅ Player deaths become MP4 chapters (VLC, mpv, MPC-HC, ffmpeg-based players)
- ✅ Optional burned-in run clock (`m:ss` since the key started) so an analyzer
  timestamp can be found in the footage
- ✅ Optional two-track audio: the game alone on track 1, everything the PC
  plays plus the microphone on track 2, with a mic boost for headsets that sit
  below the voices coming out of the desktop
- ✅ Optional cover over the chat window, so whispers, party chat and other
  people's names stay out of footage that gets shared
- ✅ Abandoned keys are kept: WoW logs no end event for one, so the recording is
  closed when the dungeon is left, WoW exits, the combat log goes quiet, or the
  app is told to quit — or on demand, from the window or the tray

The OBS runtime (DLLs + `extprocess_recorder.exe`) is vendored by
big-fat-tony/libobs-recorder and copied next to the app at build time by
`build-helper`. Requires the **nightly** toolchain (artifact/bindeps). If the
runtime is missing at runtime the app falls back to a `noop` recorder.

## Requirements

- **Advanced combat logging** enabled in WoW (`/combatlog`, or an addon like the
  in-game logger) — this is what emits the Mythic+ events.
- Windows (capture backend is Windows-only).

## Building

Requires the nightly Rust toolchain (pinned via `rust-toolchain.toml`).

```bash
npm install
npm run dev      # run in the tray with hot reload
npm run build    # release build + installer
cd src-tauri && cargo test
```

## How it works

`combatlog.rs` tails the newest `WoWCombatLog*.txt` and parses the challenge
events plus player `UNIT_DIED`s; `session.rs` turns them into recorder
start/stop calls, collects deaths as markers, writes the metadata and, once
the file is finalized, injects the markers as chapters (`mp4chapters.rs`, a
Nero `chpl` atom — no re-mux); `recorder.rs` is the capture abstraction (a
`Recorder` trait) whose OBS implementation drives libobs-recorder. See
`wcl-uploader` for the sibling uploader and the shared combat-log format notes.

### Syncing footage with the combat log

OBS needs a moment to start, so video `0:00` is a little after
`CHALLENGE_MODE_START`. Everything is timed against the machine clock (combat
log timestamps are local time, so the log file's write lag doesn't matter):

- `challengeStartVideoOffsetMs` in the sidecar is where run time `0:00` falls
  in the video (negative = before the file starts): `video_ms = run_ms + offset`.
- `markers[]` carry both `videoOffsetMs` and `runOffsetMs` per death.
- With **Burn the run clock into the video** on (default), the top-left corner
  shows the run clock, so `14:19` in bdk-analyzer is `14:19` on screen.
