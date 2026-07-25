# gaff

gaff is a terminal user interface (TUI) listing every running Claude Code agent, grouped by the project it belongs to.

Running several agents at once makes it easy to lose track of which one is working, which one is blocked, and which directory each is in. gaff reads the command-line interface (CLI)'s own on-disk state and renders it as a single table.

```
 gaff   3 agents in 3 projects  3 busy
 NAME             STATUS    WHERE                 BRANCH                   DOING                                     FOR
 chezmoi                    ~/.local/share/chezmoi
   chezmoi-82     busy      —                     main                     Sync Claude cwd to terminal tabs          2m
 gaff                       ~/projects/gaff
▌  gaff-b6        busy      —                     main                     Build TUI for managing agents             10m
 site                       ~/projects/writing/site
   site-db        busy      ⑂ centred-tables      worktree-centred-tables  Center tables and handle overflow         38s
```

## Where the data comes from

Two locations, both internal to Claude Code and both undocumented:

- `~/.claude/sessions/<pid>.json` — one file per live process, holding its working directory, a derived name, a status, and a version. This is the *registry*.
- `~/.claude/projects/<slug>/<session-id>.jsonl` — the session *transcript*. Records of type `ai-title` carry a model-generated one-line summary of the work, which is what fills the `DOING` column.

Registry files are removed on graceful exit only, so a hard-killed agent leaves one behind. gaff verifies each process identifier (PID) with `kill(pid, 0)` and drops entries whose process is gone.

## Worktrees

The claude process genuinely `chdir`s when it relocates into a *worktree*, and the registry mirrors that faithfully. Comparing the registry against `lsof -a -p <pid> -d cwd` on three live agents gives an exact match, including one sitting in `~/projects/writing/site/.claude/worktrees/centred-tables`. The transcript also moves to a worktree-named project directory, and a `worktree-state` record preserves `originalCwd` and `originalBranch`.

That makes `cwd` authoritative, which is worth stating because the obvious guess is the opposite — that an agent launched in one directory and told to work in a worktree keeps reporting the directory it started in.

What `cwd` cannot answer is which project a worktree belongs to, so grouping resolves it through git. A linked worktree's `.git` is a file reading `gitdir: /main/repo/.git/worktrees/<name>`, and everything before the `.git` component is the repo it was cut from. One code path covers every way an agent reaches a worktree.

## Reading the columns

`STATUS` is the CLI's own value — `busy`, `idle`, `ready`, `waiting`, `running` or `error`. Unrecognised values render in grey rather than being hidden, since the set belongs to Claude Code and may grow.

`FOR` is time held in the current status, taken from `statusUpdatedAt`. For a `waiting` agent that is exactly how long it has been blocked on you, which is why waiting agents sort first within their project.

Projects themselves sort alphabetically rather than by urgency. A group that jumps position whenever one of its agents changes status is harder to find than one that stays put.

`WHERE` is position within the project: `⑂ name` for a worktree, a relative path for a subdirectory, `—` at the project root.

## Performance

The transcript corpus reaches hundreds of megabytes — 553 MB across 31 project directories on the development machine — and a single transcript can exceed 3.7 MB. Re-reading on every change is not viable, so two things keep it cheap.

On first sight of a transcript, gaff seeks to the last 512 KiB. The records it wants are rewritten throughout a session rather than written once, so the tail carries a current copy; a full scan happens only if the tail yields nothing. Afterwards it reads only bytes appended since the last offset, and prefilters lines by substring before paying for a JSON parse.

Refreshes are driven by filesystem events on the registry directory and on the project directories of live transcripts, debounced at 150 ms. A separate one-second tick redraws relative times without touching disk.

## Building

```
cargo build --release    # 608,624 bytes at target/release/gaff
cargo test               # 9 tests
```

The release profile trades speed for size — `opt-level = "z"`, fat link-time optimisation (LTO), one codegen unit, and `panic = "abort"` — which is the right trade when the work is a few file reads per second. That takes the binary from 1,054,048 bytes to 608,624.

Rebuilding the standard library removes a further 115,472 bytes, at the cost of needing a nightly toolchain, the `rust-src` component and an explicit target:

```
cargo build --release -Z build-std=std,panic_abort --target aarch64-apple-darwin
# 493,152 bytes
```

Adding `-Z build-std-features=panic_immediate_abort` on top would shrink it further, but it fails to compile `core` on the nightly tested (`1.99.0-nightly`, 2026-07-23).

## Usage

```
gaff            # the TUI
gaff --once     # print the table and exit
```

Keys: `j`/`k` or arrows to move, `g`/`G` for first and last, `r` to force a refresh, `q` to quit. Project headings are skipped when moving.

## Caveats

Every field beyond `pid` and `sessionId` is parsed as optional, so a schema change degrades the display rather than breaking the tool. That is the most that can be promised: this reads private state belonging to another program, and nothing obliges that program to keep its shape. The version each agent reports is shown in the detail pane, which is the first place to look when a column goes empty.
