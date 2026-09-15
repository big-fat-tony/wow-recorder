//! Combat-log tailing and Mythic+ event detection.
//!
//! WoW writes `WoWCombatLog*.txt` into `<install>/<flavor>/Logs`. With advanced
//! combat logging on, a Mythic+ run brackets its events with
//! `CHALLENGE_MODE_START` and `CHALLENGE_MODE_END`. We tail the newest log file
//! and emit those two events; the recorder starts/stops on them.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::Serialize;

/// File-name pattern for every WoW flavor.
pub const LOG_FILE_PATTERN: &str = r"^WoWCombatLog.*\.txt$";

/// Ignore log files not touched in the last few hours when first attaching.
const MAX_AGE_MS: i64 = 6 * 60 * 60 * 1000;
const POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeStart {
    pub zone: String,
    pub instance_id: Option<i64>,
    pub challenge_id: Option<i64>,
    pub keystone_level: Option<i64>,
    pub affixes: Vec<i64>,
    /// Raw combat-log timestamp (e.g. `9/14 20:01:05.123`).
    pub log_timestamp: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeEnd {
    pub instance_id: Option<i64>,
    pub success: bool,
    pub keystone_level: Option<i64>,
    pub duration_ms: Option<i64>,
    pub log_timestamp: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum LogEvent {
    ChallengeStart(ChallengeStart),
    ChallengeEnd(ChallengeEnd),
}

/// Split a combat-log CSV argument list, respecting `"..."` quoting and
/// `[...]`/`(...)` nesting (affix and info arrays).
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            '[' | '(' if !in_quotes => {
                depth += 1;
                cur.push(c);
            }
            ']' | ')' if !in_quotes => {
                depth -= 1;
                cur.push(c);
            }
            ',' if !in_quotes && depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn parse_int(s: &str) -> Option<i64> {
    s.trim().trim_matches('"').parse().ok()
}

fn parse_affixes(s: &str) -> Vec<i64> {
    s.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(|p| parse_int(p))
        .collect()
}

/// Parse one line into a Mythic+ event, if it is one.
pub fn parse_line(line: &str) -> Option<LogEvent> {
    if !line.contains("CHALLENGE_MODE_") {
        return None;
    }
    // `<timestamp>  <EVENT,args...>` — two spaces separate the timestamp.
    let (timestamp, rest) = line.split_once("  ")?;
    let log_timestamp = timestamp.trim().to_string();
    let (event, args_str) = rest.split_once(',')?;
    let args = split_args(args_str);

    match event.trim() {
        "CHALLENGE_MODE_START" => {
            // "zone", instanceID, challengeModeID, keystoneLevel, [affixes]
            let zone = args.first().map(|s| s.trim().trim_matches('"').to_string()).unwrap_or_default();
            Some(LogEvent::ChallengeStart(ChallengeStart {
                zone,
                instance_id: args.get(1).and_then(|s| parse_int(s)),
                challenge_id: args.get(2).and_then(|s| parse_int(s)),
                keystone_level: args.get(3).and_then(|s| parse_int(s)),
                affixes: args.get(4).map(|s| parse_affixes(s)).unwrap_or_default(),
                log_timestamp,
            }))
        }
        "CHALLENGE_MODE_END" => {
            // instanceID, success(0/1), keystoneLevel, totalTimeMs
            Some(LogEvent::ChallengeEnd(ChallengeEnd {
                instance_id: args.first().and_then(|s| parse_int(s)),
                success: args.get(1).and_then(|s| parse_int(s)).map(|v| v != 0).unwrap_or(false),
                keystone_level: args.get(2).and_then(|s| parse_int(s)),
                duration_ms: args.get(3).and_then(|s| parse_int(s)),
                log_timestamp,
            }))
        }
        _ => None,
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn modified_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Newest `WoWCombatLog*.txt` in `dir` modified within `MAX_AGE_MS`.
pub fn newest_log(dir: &Path, pattern: &Regex) -> Option<PathBuf> {
    let now = now_ms();
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| pattern.is_match(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .filter(|p| now - modified_ms(p) <= MAX_AGE_MS)
        .max_by_key(|p| modified_ms(p))
}

/// Tail the newest combat log in `dir`, invoking `on_event` for each Mythic+
/// event, until `stop` returns true. Blocking; run on a dedicated task.
pub fn tail<F, S>(dir: &Path, mut on_event: F, mut stop: S) -> io::Result<()>
where
    F: FnMut(LogEvent),
    S: FnMut() -> bool,
{
    let pattern = Regex::new(LOG_FILE_PATTERN).expect("valid pattern");
    let mut current: Option<PathBuf> = None;
    let mut position: u64 = 0;

    while !stop() {
        let latest = newest_log(dir, &pattern);
        match (&current, &latest) {
            (Some(c), Some(l)) if c != l => {
                // New log file rolled in: start at its end (ignore history).
                current = Some(l.clone());
                position = std::fs::metadata(l).map(|m| m.len()).unwrap_or(0);
            }
            (None, Some(l)) => {
                current = Some(l.clone());
                position = std::fs::metadata(l).map(|m| m.len()).unwrap_or(0);
            }
            _ => {}
        }

        if let Some(path) = &current {
            let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if len < position {
                position = 0; // truncated/rotated
            }
            if len > position {
                position = read_new_lines(path, position, &mut on_event)?;
            }
        }

        std::thread::sleep(POLL);
    }
    Ok(())
}

/// Read complete lines from `position` to EOF, emitting parsed events; returns
/// the new position (start of any trailing partial line).
fn read_new_lines<F: FnMut(LogEvent)>(path: &Path, position: u64, on_event: &mut F) -> io::Result<u64> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(position))?;
    let mut reader = BufReader::new(file);
    let mut pos = position;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 || !buf.ends_with(b"\n") {
            break; // EOF or partial line — leave it for next poll
        }
        pos += n as u64;
        let line = String::from_utf8_lossy(&buf);
        if let Some(event) = parse_line(line.trim_end()) {
            on_event(event);
        }
    }
    Ok(pos)
}

/// Best-effort discovery of a WoW `Logs` directory.
pub fn detect_log_directory() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Ok(p) = std::env::var(var) {
            roots.push(Path::new(&p).join("World of Warcraft"));
        }
    }
    for drive in b'C'..=b'Z' {
        let d = format!("{}:\\", drive as char);
        if Path::new(&d).exists() {
            roots.push(Path::new(&d).join("World of Warcraft"));
            roots.push(Path::new(&d).join("Games").join("World of Warcraft"));
        }
    }
    for flavor in ["_retail_", "_classic_", "_classic_era_"] {
        for root in &roots {
            let logs = root.join(flavor).join("Logs");
            if logs.is_dir() {
                return Some(logs);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_challenge_start() {
        let line = r#"9/14 20:01:05.123  CHALLENGE_MODE_START,"Ara-Kara, City of Echoes",2648,542,10,[10,9,147]"#;
        match parse_line(line).unwrap() {
            LogEvent::ChallengeStart(s) => {
                assert_eq!(s.zone, "Ara-Kara, City of Echoes");
                assert_eq!(s.instance_id, Some(2648));
                assert_eq!(s.challenge_id, Some(542));
                assert_eq!(s.keystone_level, Some(10));
                assert_eq!(s.affixes, vec![10, 9, 147]);
                assert_eq!(s.log_timestamp, "9/14 20:01:05.123");
            }
            _ => panic!("expected start"),
        }
    }

    #[test]
    fn parses_challenge_end_success() {
        let line = "9/14 20:34:51.900  CHALLENGE_MODE_END,2648,1,10,1826000";
        match parse_line(line).unwrap() {
            LogEvent::ChallengeEnd(e) => {
                assert_eq!(e.instance_id, Some(2648));
                assert!(e.success);
                assert_eq!(e.keystone_level, Some(10));
                assert_eq!(e.duration_ms, Some(1826000));
            }
            _ => panic!("expected end"),
        }
    }

    #[test]
    fn parses_challenge_end_depleted() {
        let line = "9/14 20:34:51.900  CHALLENGE_MODE_END,2648,0,10,2400000";
        match parse_line(line).unwrap() {
            LogEvent::ChallengeEnd(e) => assert!(!e.success),
            _ => panic!("expected end"),
        }
    }

    #[test]
    fn ignores_unrelated_lines() {
        assert!(parse_line("9/14 20:01:05.123  SPELL_DAMAGE,Player-1,\"x\"").is_none());
        assert!(parse_line("garbage").is_none());
    }

    #[test]
    fn newer_timestamp_format_with_year() {
        let line = r#"9/14/2026 20:01:05.123-4  CHALLENGE_MODE_START,"Zone",1,2,7,[1]"#;
        match parse_line(line).unwrap() {
            LogEvent::ChallengeStart(s) => {
                assert_eq!(s.keystone_level, Some(7));
                assert_eq!(s.log_timestamp, "9/14/2026 20:01:05.123-4");
            }
            _ => panic!("expected start"),
        }
    }
}
