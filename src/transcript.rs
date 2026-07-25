//! Incremental follower for session transcripts (`~/.claude/projects/*/*.jsonl`).
//!
//! Transcripts run to several MB each and the whole corpus is hundreds of MB, so
//! re-reading on every change is not an option. Two things keep this cheap:
//!
//!   1. On first sight we seek to the last `INITIAL_TAIL` bytes. The records we
//!      want (`ai-title`, `last-prompt`, `worktree-state`) are rewritten
//!      throughout a session rather than written once, so the tail carries a
//!      current copy. If the tail yields nothing we fall back to a full scan once.
//!   2. Afterwards we only ever read bytes appended since the last offset.
//!
//! Lines are cheaply prefiltered by substring before paying for a JSON parse —
//! most of a transcript is large assistant messages we have no interest in.

use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How far back to read when first following a transcript.
const INITIAL_TAIL: u64 = 512 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSession {
    #[serde(default)]
    pub original_cwd: Option<PathBuf>,
    // The worktree's own name is not kept: it is derivable from `cwd`, which the
    // process chdirs into. Only what git cannot tell us is retained.
    #[serde(default)]
    pub worktree_branch: Option<String>,
    #[serde(default)]
    pub original_branch: Option<String>,
}

/// What we surface about a session, distilled from its transcript.
#[derive(Debug, Clone, Default)]
pub struct Info {
    /// Model-generated one-line summary of the session's work.
    pub title: Option<String>,
    /// Most recent user prompt (already truncated by the writer).
    pub last_prompt: Option<String>,
    /// Present only when the session relocated into a worktree.
    pub worktree: Option<WorktreeSession>,
    /// Mtime of the transcript — a good proxy for last activity.
    pub last_activity: Option<SystemTime>,
}

/// Per-file read position and accumulated state.
struct Follower {
    offset: u64,
    info: Info,
    scanned_fully: bool,
    /// Whether `offset` is known to sit on a line boundary. False only for the
    /// initial blind seek into the tail, which lands mid-record.
    aligned: bool,
}

#[derive(Default)]
pub struct Transcripts {
    followers: HashMap<PathBuf, Follower>,
}

impl Transcripts {
    /// Bring `path` up to date and return the current distilled info.
    pub fn read(&mut self, path: &Path) -> Info {
        let Ok(meta) = std::fs::metadata(path) else {
            return Info::default();
        };
        let len = meta.len();

        let follower = self.followers.entry(path.to_path_buf()).or_insert_with(|| {
            // Start near the end; a fresh, short transcript is read in full.
            let offset = len.saturating_sub(INITIAL_TAIL);
            Follower {
                offset,
                info: Info::default(),
                scanned_fully: len <= INITIAL_TAIL,
                aligned: offset == 0,
            }
        });

        // A shrinking file means it was rotated or rewritten — start over.
        if len < follower.offset {
            follower.offset = 0;
            follower.scanned_fully = true;
            follower.aligned = true;
        }

        if len > follower.offset {
            follower.offset = consume(path, follower.offset, follower.aligned, &mut follower.info);
            follower.aligned = true;
        }

        // The tail held none of the records we wanted; pay for one full pass.
        if !follower.scanned_fully
            && follower.info.title.is_none()
            && follower.info.last_prompt.is_none()
        {
            let mut fresh = Info::default();
            consume(path, 0, true, &mut fresh);
            fresh.last_activity = follower.info.last_activity;
            follower.info = fresh;
            follower.scanned_fully = true;
        }

        follower.info.last_activity = meta.modified().ok();
        follower.info.clone()
    }

    /// Drop followers for transcripts no longer in play, so a long-running
    /// gaff doesn't accumulate state for every session it ever saw.
    pub fn retain(&mut self, keep: &[PathBuf]) {
        self.followers.retain(|k, _| keep.contains(k));
    }
}

/// Read complete lines from `start`, folding any records of interest into `info`.
/// Returns the offset just past the last complete line consumed.
///
/// `aligned` says whether `start` sits on a line boundary. When it does not, the
/// leading partial record is discarded; when it does, discarding would silently
/// drop the first newly-appended record.
fn consume(path: &Path, start: u64, aligned: bool, info: &mut Info) -> u64 {
    let Ok(file) = File::open(path) else {
        return start;
    };
    let mut reader = BufReader::new(file);

    let mut pos = start;
    if start > 0 {
        if reader.seek(SeekFrom::Start(start)).is_err() {
            return start;
        }
        if !aligned {
            let mut partial = Vec::new();
            match reader.read_until(b'\n', &mut partial) {
                Ok(0) => return start,
                Ok(n) => pos += n as u64,
                Err(_) => return start,
            }
        }
    }

    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(n) => {
                // A line without a trailing newline is still being written;
                // leave `pos` before it so we re-read it once complete.
                if line.last() != Some(&b'\n') {
                    break;
                }
                pos += n as u64;
                if let Ok(text) = std::str::from_utf8(&line) {
                    apply(text, info);
                }
            }
            Err(_) => break,
        }
    }
    pos
}

