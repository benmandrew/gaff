//! End-to-end tests for `gaff --once` against a throwaway `CLAUDE_CONFIG_DIR`.
//!
//! `--once` prints exactly what the TUI would show, so driving it is the cheapest
//! way to test the whole chain — registry, transcript follower, git resolution,
//! grouping and ordering — as a user actually meets it.

mod common;

use common::*;

// Session ids are uuid-shaped because gaff falls back to their first 8 chars
// when a session has no name.
const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const C: &str = "cccccccc-0000-4000-8000-000000000003";

/// Registry files are swept on graceful exit only, so a hard-killed agent leaves
/// one behind. If liveness went unchecked, the list would slowly fill with ghosts
/// that no keypress can clear — the one failure that would make the tool useless.
#[test]
fn dead_processes_are_dropped_but_live_ones_are_kept() {
    let fx = Fixture::new();
    let repo = fx.repo("alpha", "main");

    fx.add_session(&SessionSpec::new(A, &repo).name("alive").status("busy"));
    fx.add_session(&SessionSpec::new(B, &repo).name("ghost").status("busy").pid(dead_pid()));

    let out = fx.run_once();
    assert!(agent_row(&out, "alive").is_some(), "live session missing:\n{out}");
    assert!(agent_row(&out, "ghost").is_none(), "stale session should be dropped:\n{out}");
    assert_eq!(headings(&out), vec!["~/projects/alpha  (1)"], "count must exclude the ghost");
}

/// The `DOING` column is the reason to keep gaff open at all: it is the only
/// place the work an agent is doing is visible without switching to its window.
#[test]
fn ai_title_from_the_transcript_reaches_the_output() {
    let fx = Fixture::new();
    let repo = fx.repo("alpha", "main");
    fx.add_session(&SessionSpec::new(A, &repo).name("worker").status("busy"));
    fx.add_transcript(&repo, A, &format!("{}{}", last_prompt("centre them"), ai_title("Centre the tables")));

    let out = fx.run_once();
    let row = agent_row(&out, "worker").expect("agent row");
    assert_eq!(title_of(row), "Centre the tables", "title missing from row: {row:?}");
}

/// An agent that relocated into a worktree still belongs to the project it was
/// cut from. Grouping it under itself would scatter one project's agents across
/// several headings, which is precisely the confusion gaff exists to remove.
#[test]
fn worktree_agent_groups_under_the_repo_it_was_cut_from() {
    let fx = Fixture::new();
    let repo = fx.repo("site", "main");
    let wt = add_worktree(&repo, &repo.join(".claude/worktrees/centred-tables"), "wt-centred-tables");

    fx.add_session(&SessionSpec::new(A, &repo).name("at-root").status("busy"));
    fx.add_session(&SessionSpec::new(B, &wt).name("in-tree").status("busy"));

    let out = fx.run_once();
    assert_eq!(headings(&out), vec!["~/projects/site  (2)"], "worktree must not head its own group");

    let row = agent_row(&out, "in-tree").expect("worktree agent row");
    assert_eq!(where_of(row), "⑂ centred-tables", "worktree location: {row:?}");
    assert_eq!(branch_of(row), "wt-centred-tables", "worktree branch: {row:?}");
}

/// `WHERE` answers "which of this project's directories is this agent in?".
/// A root agent and a subdirectory agent must be distinguishable at a glance,
/// or two agents in the same repo look identical.
#[test]
fn location_shows_position_within_the_project() {
    let fx = Fixture::new();
    let repo = fx.repo("alpha", "main");
    let sub = repo.join("crates/inner");
    std::fs::create_dir_all(&sub).unwrap();

    fx.add_session(&SessionSpec::new(A, &repo).name("at-root").status("busy"));
    fx.add_session(&SessionSpec::new(B, &sub).name("in-sub").status("busy"));

    let out = fx.run_once();
    let root = agent_row(&out, "at-root").expect("root agent");
    let inner = agent_row(&out, "in-sub").expect("subdir agent");
    assert_eq!(where_of(root), "—", "project root renders as a dash: {root:?}");
    assert_eq!(where_of(inner), "crates/inner", "subdirectory renders relative: {inner:?}");
}

/// One heading per project, carrying the number of agents under it. A second
/// heading for the same repo would double-count and split the group.
#[test]
fn agents_in_one_repo_share_a_single_heading() {
    let fx = Fixture::new();
    let repo = fx.repo("alpha", "main");
    fx.add_session(&SessionSpec::new(A, &repo).name("one").status("busy"));
    fx.add_session(&SessionSpec::new(B, &repo).name("two").status("busy"));

    let out = fx.run_once();
    assert_eq!(headings(&out), vec!["~/projects/alpha  (2)"]);
    assert_eq!(agent_rows(&out).len(), 2);
}

/// Projects hold a fixed alphabetical order while urgency only sorts agents
/// inside a project. A group that jumped to the top whenever one of its agents
/// started waiting would move under the cursor of anyone glancing at the list.
#[test]
fn projects_sort_alphabetically_and_urgency_only_orders_within_one() {
    let fx = Fixture::new();
    let alpha = fx.repo("alpha", "main");
    let zulu = fx.repo("zulu", "main");

    fx.add_session(&SessionSpec::new(A, &alpha).name("a-busy").status("busy"));
    // The waiting agent sits in the alphabetically *later* project, so if urgency
    // leaked into project order this would surface as zulu jumping ahead.
    fx.add_session(&SessionSpec::new(B, &zulu).name("z-busy").status("busy"));
    fx.add_session(&SessionSpec::new(C, &zulu).name("z-waiting").status("waiting"));

    let out = fx.run_once();
    assert_eq!(headings(&out), vec!["~/projects/alpha  (1)", "~/projects/zulu  (2)"]);

    let names: Vec<&str> = agent_rows(&out).iter().filter_map(|l| l.split_whitespace().next()).collect();
    assert_eq!(names, vec!["a-busy", "z-waiting", "z-busy"], "waiting sorts first within its project");
}

