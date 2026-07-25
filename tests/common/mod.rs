//! Fixtures for the end-to-end tests: a throwaway `CLAUDE_CONFIG_DIR`, real git
//! repositories, and a way to run the real binary against them.
//!
//! Everything here goes through a child process. `registry::claude_home()` reads
//! `CLAUDE_CONFIG_DIR` from the process environment, and `cargo test` runs tests
//! as threads in one process, so setting it in-process would race between tests.
//! Spawning the binary with the variable set on the child sidesteps that and
//! makes these genuinely end-to-end at the same time.
#![allow(dead_code)]

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

/// A fake `CLAUDE_CONFIG_DIR` plus a fake `$HOME` to hang project repos off.
///
/// The real `~/.claude` is never touched: gaff is strictly read-only over that
/// tree, and a test that wrote to it would be corrupting the user's live state.
pub struct Fixture {
    _tmp: TempDir,
    /// Canonical tempdir root. gaff canonicalises every path it reads, and on
    /// macOS tempdirs sit under `/var`, a symlink to `/private/var`, so the
    /// fixture has to resolve its own paths or nothing compares equal.
    pub root: PathBuf,
    pub home: PathBuf,
    pub config: PathBuf,
    /// Registry filenames only have to be unique and end in `.json`; gaff reads
    /// every `*.json` in the directory and takes the pid from the contents.
    counter: Cell<u32>,
}

impl Fixture {
    pub fn new() -> Self {
        let fx = Self::bare();
        fs::create_dir_all(fx.config.join("sessions")).unwrap();
        fs::create_dir_all(fx.config.join("projects")).unwrap();
        fx
    }

    /// A config dir with neither `sessions/` nor `projects/` — what a machine
    /// that has never run a Claude Code session looks like.
    pub fn bare() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let config = home.join(".claude");
        fs::create_dir_all(&config).unwrap();
        Fixture {
            _tmp: tmp,
            root,
            home,
            config,
            counter: Cell::new(0),
        }
    }

    fn next_id(&self) -> u32 {
        let n = self.counter.get();
        self.counter.set(n + 1);
        n
    }

    /// Create a git repo at `~/projects/<name>` and return its canonical path.
    pub fn repo(&self, name: &str, branch: &str) -> PathBuf {
        init_repo(&self.home.join("projects").join(name), branch)
    }

    pub fn add_session(&self, spec: &SessionSpec) {
        let path =
            self.config
                .join("sessions")
                .join(format!("{}-{}.json", spec.pid, self.next_id()));
        fs::write(path, spec.to_json()).unwrap();
    }

    /// Write a registry file verbatim — for feeding gaff bytes a builder would
    /// never produce, such as a half-written or corrupt file.
    pub fn write_registry_file(&self, filename: &str, contents: &str) {
        fs::write(self.config.join("sessions").join(filename), contents).unwrap();
    }

    /// Write a transcript where Claude Code would put it: under a project
    /// directory named after the slugified cwd. gaff finds it by session id
    /// rather than by slug, so the directory name is cosmetic here.
    pub fn add_transcript(&self, cwd: &Path, session_id: &str, contents: &str) {
        let slug = cwd.to_string_lossy().replace(['/', '.'], "-");
        let dir = self.config.join("projects").join(slug);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{session_id}.jsonl")), contents).unwrap();
    }

    /// Run `gaff --once`, assert it succeeded, and hand back its stdout.
    pub fn run_once(&self) -> String {
        let out = self
            .command()
            .arg("--once")
            .output()
            .expect("run gaff --once");
        assert!(
            out.status.success(),
            "gaff --once failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("gaff prints utf-8")
    }

    /// A `Command` for the real binary, pointed at this fixture.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gaff"));
        cmd.env("CLAUDE_CONFIG_DIR", &self.config)
            .env("HOME", &self.home);
        cmd
    }
}

/// A registry entry. Every field beyond pid/sessionId/cwd is optional in the
/// real format, so each is optional here too.
pub struct SessionSpec {
    pub pid: i32,
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub status: Option<String>,
    pub status_updated_at: Option<i64>,
    pub started_at: Option<i64>,
    pub version: Option<String>,
}

impl SessionSpec {
    /// Defaults to a live pid, since a dead one is dropped before it can be
    /// asserted on.
    pub fn new(session_id: &str, cwd: &Path) -> Self {
        SessionSpec {
            pid: live_pid(),
            session_id: session_id.into(),
            cwd: cwd.to_path_buf(),
            name: None,
            status: None,
            status_updated_at: None,
            started_at: None,
            version: None,
        }
    }

    pub fn pid(mut self, pid: i32) -> Self {
        self.pid = pid;
        self
    }

    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn status(mut self, status: &str) -> Self {
        self.status = Some(status.into());
        self
    }

