//! Application state: the joined view of registry + transcript + git,
//! grouped by the project each agent belongs to.

use crate::git;
use crate::registry::{self, Session};
use crate::transcript::{Info, Transcripts};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct Agent {
    pub session: Session,
    pub info: Info,
    pub branch: Option<String>,
    pub transcript: Option<PathBuf>,
    /// Main repo this agent belongs to — the grouping key. Worktrees resolve to
    /// the project they were cut from, not to themselves.
    pub project: PathBuf,
}

impl Agent {
    pub fn is_worktree(&self) -> bool {
        self.worktree_name().is_some()
    }

    /// Name of the worktree this agent sits in, if any.
    ///
    /// Derived from the directory alone. The claude process genuinely `chdir`s
    /// when it relocates into a worktree, so `cwd` is authoritative for both the
    /// built-in flow and a worktree entered by hand — one code path covers both,
    /// and the session's own `worktree-state` record is not needed here.
    pub fn worktree_name(&self) -> Option<String> {
        let root = git::repo_root(&self.session.cwd)?;
        if root == self.project {
            return None;
        }
        Some(root.file_name()?.to_string_lossy().into_owned())
    }

    /// Where inside the project this agent sits: a worktree, a subdirectory, or
    /// the project root itself.
    pub fn location(&self) -> String {
        if let Some(name) = self.worktree_name() {
            return format!("⑂ {name}");
        }
        match self.session.cwd.strip_prefix(&self.project) {
            Ok(rel) if rel.as_os_str().is_empty() => "—".into(),
            Ok(rel) => rel.to_string_lossy().into_owned(),
            // Not under the project at all (e.g. a non-repo dir) — show it whole.
            Err(_) => registry::tildify(&self.session.cwd),
        }
    }

    /// Time since the transcript last grew — real work, as opposed to the
    /// registry heartbeat, which ticks whether or not anything is happening.
    pub fn idle_for(&self) -> Option<Duration> {
        SystemTime::now().duration_since(self.info.last_activity?).ok()
    }

    /// How long this agent has held its current status. For a `waiting` agent
    /// that is precisely how long it has been blocked on you.
    ///
    /// Falls back to transcript mtime when the registry omits the timestamp.
    pub fn status_age(&self) -> Option<Duration> {
        let since = match self.session.status_updated_at {
            Some(ms) => UNIX_EPOCH + Duration::from_millis(ms.try_into().ok()?),
            None => self.info.last_activity?,
        };
        SystemTime::now().duration_since(since).ok()
    }

    pub fn uptime(&self) -> Option<Duration> {
        let started = UNIX_EPOCH + Duration::from_millis(self.session.started_at?.try_into().ok()?);
        SystemTime::now().duration_since(started).ok()
    }

    /// Display name of the project this agent belongs to.
    pub fn project_name(&self) -> String {
        self.project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| registry::tildify(&self.project))
    }

    /// Sort key *within* a project: agents needing you first, then errors.
    pub fn priority(&self) -> u8 {
        match self.session.status_str() {
            "waiting" => 0,
            "error" => 1,
            "idle" | "ready" => 2,
            _ => 3,
        }
    }
}

/// A rendered line: either a project heading or one of its agents.
pub enum DisplayRow {
    Project { name: String, path: String, count: usize },
    Agent(usize),
}

#[derive(Default)]
pub struct App {
    pub agents: Vec<Agent>,
    pub rows: Vec<DisplayRow>,
    transcripts: Transcripts,
    pub error: Option<String>,
}

