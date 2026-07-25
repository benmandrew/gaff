# gaff

TUI listing running Claude Code agents, grouped by project. See @README.md for
what it shows and why.

## Constraints

- **The data sources are undocumented internal state.** `~/.claude/sessions/<pid>.json`
  and `~/.claude/projects/*/*.jsonl` belong to Claude Code, not to us. Parse
  leniently: every field beyond `pid`/`sessionId` is `Option`, unknown enum
  values render rather than panic or hide. Never assume a field exists because
  it did last week.
- **Never write to `~/.claude`.** gaff is strictly read-only over that tree.
- **The transcript corpus is huge** (~550 MB, individual files >3 MB). Any new
  transcript reading must stay incremental — see `transcript.rs`. Do not add a
  code path that parses a whole transcript per refresh.

## Layout

| File | Role |
| --- | --- |
| `registry.rs` | `~/.claude/sessions/*.json`, PID liveness, path helpers |
| `transcript.rs` | Incremental tail-follower; extracts `ai-title`, `last-prompt`, `worktree-state` |
| `git.rs` | Branch and main-repo resolution by reading `.git` directly, no subprocess |
| `app.rs` | Joins the three sources; builds the grouped `DisplayRow` list |
| `ui.rs` | ratatui rendering |
| `main.rs` | Event loop, fs watching, `--once` |

## Decisions

- **`cwd` is authoritative; the process really does `chdir`.** Verified against
  `lsof -a -p <pid> -d cwd` — registry and process cwd match exactly, including
  for a session relocated into a worktree. So worktree detection needs only
  `cwd` + git, and `worktree-state` is kept solely for `originalCwd` and
  `originalBranch`, which git cannot supply. Do not reintroduce a second code
  path keyed on `worktree-state` for location or grouping.
- **Grouping resolves through git.** A worktree's `.git` file names its parent
  repo's gitdir; the component before `.git` is the project. `cwd` alone cannot
  answer which project a worktree belongs to, which is why this indirection
  exists.
- **Projects sort alphabetically; urgency only orders agents within a project.**
  A stable, predictable group order matters more than floating the urgent group
  to the top — the list is glanced at repeatedly, and a project that moves as its
  agents change status is hard to find. Selection is anchored by `sessionId`
  across refreshes so any reordering does not move the cursor.
- **`FOR` uses `statusUpdatedAt`, not transcript mtime.** Time-in-status is the
  useful number; the registry heartbeat ticks regardless of activity. Transcript
  mtime is shown separately in the detail pane as "last wrote".
- **Offsets are line-aligned by construction.** `consume()` only discards a
  leading partial record when explicitly told the offset is unaligned, which is
  true solely for the initial blind seek into the tail. Discarding
  unconditionally silently swallows the first record of every incremental read —
  this was a real bug, covered by `later_records_supersede_earlier`.

- **The release profile optimises for size** (`opt-level = "z"`, fat LTO, one
  codegen unit, `panic = "abort"`): 608,624 bytes vs 1,054,048 at `opt-level = 2`.
  The workload is a few file reads per second, so speed is not the constraint.

## Testing

`cargo test`. The follower tests cover append, supersede, partial trailing line,
truncation and malformed input; `git.rs` builds a real repo with a real worktree.
There is no test harness for the TUI itself — verify rendering by driving the
binary in a pty.