    /// Minutes ago that the session entered its current status.
    pub fn status_age_mins(mut self, mins: i64) -> Self {
        self.status_updated_at = Some(now_ms() - mins * 60_000);
        self
    }

    pub fn started_mins_ago(mut self, mins: i64) -> Self {
        self.started_at = Some(now_ms() - mins * 60_000);
        self
    }

    pub fn version(mut self, v: &str) -> Self {
        self.version = Some(v.into());
        self
    }

    fn to_json(&self) -> String {
        let mut obj = serde_json::Map::new();
        obj.insert("pid".into(), self.pid.into());
        obj.insert("sessionId".into(), self.session_id.clone().into());
        obj.insert("cwd".into(), self.cwd.to_string_lossy().into_owned().into());
        let mut put = |k: &str, v: Option<serde_json::Value>| {
            if let Some(v) = v {
                obj.insert(k.into(), v);
            }
        };
        put("name", self.name.clone().map(Into::into));
        put("status", self.status.clone().map(Into::into));
        put("statusUpdatedAt", self.status_updated_at.map(Into::into));
        put("startedAt", self.started_at.map(Into::into));
        put("version", self.version.clone().map(Into::into));
        serde_json::Value::Object(obj).to_string()
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A pid guaranteed to be alive: our own.
pub fn live_pid() -> i32 {
    std::process::id() as i32
}

/// A pid guaranteed to be dead.
///
/// Picking a large number and hoping is not reliable — it may well be in use.
/// Running a trivial child to completion and reaping it leaves its pid free, and
/// nothing on the machine can reuse it before the test looks at it, short of the
/// pid space wrapping within microseconds.
pub fn dead_pid() -> i32 {
    let mut child = Command::new("true").spawn().expect("spawn true");
    let pid = child.id() as i32;
    child.wait().expect("reap true");
    pid
}

pub fn ai_title(title: &str) -> String {
    format!(
        "{{\"type\":\"ai-title\",\"aiTitle\":{}}}\n",
        json_str(title)
    )
}

pub fn last_prompt(prompt: &str) -> String {
    format!(
        "{{\"type\":\"last-prompt\",\"lastPrompt\":{}}}\n",
        json_str(prompt)
    )
}

fn json_str(s: &str) -> String {
    serde_json::Value::String(s.into()).to_string()
}

pub fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A real repository with one commit — grouping resolves through git, so a
/// fabricated `.git` directory would not exercise the same code.
pub fn init_repo(path: &Path, branch: &str) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", branch]);
    git(path, &["config", "user.email", "t@example.com"]);
    git(path, &["config", "user.name", "t"]);
    fs::write(path.join("f"), "x").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-qm", "init"]);
    path.canonicalize().unwrap()
}

/// A real linked worktree, whose `.git` is a file pointing back at the parent.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            path.to_str().unwrap(),
        ],
    );
    path.canonicalize().unwrap()
}

/// Project heading lines from `--once` output — the unindented ones.
pub fn headings(out: &str) -> Vec<&str> {
    out.lines()
        .filter(|l| !l.is_empty() && !l.starts_with(' '))
        .collect()
}

/// Agent row lines, in the order printed.
pub fn agent_rows(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("  ")).collect()
}

/// The agent row whose NAME column is `name`.
pub fn agent_row<'a>(out: &'a str, name: &str) -> Option<&'a str> {
    agent_rows(out)
        .into_iter()
        .find(|l| l.split_whitespace().next() == Some(name))
}

// `print_once` lays each agent row out in fixed-width fields:
//   "  {name:<14} {status:<8} {age:<4} {where:<24} {branch:<26} {title}"
// so a column can be read back by offset. Asserting on the whole row instead
// would let a match in the wrong column pass — several columns fall back to the
// same "—" placeholder.
fn column(row: &str, start: usize, width: Option<usize>) -> String {
    let chars = row.chars().skip(start);
    match width {
        Some(w) => chars.take(w).collect::<String>().trim().to_string(),
        None => chars.collect::<String>().trim().to_string(),
    }
}

pub fn status_of(row: &str) -> String {
    column(row, 17, Some(8))
}

pub fn where_of(row: &str) -> String {
    column(row, 31, Some(24))
}

pub fn branch_of(row: &str) -> String {
    column(row, 56, Some(26))
}

pub fn title_of(row: &str) -> String {
    column(row, 83, None)
}

/// Every file under `dir` as (relative path, length, mtime) — enough to catch
/// gaff writing, truncating or touching anything in the tree it reads.
pub fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, u64, SystemTime)>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let meta = entry.metadata().unwrap();
        let rel = path.strip_prefix(root).unwrap().to_path_buf();
        out.push((rel, meta.len(), meta.modified().unwrap()));
        if meta.is_dir() {
            walk(root, &path, out);
        }
    }
}