/// Fold a single transcript line into `info`, if it is a record we care about.
fn apply(line: &str, info: &mut Info) {
    // Substring prefilter: skip the JSON parse for the bulk of the file.
    let is_title = line.contains(r#""ai-title""#);
    let is_prompt = line.contains(r#""last-prompt""#);
    let is_worktree = line.contains(r#""worktree-state""#);
    if !(is_title || is_prompt || is_worktree) {
        return;
    }

    #[derive(Deserialize)]
    struct Record {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default, rename = "aiTitle")]
        ai_title: Option<String>,
        #[serde(default, rename = "lastPrompt")]
        last_prompt: Option<String>,
        #[serde(default, rename = "worktreeSession")]
        worktree_session: Option<WorktreeSession>,
    }

    let Ok(rec) = serde_json::from_str::<Record>(line) else {
        return;
    };
    // These records recur through a transcript; last one wins.
    match rec.kind.as_str() {
        "ai-title" => info.title = rec.ai_title.or(info.title.take()),
        "last-prompt" => info.last_prompt = rec.last_prompt.or(info.last_prompt.take()),
        "worktree-state" => info.worktree = rec.worktree_session.or(info.worktree.take()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn append(path: &Path, text: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    const TITLE: &str = r#"{"type":"ai-title","aiTitle":"Center tables","sessionId":"s"}"#;
    const PROMPT: &str = r#"{"type":"last-prompt","lastPrompt":"make it centred","sessionId":"s"}"#;
    const WORKTREE: &str = r#"{"type":"worktree-state","worktreeSession":{"originalCwd":"/repo","worktreeName":"wt","worktreeBranch":"worktree-wt","originalBranch":"main"},"sessionId":"s"}"#;
    const NOISE: &str = r#"{"type":"assistant","message":{"content":"lots of text"},"uuid":"x"}"#;

    #[test]
    fn extracts_records_of_interest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        append(&p, &format!("{NOISE}\n{TITLE}\n{PROMPT}\n{WORKTREE}\n"));

        let info = Transcripts::default().read(&p);
        assert_eq!(info.title.as_deref(), Some("Center tables"));
        assert_eq!(info.last_prompt.as_deref(), Some("make it centred"));
        let wt = info.worktree.expect("worktree state");
        assert_eq!(wt.worktree_branch.as_deref(), Some("worktree-wt"));
        assert_eq!(wt.original_branch.as_deref(), Some("main"));
        assert_eq!(wt.original_cwd.as_deref(), Some(Path::new("/repo")));
    }

    #[test]
    fn later_records_supersede_earlier() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        append(&p, &format!("{TITLE}\n"));

        let mut t = Transcripts::default();
        assert_eq!(t.read(&p).title.as_deref(), Some("Center tables"));

        // Only the appended bytes are re-read, but the newer title must win.
        append(
            &p,
            "{\"type\":\"ai-title\",\"aiTitle\":\"Handle overflow\"}\n",
        );
        assert_eq!(t.read(&p).title.as_deref(), Some("Handle overflow"));
    }

    #[test]
    fn earlier_state_survives_incremental_reads() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        append(&p, &format!("{TITLE}\n"));

        let mut t = Transcripts::default();
        t.read(&p);
        append(&p, &format!("{NOISE}\n{PROMPT}\n"));

        // The title came from a chunk we no longer re-read; it must persist.
        let info = t.read(&p);
        assert_eq!(info.title.as_deref(), Some("Center tables"));
        assert_eq!(info.last_prompt.as_deref(), Some("make it centred"));
    }

    #[test]
    fn partial_trailing_line_is_deferred_until_complete() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        // A record still being written, with no trailing newline yet.
        append(&p, &TITLE[..TITLE.len() - 20]);

        let mut t = Transcripts::default();
        assert_eq!(
            t.read(&p).title,
            None,
            "half-written line must not be parsed"
        );

        append(&p, &format!("{}\n", &TITLE[TITLE.len() - 20..]));
        assert_eq!(t.read(&p).title.as_deref(), Some("Center tables"));
    }

    #[test]
    fn truncation_resets_the_follower() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        append(&p, &format!("{NOISE}\n{TITLE}\n"));

        let mut t = Transcripts::default();
        assert_eq!(t.read(&p).title.as_deref(), Some("Center tables"));

        // Rewritten shorter: the old offset now points past the end.
        std::fs::write(&p, format!("{PROMPT}\n")).unwrap();
        let info = t.read(&p);
        assert_eq!(info.last_prompt.as_deref(), Some("make it centred"));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        append(
            &p,
            &format!("not json at all\n{{\"type\":\"ai-title\" truncated\n{TITLE}\n"),
        );

        assert_eq!(
            Transcripts::default().read(&p).title.as_deref(),
            Some("Center tables")
        );
    }

    #[test]
    fn missing_file_yields_empty_info() {
        let dir = tempfile::tempdir().unwrap();
        let info = Transcripts::default().read(&dir.path().join("nope.jsonl"));
        assert!(info.title.is_none() && info.worktree.is_none());
    }
}
