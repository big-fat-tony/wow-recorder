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
- ⏳ OBS capture backend (libobs-recorder) — behind the `obs` feature

Until the OBS backend is wired, the app runs with a `noop` recorder that logs
the start/stop timing and writes metadata but produces no video. Build with
`--features obs` once the capture backend is in place.

## Requirements

- **Advanced combat logging** enabled in WoW (`/combatlog`, or an addon like the
  in-game logger) — this is what emits the Mythic+ events.
- Windows (capture backend is Windows-only).

## Building

```bash
npm install
npm run dev      # run in the tray with hot reload
npm run build    # release build + installer
cd src-tauri && cargo test
```

## How it works

`combatlog.rs` tails the newest `WoWCombatLog*.txt` and parses the challenge
events; `session.rs` turns them into recorder start/stop calls and writes the
metadata; `recorder.rs` is the capture abstraction (a `Recorder` trait) whose
OBS implementation drives libobs-recorder. See `wcl-uploader` for the sibling
uploader and the shared combat-log format notes.
