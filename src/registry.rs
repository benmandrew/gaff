//! Reads Claude Code's live-session registry: `~/.claude/sessions/<pid>.json`.
//!
//! This is internal CLI state, not a documented API — every field beyond `pid`
//! and `sessionId` is treated as optional so a schema change degrades the
//! display rather than breaking the tool.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub pid: i32,
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// When the session last entered its current status.
    #[serde(default)]
    pub status_updated_at: Option<i64>,
}

impl Session {
    /// A name to show in the `AGENT` column, falling back to the session id's
    /// first eight bytes — or to the id whole when that would split a character
    /// or run off the end.
    ///
    /// `get` rather than a slice, because the id belongs to Claude Code and
    /// nothing guarantees its length or encoding. This is called from the render
    /// path, so a panic here takes the TUI down instead of degrading one column.
    /// `ui` truncates to the column width regardless, so an over-long fallback
    /// costs nothing.
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .unwrap_or_else(|| self.session_id.get(..8).unwrap_or(&self.session_id))
    }

    pub fn status_str(&self) -> &str {
        self.status.as_deref().unwrap_or("unknown")
    }
}

pub fn claude_home() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".claude")
        }
    }
}

pub fn sessions_dir() -> PathBuf {
    claude_home().join("sessions")
}

pub fn projects_dir() -> PathBuf {
    claude_home().join("projects")
}

/// True if the process exists. `EPERM` counts as alive — the process is there,
/// we just don't own it.
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs error checking only; it delivers nothing.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Load every registered session whose process is still alive.
///
/// Registry files appear to be swept on exit, but a hard kill can leave one
/// behind, so liveness is verified rather than assumed.
pub fn load() -> Result<Vec<Session>> {
    let dir = sessions_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        // No sessions dir at all just means nothing is running yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context(format!("reading {}", dir.display())),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // A session writing its file as we read it yields a partial parse;
        // skipping is correct, the next refresh picks it up.
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Ok(session) = serde_json::from_str::<Session>(&text) else { continue };
        if pid_alive(session.pid) {
            out.push(session);
        }
    }
    Ok(out)
}