impl App {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the agent list from disk.
    pub fn refresh(&mut self) {
        let sessions = match registry::load() {
            Ok(s) => {
                self.error = None;
                s
            }
            Err(e) => {
                self.error = Some(format!("registry: {e}"));
                return;
            }
        };

        let mut agents = Vec::with_capacity(sessions.len());
        let mut live_transcripts = Vec::with_capacity(sessions.len());

        for mut session in sessions {
            // Canonicalise once, so `cwd`, the project root and the worktree
            // parent are all directly comparable regardless of symlinks.
            session.cwd = git::canonical(&session.cwd);

            let transcript = registry::find_transcript(&session.session_id);
            let info = match &transcript {
                Some(p) => {
                    live_transcripts.push(p.clone());
                    self.transcripts.read(p)
                }
                None => Info::default(),
            };

            // `cwd` is authoritative — the process chdirs on relocation — so
            // resolving it through git covers every way an agent reaches a
            // worktree. A non-repo directory groups under itself.
            let project = git::main_repo_root(&session.cwd).unwrap_or_else(|| session.cwd.clone());

            let branch = info
                .worktree
                .as_ref()
                .and_then(|w| w.worktree_branch.clone())
                .or_else(|| git::branch(&session.cwd));

            agents.push(Agent { session, info, branch, transcript, project });
        }

        self.transcripts.retain(&live_transcripts);
        self.agents = agents;
        self.rebuild_rows();
    }

    /// Order agents by project, then lay out heading + member rows.
    ///
    /// Projects sort alphabetically by name, so a group keeps its place in the
    /// list as its agents change status. Within a project, agents wanting your
    /// attention come first.
    fn rebuild_rows(&mut self) {
        let mut order: Vec<usize> = (0..self.agents.len()).collect();
        order.sort_by(|&a, &b| {
            let (a, b) = (&self.agents[a], &self.agents[b]);
            a.project_name()
                .to_lowercase()
                .cmp(&b.project_name().to_lowercase())
                // Distinct projects can share a basename; keep them apart.
                .then_with(|| a.project.cmp(&b.project))
                .then_with(|| a.priority().cmp(&b.priority()))
                .then_with(|| a.session.display_name().cmp(b.session.display_name()))
        });

        let mut rows = Vec::with_capacity(order.len() + 4);
        let mut current: Option<&PathBuf> = None;
        for &i in &order {
            let agent = &self.agents[i];
            if current != Some(&agent.project) {
                rows.push(DisplayRow::Project {
                    name: agent.project_name(),
                    path: registry::tildify(&agent.project),
                    count: order.iter().filter(|&&j| self.agents[j].project == agent.project).count(),
                });
                current = Some(&agent.project);
            }
            rows.push(DisplayRow::Agent(i));
        }
        self.rows = rows;
    }

    pub fn is_selectable(&self, row: usize) -> bool {
        matches!(self.rows.get(row), Some(DisplayRow::Agent(_)))
    }

    pub fn agent_at(&self, row: usize) -> Option<&Agent> {
        match self.rows.get(row)? {
            DisplayRow::Agent(i) => self.agents.get(*i),
            DisplayRow::Project { .. } => None,
        }
    }

    /// First selectable row at or after `from`, searching in `delta`'s direction.
    pub fn seek_selectable(&self, from: usize, delta: isize) -> Option<usize> {
        let mut i = from as isize;
        while i >= 0 && (i as usize) < self.rows.len() {
            if self.is_selectable(i as usize) {
                return Some(i as usize);
            }
            i += delta;
        }
        None
    }

    pub fn first_selectable(&self) -> Option<usize> {
        self.seek_selectable(0, 1)
    }

    pub fn last_selectable(&self) -> Option<usize> {
        self.seek_selectable(self.rows.len().checked_sub(1)?, -1)
    }

    pub fn row_of_session(&self, session_id: &str) -> Option<usize> {
        (0..self.rows.len()).find(|&r| {
            self.agent_at(r).is_some_and(|a| a.session.session_id == session_id)
        })
    }

    /// Directories worth watching: the registry, plus each project dir holding
    /// a live transcript.
    pub fn watch_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![registry::sessions_dir()];
        for agent in &self.agents {
            if let Some(parent) = agent.transcript.as_ref().and_then(|p| p.parent()) {
                let parent = parent.to_path_buf();
                if !dirs.contains(&parent) {
                    dirs.push(parent);
                }
            }
        }
        dirs
    }
}
