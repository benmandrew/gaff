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
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.session_id[..8])
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
        let home = home.to_string_lossy().to_string();
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    s.into_owned()
}