/// Locate a session's transcript by scanning project dirs for `<sessionId>.jsonl`.
///
/// The project dir name is a slugified cwd, but a session that relocated into a
/// worktree lives under the *worktree's* slug, not its launch dir. Rather than
/// reimplement the slug rules, we match on the globally-unique session id.
pub fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let filename = format!("{session_id}.jsonl");
    let dirs = fs::read_dir(projects_dir()).ok()?;
    for entry in dirs.flatten() {
        let candidate = entry.path().join(&filename);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Shorten `/Users/me/projects/foo` to `~/projects/foo` for display.
pub fn tildify(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Some(home) = std::env::var_os("HOME") {
        // Paths are canonicalised before they reach here, so `$HOME` has to be
        // resolved the same way or a symlinked home would never match.
        let home = crate::git::canonical(Path::new(&home));
        if let Some(rest) = s.strip_prefix(&*home.to_string_lossy()) {
            return format!("~{rest}");
        }
    }
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Session {
        serde_json::from_str::<Session>(json).expect("session parses")
    }

    /// The registry belongs to Claude Code, not to us. Everything past the two
    /// fields we cannot work without has to be optional, so an older or leaner
    /// writer still yields a usable row instead of vanishing from the list.
    #[test]
    fn a_minimal_session_parses_with_everything_else_absent() {
        let s = parse(r#"{"pid":42,"sessionId":"0123abcd-ef","cwd":"/repo"}"#);
        assert_eq!(s.pid, 42);
        assert_eq!(s.cwd, PathBuf::from("/repo"));
        assert!(s.started_at.is_none());
        assert!(s.version.is_none());
        assert!(s.kind.is_none());
        assert!(s.entrypoint.is_none());
        assert!(s.name.is_none());
        assert!(s.status.is_none());
        assert!(s.status_updated_at.is_none());
    }

    /// The other half of the same contract: Claude Code adding a field must not
    /// make every session fail to parse and empty the whole table.
    #[test]
    fn unknown_fields_are_ignored_rather_than_rejected() {
        let s = parse(
            r#"{"pid":7,"sessionId":"0123abcd-ef","cwd":"/repo",
                "somethingNew":{"nested":[1,2,3]},"futureCount":9,"model":"opus"}"#,
        );
        assert_eq!(s.pid, 7);
    }

    /// The registry writes camelCase; a rename that silently stopped matching
    /// would blank the `FOR` column and the uptime line without any error.
    #[test]
    fn camel_case_names_map_onto_the_struct() {
        let s = parse(
            r#"{"pid":7,"sessionId":"abcd1234-ef","cwd":"/repo",
                "startedAt":1700000000000,"statusUpdatedAt":1700000060000,
                "version":"2.0.1","kind":"cli","entrypoint":"tui",
                "name":"gaff","status":"waiting"}"#,
        );
        assert_eq!(s.session_id, "abcd1234-ef");
        assert_eq!(s.started_at, Some(1_700_000_000_000));
        assert_eq!(s.status_updated_at, Some(1_700_000_060_000));
        assert_eq!(s.version.as_deref(), Some("2.0.1"));
        assert_eq!(s.kind.as_deref(), Some("cli"));
        assert_eq!(s.entrypoint.as_deref(), Some("tui"));
    }

    /// `pid` drives the liveness check and `sessionId` finds the transcript;
    /// without either there is nothing to show, so a malformed file is dropped
    /// by `load` rather than displayed half-populated.
    #[test]
    fn the_two_load_bearing_fields_are_required() {
        assert!(serde_json::from_str::<Session>(r#"{"sessionId":"abcd1234","cwd":"/r"}"#).is_err());
        assert!(serde_json::from_str::<Session>(r#"{"pid":1,"cwd":"/r"}"#).is_err());
    }

    /// An unnamed session still needs something in the `NAME` column, and the
    /// session id's leading chars are what the user sees elsewhere.
    #[test]
    fn display_name_falls_back_to_the_session_id_prefix() {
        let named = parse(r#"{"pid":1,"sessionId":"abcd1234-ef","cwd":"/r","name":"tidy-docs"}"#);
        assert_eq!(named.display_name(), "tidy-docs");

        let unnamed = parse(r#"{"pid":1,"sessionId":"abcd1234-5678","cwd":"/r"}"#);
        assert_eq!(unnamed.display_name(), "abcd1234");
    }

    /// The id is another program's field, so nothing about its length or
    /// encoding is guaranteed. Both of these used to panic on a raw `[..8]`
    /// slice, and from the render path, which took the whole TUI down rather
    /// than degrading the one column that could not be filled.
    #[test]
    fn a_short_or_multi_byte_session_id_does_not_panic() {
        let short = parse(r#"{"pid":1,"sessionId":"abc","cwd":"/r"}"#);
        assert_eq!(short.display_name(), "abc");

        // Byte 8 falls inside the third character, so slicing there is a panic.
        let wide = parse(r#"{"pid":1,"sessionId":"日本語テスト","cwd":"/r"}"#);
        assert_eq!(wide.display_name(), "日本語テスト");

        let empty = parse(r#"{"pid":1,"sessionId":"","cwd":"/r"}"#);
        assert_eq!(empty.display_name(), "");
    }

    /// An unrecognised or missing status must render as a word, since `ui`
    /// styles by string and a blank cell reads as a rendering failure.
    #[test]
    fn status_str_falls_back_to_unknown() {
        let s = parse(r#"{"pid":1,"sessionId":"abcd1234","cwd":"/r"}"#);
        assert_eq!(s.status_str(), "unknown");
        let s = parse(r#"{"pid":1,"sessionId":"abcd1234","cwd":"/r","status":"waiting"}"#);
        assert_eq!(s.status_str(), "waiting");
    }

    /// A path that cannot lie under any home directory has to survive display
    /// untouched — mangling one would point the user at a directory that does
    /// not exist. Written to hold whatever `$HOME` happens to be.
    #[test]
    fn paths_outside_home_are_left_alone() {
        let Some(home) = std::env::var_os("HOME") else { return };
        let home = crate::git::canonical(Path::new(&home)).to_string_lossy().into_owned();
        if home.is_empty() {
            return;
        }
        // Two candidates, so one of them is guaranteed not to be a prefix of
        // this machine's home no matter where home lives.
        let outside = if home.starts_with("/zz") { "/yy-not-home/x" } else { "/zz-not-home/x" };
        assert_eq!(tildify(Path::new(outside)), outside);
    }

    /// The `WHERE` column and the detail pane are narrow; a home-relative path
    /// is the difference between a readable row and a truncated one.
    #[test]
    fn paths_under_home_are_shortened() {
        let Some(home) = std::env::var_os("HOME") else { return };
        // `tildify` canonicalises `$HOME` before comparing, so the fixture has
        // to be built from the canonical form or a symlinked home never matches.
        let home = crate::git::canonical(Path::new(&home));
        if home.as_os_str().is_empty() {
            return;
        }
        assert_eq!(tildify(&home.join("projects/gaff")), "~/projects/gaff");
        assert_eq!(tildify(&home), "~");
    }
}
