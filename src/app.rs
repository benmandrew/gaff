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
        SystemTime::now()
            .duration_since(self.info.last_activity?)
            .ok()
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
    Project {
        name: String,
        path: String,
        count: usize,
    },
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

            agents.push(Agent {
                session,
                info,
                branch,
                transcript,
                project,
            });
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
                    count: order
                        .iter()
                        .filter(|&&j| self.agents[j].project == agent.project)
                        .count(),
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
            self.agent_at(r)
                .is_some_and(|a| a.session.session_id == session_id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// An agent with just enough filled in to be sorted and rendered. Session
    /// ids are padded to eight chars so `display_name` is safe to call.
    fn agent(name: &str, status: &str, project: &Path) -> Agent {
        Agent {
            session: Session {
                pid: 1,
                session_id: format!("{name}-0123456789"),
                cwd: project.to_path_buf(),
                started_at: None,
                version: None,
                kind: None,
                entrypoint: None,
                name: Some(name.to_string()),
                status: Some(status.to_string()),
                status_updated_at: None,
            },
            info: Info::default(),
            branch: None,
            transcript: None,
            project: project.to_path_buf(),
        }
    }

    fn app_with(agents: Vec<Agent>) -> App {
        let mut app = App {
            agents,
            ..Default::default()
        };
        app.rebuild_rows();
        app
    }

    /// One line per row, headings prefixed, for asserting whole layouts at once.
    fn layout(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .map(|r| match r {
                DisplayRow::Project { name, count, .. } => format!("# {name} x{count}"),
                DisplayRow::Agent(i) => app.agents[*i].session.display_name().to_string(),
            })
            .collect()
    }

    fn git(dir: &Path, args: &[&str]) {
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

    /// Ordering within a project exists so the agents that need you are the
    /// ones you see first. An unrecognised status must sort last rather than
    /// jumping the queue ahead of a genuinely blocked session.
    #[test]
    fn agents_needing_attention_sort_ahead_of_working_ones() {
        let p = Path::new("/p");
        assert_eq!(agent("a", "waiting", p).priority(), 0);
        assert_eq!(agent("a", "error", p).priority(), 1);
        assert_eq!(agent("a", "idle", p).priority(), 2);
        assert_eq!(agent("a", "ready", p).priority(), 2);
        assert_eq!(agent("a", "busy", p).priority(), 3);
        assert_eq!(agent("a", "running", p).priority(), 3);
        assert_eq!(
            agent("a", "teleporting", p).priority(),
            3,
            "an unknown status waits its turn"
        );

        let mut unset = agent("a", "busy", p);
        unset.session.status = None;
        assert_eq!(unset.priority(), 3);
    }

    /// `WHERE` answers "where in this project is it?". At the root there is
    /// nothing to say, and a subdirectory should read relative to the heading
    /// above it rather than repeating the project path on every row.
    #[test]
    fn location_is_relative_to_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        // macOS /var is a symlink to /private/var; compare canonical paths.
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let sub = repo.join("src/ui");
        std::fs::create_dir_all(&sub).unwrap();

        let root = agent("a", "busy", &repo);
        assert!(!root.is_worktree());
        assert_eq!(root.location(), "—");

        let mut nested = agent("a", "busy", &repo);
        nested.session.cwd = sub;
        assert_eq!(nested.location(), "src/ui");
    }

    /// A directory outside its own project has no relative form, and silently
    /// rendering "—" would claim it sits at the root. Show the path whole.
    #[test]
    fn location_falls_back_to_the_whole_path_when_outside_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("project");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();

        let mut a = agent("a", "busy", &project);
        a.session.cwd = elsewhere.clone();
        let loc = a.location();
        assert_eq!(loc, registry::tildify(&elsewhere));
        assert!(loc.ends_with("elsewhere"), "shown whole, not as a fragment");
        assert_ne!(loc, "—");
    }

    /// A worktree is the one location worth calling out: it is the same project
    /// but not the same checkout, and mistaking the two costs a lost edit.
    #[test]
    fn location_marks_a_worktree_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f"), "x").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);

        let wt = root.join("centred-tables");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );

        // The project stays the main repo — that is the grouping key.
        let mut a = agent("a", "busy", &repo);
        a.session.cwd = wt;
        assert!(a.is_worktree());
        assert_eq!(a.worktree_name().as_deref(), Some("centred-tables"));
        assert_eq!(a.location(), "⑂ centred-tables");
    }

    /// The heading is the project's basename; a path with no basename (the
    /// filesystem root) still has to produce a heading rather than an empty one.
    #[test]
    fn project_name_is_the_basename_with_a_path_fallback() {
        assert_eq!(
            agent("a", "busy", Path::new("/home/me/projects/gaff")).project_name(),
            "gaff"
        );
        assert_eq!(
            agent("a", "busy", Path::new("/")).project_name(),
            registry::tildify(Path::new("/"))
        );
    }

    /// `FOR` is time held in the current status. When the registry omits the
    /// timestamp the transcript's mtime is the next best evidence of when
    /// something last happened — better than an empty column.
    #[test]
    fn status_age_prefers_the_registry_and_falls_back_to_the_transcript() {
        let mut a = agent("a", "waiting", Path::new("/p"));
        assert_eq!(a.status_age(), None, "nothing to go on yet");

        a.info.last_activity = Some(SystemTime::now() - Duration::from_secs(600));
        let fallback = a.status_age().expect("transcript mtime stands in");
        assert!((590..700).contains(&fallback.as_secs()), "got {fallback:?}");

        a.session.status_updated_at = Some(now_ms() - 5_000);
        let preferred = a.status_age().expect("registry timestamp wins");
        assert!(preferred.as_secs() < 60, "got {preferred:?}");
    }

    /// The agent and gaff read the clock independently, and the registry can
    /// hold a timestamp a little in the future. `duration_since` errors on that,
    /// so it must degrade to an empty cell rather than take the tool down.
    #[test]
    fn a_future_timestamp_empties_the_column_instead_of_panicking() {
        let mut a = agent("a", "waiting", Path::new("/p"));
        a.session.status_updated_at = Some(now_ms() + 3_600_000);
        assert_eq!(a.status_age(), None);

        a.info.last_activity = Some(SystemTime::now() + Duration::from_secs(3600));
        a.session.status_updated_at = None;
        assert_eq!(a.status_age(), None);
    }

    /// Same contract for uptime, but the millisecond value is an `i64` straight
    /// out of untrusted JSON: a negative or absurd number must not reach the
    /// arithmetic.
    #[test]
    fn uptime_rejects_timestamps_it_cannot_use() {
        let mut a = agent("a", "busy", Path::new("/p"));
        assert_eq!(a.uptime(), None, "no startedAt, no uptime");

        a.session.started_at = Some(now_ms() - 120_000);
        let up = a.uptime().expect("a real start time");
        assert!((110..200).contains(&up.as_secs()), "got {up:?}");

        a.session.started_at = Some(-1);
        assert_eq!(a.uptime(), None, "a negative epoch is not a start time");

        a.session.started_at = Some(i64::MAX);
        assert_eq!(a.uptime(), None, "nor is one far in the future");
    }

    /// Projects hold their place alphabetically so a group does not move as its
    /// agents change status — the list is glanced at repeatedly, and a heading
    /// that jumps is one you have to hunt for. Case must not split the order.
    #[test]
    fn projects_sort_alphabetically_ignoring_case() {
        // The urgent agent sits in the alphabetically *last* project, so a sort
        // that let urgency reach the project level would float Zebra to the top.
        // With every agent at the same status this test cannot tell the two
        // orderings apart.
        let app = app_with(vec![
            agent("a", "waiting", Path::new("/p/Zebra")),
            agent("b", "busy", Path::new("/p/apple")),
            agent("c", "idle", Path::new("/p/Mango")),
        ]);
        assert_eq!(
            layout(&app),
            ["# apple x1", "b", "# Mango x1", "c", "# Zebra x1", "a"]
        );
    }

    /// Two checkouts named `site` under different parents are different
    /// projects. Merging them under one heading would put an agent's rows
    /// beneath a path it has nothing to do with.
    #[test]
    fn projects_sharing_a_basename_stay_separate() {
        let app = app_with(vec![
            agent("second", "busy", Path::new("/y/foo")),
            agent("first", "busy", Path::new("/x/foo")),
        ]);
        assert_eq!(layout(&app), ["# foo x1", "first", "# foo x1", "second"]);

        let paths: Vec<_> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Project { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            paths,
            ["/x/foo", "/y/foo"],
            "the headings are told apart by path"
        );
    }

    /// Within a project, urgency orders the rows and the name breaks ties, so
    /// the same set of agents always renders in the same order between
    /// refreshes rather than shuffling under the cursor.
    #[test]
    fn agents_within_a_project_sort_by_urgency_then_name() {
        let p = Path::new("/p/one");
        let app = app_with(vec![
            agent("dave", "busy", p),
            agent("alice", "busy", p),
            agent("carol", "error", p),
            agent("bob", "waiting", p),
        ]);
        assert_eq!(layout(&app), ["# one x4", "bob", "carol", "alice", "dave"]);
    }

    /// One heading per project, carrying its own member count — the `×n` badge
    /// is how you see at a glance that a project has more agents than fit.
    #[test]
    fn each_project_gets_one_heading_carrying_its_count() {
        let app = app_with(vec![
            agent("a", "busy", Path::new("/p/one")),
            agent("b", "busy", Path::new("/p/two")),
            agent("c", "busy", Path::new("/p/one")),
            agent("d", "busy", Path::new("/p/one")),
        ]);
        assert_eq!(layout(&app), ["# one x3", "a", "c", "d", "# two x1", "b"]);
    }

    /// Headings are labels, not rows you can act on. Selecting one would leave
    /// the detail pane blank and make `j`/`k` feel like they had missed a step.
    #[test]
    fn headings_are_never_selectable() {
        let app = app_with(vec![
            agent("a", "busy", Path::new("/p/one")),
            agent("b", "busy", Path::new("/p/two")),
        ]);
        assert_eq!(layout(&app), ["# one x1", "a", "# two x1", "b"]);

        assert!(!app.is_selectable(0) && !app.is_selectable(2));
        assert!(app.is_selectable(1) && app.is_selectable(3));
        assert!(app.agent_at(0).is_none(), "a heading holds no agent");
        assert_eq!(app.agent_at(1).unwrap().session.display_name(), "a");
        assert!(app.agent_at(99).is_none(), "and off the end is not a panic");
    }

    /// Moving the cursor steps over headings in both directions, so `j` from
    /// the last agent of one project lands on the first agent of the next.
    #[test]
    fn seeking_skips_over_headings_in_both_directions() {
        let app = app_with(vec![
            agent("a", "busy", Path::new("/p/one")),
            agent("b", "busy", Path::new("/p/two")),
        ]);
        assert_eq!(app.first_selectable(), Some(1));
        assert_eq!(app.last_selectable(), Some(3));
        assert_eq!(
            app.seek_selectable(2, 1),
            Some(3),
            "forwards past a heading"
        );
        assert_eq!(
            app.seek_selectable(2, -1),
            Some(1),
            "backwards past a heading"
        );
        assert_eq!(app.seek_selectable(3, 1), Some(3), "already on an agent");
        assert_eq!(
            app.seek_selectable(0, -1),
            None,
            "nothing selectable above the first heading"
        );
    }

    /// Selection is anchored by session id so a refresh that reorders rows does
    /// not move the cursor onto a different agent.
    #[test]
    fn a_session_can_be_found_again_after_a_rebuild() {
        let app = app_with(vec![
            agent("a", "busy", Path::new("/p/one")),
            agent("b", "waiting", Path::new("/p/one")),
        ]);
        let row = app
            .row_of_session("a-0123456789")
            .expect("agent a has a row");
        assert_eq!(app.agent_at(row).unwrap().session.display_name(), "a");
        assert_eq!(
            app.row_of_session("gone-0123456789"),
            None,
            "a departed agent is simply absent"
        );
    }

    /// Nothing running is the normal state most of the day. Every accessor has
    /// to answer "nothing" — `last_selectable` in particular subtracts one from
    /// the row count.
    #[test]
    fn an_empty_app_answers_without_panicking() {
        let app = app_with(Vec::new());
        assert!(app.rows.is_empty());
        assert_eq!(app.first_selectable(), None);
        assert_eq!(app.last_selectable(), None);
        assert_eq!(app.seek_selectable(0, 1), None);
        assert_eq!(app.seek_selectable(0, -1), None);
        assert!(!app.is_selectable(0));
        assert!(app.agent_at(0).is_none());
        assert_eq!(app.row_of_session("anything"), None);
    }
}