/// This reads another program's private state, which may be mid-write or may
/// change shape without notice. One unreadable file must never take down the
/// whole listing — the other agents are still there and still need showing.
#[test]
fn malformed_registry_file_does_not_hide_valid_sessions() {
    let fx = Fixture::new();
    let repo = fx.repo("alpha", "main");
    fx.write_registry_file("99999.json", "{\"pid\": 42, \"sessi");
    fx.write_registry_file("99998.json", "not json at all");
    fx.add_session(&SessionSpec::new(A, &repo).name("survivor").status("busy"));

    let out = fx.run_once();
    assert!(agent_row(&out, "survivor").is_some(), "valid session lost to a bad neighbour:\n{out}");
    assert_eq!(agent_rows(&out).len(), 1);
}

/// Every field beyond pid/sessionId/cwd is optional, so a schema change should
/// degrade a column rather than drop the agent. An agent that vanishes because
/// Claude Code renamed a field is worse than one with an empty column.
#[test]
fn session_missing_every_optional_field_still_lists() {
    let fx = Fixture::new();
    let repo = fx.repo("alpha", "main");
    fx.add_session(&SessionSpec::new(A, &repo));

    let out = fx.run_once();
    // With no name, gaff falls back to the leading chunk of the session id.
    let row = agent_row(&out, &A[..8]).expect("row for a bare session");
    assert_eq!(status_of(row), "unknown", "absent status falls back to `unknown`: {row:?}");
    assert_eq!(headings(&out), vec!["~/projects/alpha  (1)"]);
}

/// A session that has not yet written a transcript, or whose transcript has been
/// cleaned up, is still a running agent worth knowing about.
#[test]
fn session_without_a_transcript_still_lists() {
    let fx = Fixture::new();
    let repo = fx.repo("alpha", "main");
    fx.add_session(&SessionSpec::new(A, &repo).name("untold").status("idle"));

    let out = fx.run_once();
    let row = agent_row(&out, "untold").expect("row without a transcript");
    assert_eq!(status_of(row), "idle");
    assert_eq!(title_of(row), "—", "empty title renders as a dash: {row:?}");
}

/// Transcripts are appended to live, so a read can land on a half-written line,
/// and unknown record types appear as the format grows. Either must cost at most
/// the offending line, not the records around it.
#[test]
fn malformed_transcript_lines_do_not_lose_valid_records() {
    let fx = Fixture::new();
    let repo = fx.repo("alpha", "main");
    fx.add_session(&SessionSpec::new(A, &repo).name("worker").status("busy"));
    fx.add_transcript(
        &repo,
        A,
        &format!(
            "garbage\n{{\"type\":\"ai-title\" truncated\n{}{{\"type\":\"unknown-kind\"}}\n",
            ai_title("Survives the noise")
        ),
    );

    let out = fx.run_once();
    let row = agent_row(&out, "worker").expect("agent row");
    assert_eq!(title_of(row), "Survives the noise", "valid record lost: {row:?}");
}

/// Agents are routinely run outside a repository — a scratch directory, a home
/// directory, a freshly created folder. They group under themselves rather than
/// disappearing because git had no answer.
#[test]
fn agent_outside_a_git_repo_groups_under_its_own_directory() {
    let fx = Fixture::new();
    let scratch = fx.home.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    fx.add_session(&SessionSpec::new(A, &scratch).name("loose").status("busy"));

    let out = fx.run_once();
    assert_eq!(headings(&out), vec!["~/scratch  (1)"]);
    let row = agent_row(&out, "loose").expect("non-repo agent row");
    assert_eq!(where_of(row), "—", "its own directory is the project root: {row:?}");
}

/// On a machine that has never run Claude Code there is no `sessions/` directory
/// at all. That is "nothing running", not an error to report.
#[test]
fn absent_sessions_directory_is_a_clean_empty_run() {
    let fx = Fixture::bare();
    let out = fx.command().arg("--once").output().expect("run gaff --once");

    assert!(out.status.success(), "exit status: {}", out.status);
    assert!(out.stdout.is_empty(), "stdout: {:?}", String::from_utf8_lossy(&out.stdout));
    assert!(out.stderr.is_empty(), "stderr: {:?}", String::from_utf8_lossy(&out.stderr));
}

/// gaff is strictly read-only over `~/.claude`. That tree is Claude Code's live
/// state; a stray write could confuse or corrupt a running agent, and nothing in
/// gaff's job needs one.
#[test]
fn running_gaff_writes_nothing_into_the_config_dir() {
    let fx = Fixture::new();
    let repo = fx.repo("alpha", "main");
    fx.add_session(&SessionSpec::new(A, &repo).name("worker").status("busy").status_age_mins(3));
    fx.add_transcript(&repo, A, &ai_title("Leave no trace"));
    fx.write_registry_file("broken.json", "{oops");

    let before = snapshot(&fx.config);
    let out = fx.run_once();
    assert!(agent_row(&out, "worker").is_some(), "fixture should produce a row:\n{out}");

    assert_eq!(before, snapshot(&fx.config), "gaff modified its input tree");
}
