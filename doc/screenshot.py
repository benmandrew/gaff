#!/usr/bin/env python3
"""Regenerate doc/screenshot.svg.

Builds a synthetic CLAUDE_CONFIG_DIR and a set of throwaway git repos, runs the
real gaff binary against them in a pty, then renders the resulting screen to SVG.
Going through the actual renderer means the screenshot cannot drift from what the
tool does; only the fixture below is invented.

    cargo build --release
    uv run --with pyte doc/screenshot.py

Text is positioned per colour-run at an explicit x, so alignment does not depend
on the viewer having any particular monospace font. Colours are presentation
attributes rather than CSS, because GitHub sanitises <style> out of SVGs.
"""
import fcntl
import html
import json
import os
import pty
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path

import pyte

REPO = Path(__file__).resolve().parent.parent
BIN = REPO / "target/release/gaff"
OUT = REPO / "doc/screenshot.svg"

# Sized so the table, detail pane and footer fit exactly, with no dead rows.
COLS, ROWS = 140, 19

# pyte reports the xterm defaults; remap those onto a calmer modern palette.
PALETTE = {
    "default": "#c5cad3",
    "7f7f7f": "#6b7387",  # DarkGray  — project paths, dimmed detail
    "0000ee": "#61afef",  # Blue      — plain directories
    "00cdcd": "#56b6c2",  # Cyan      — branches, and the name badge
    "cdcd00": "#e5c07b",  # Yellow    — busy
    "00cd00": "#98c379",  # Green     — waiting
    "cd0000": "#e06c75",  # Red       — error
    "cd00cd": "#c678dd",  # Magenta   — worktrees
    "ffffff": "#ffffff",  # White     — project names
    "000000": "#1b1e24",  # badge foreground
}
BG = "#1b1e24"
BG_MAP = {"282c3c": "#2b303b", "00cdcd": "#56b6c2"}
CW, LH, PAD_X, PAD_Y, TITLEBAR = 8.05, 19.0, 18.0, 14.0, 30.0

# (agent name, project, status, minutes in status, summary)
AGENTS = [
    ("cavalry-a1", "cavalry", "waiting", 6, "Add bool variables to the Hoare logic frontend"),
    ("cavalry-7c", "cavalry", "busy", 23, "Benchmark Z3 against the native solver path"),
    ("gaff-b6", "gaff", "busy", 5, "Render the README header as a terminal SVG"),
    ("site-db", "worktree", "idle", 14, "Centre tables and handle horizontal overflow"),
    ("site-f2", "site", "busy", 49, "Draft the floating-point geography article"),
]


def git(d, *args):
    subprocess.run(["git", "-C", str(d), *args], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def make_repo(path, branch):
    path.mkdir(parents=True, exist_ok=True)
    git(path, "init", "-q", "-b", branch)
    git(path, "config", "user.email", "demo@example.com")
    git(path, "config", "user.name", "demo")
    (path / "f").write_text("x")
    git(path, "add", ".")
    git(path, "commit", "-qm", "init")
    return path


def build_fixture(root):
    home = root / "home"
    cfg = home / ".claude"
    now = int(time.time() * 1000)

    projects = {
        "cavalry": make_repo(home / "projects/cavalry", "feat/bool-variables"),
        "gaff": make_repo(home / "projects/gaff", "main"),
        "site": make_repo(home / "projects/writing/site", "article/geography-of-floating-point"),
    }
    # A genuine linked worktree, so gaff resolves it back to `site` through .git
    # rather than being told about it.
    wt = projects["site"] / ".claude/worktrees/centred-tables"
    git(projects["site"], "worktree", "add", "-q", "-b", "worktree-centred-tables", str(wt))
    projects["worktree"] = wt

    (cfg / "sessions").mkdir(parents=True)
    (cfg / "projects").mkdir(parents=True)

    for i, (name, key, status, mins, title) in enumerate(AGENTS):
        cwd = projects[key]
        sid = f"{i:08d}-1d03-4f54-bbc6-bc56b953c37f"
        (cfg / "sessions" / f"{1000 + i}.json").write_text(json.dumps({
            # pid 1 is launchd: kill(1, 0) returns EPERM, which gaff counts as
            # alive, so the liveness check passes without a real process.
            "pid": 1, "sessionId": sid, "cwd": str(cwd),
            "startedAt": now - (mins + 30) * 60_000,
            "version": "2.1.219", "kind": "interactive", "entrypoint": "cli",
            "name": name, "nameSource": "derived", "status": status,
            "updatedAt": now, "statusUpdatedAt": now - mins * 60_000,
        }))
        slug = str(cwd).replace("/", "-").replace(".", "-")
        d = cfg / "projects" / slug
        d.mkdir(parents=True, exist_ok=True)
        (d / f"{sid}.jsonl").write_text(
            json.dumps({"type": "ai-title", "aiTitle": title, "sessionId": sid}) + "\n"
            + json.dumps({"type": "last-prompt", "lastPrompt": title, "sessionId": sid}) + "\n")

    return home, cfg


def capture(home, cfg):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.update(TERM="xterm-256color", COLUMNS=str(COLS), LINES=str(ROWS),
                          HOME=str(home), CLAUDE_CONFIG_DIR=str(cfg))
        os.execv(str(BIN), [str(BIN)])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    buf, end, quit_sent = b"", time.time() + 2.0, False
    while time.time() < end:
        if not quit_sent and time.time() > end - 1.0:
            os.write(fd, b"q")
            quit_sent = True
        if select.select([fd], [], [], 0.1)[0]:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            buf += chunk
    os.close(fd)

    screen = pyte.Screen(COLS, ROWS)
    pyte.Stream(screen).feed(buf.decode("utf-8", "replace"))
    return screen


def runs(screen, y):
    """Group one row into maximal runs sharing foreground, background and weight."""
    out, cur = [], None
    for x in range(COLS):
        cell = screen.buffer[y][x]
        key = (cell.fg, cell.bg, cell.bold)
        if cur and cur[0] == key and cur[2] == x:
            cur[1] += cell.data or " "
            cur[2] = x + 1
        else:
            if cur:
                out.append(cur)
            cur = [key, cell.data or " ", x + 1, x]
    if cur:
        out.append(cur)
    return out


def render(screen):
    w = COLS * CW + PAD_X * 2
    h = ROWS * LH + PAD_Y * 2 + TITLEBAR

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0f}" height="{h:.0f}" '
        f'viewBox="0 0 {w:.0f} {h:.0f}" font-family="ui-monospace,SFMono-Regular,'
        f'Menlo,Consolas,&quot;DejaVu Sans Mono&quot;,monospace" font-size="13">',
        f'<rect width="{w:.0f}" height="{h:.0f}" rx="8" fill="{BG}"/>',
    ]
    # Window chrome: the usual visual cue that this is a terminal.
    for i, colour in enumerate(("#e06c75", "#e5c07b", "#98c379")):
        parts.append(f'<circle cx="{PAD_X + 6 + i * 16:.0f}" cy="15" r="5.5" fill="{colour}"/>')

    for y in range(ROWS):
        baseline = TITLEBAR + PAD_Y + y * LH
        for (fg, bg, bold), text, _end, start in runs(screen, y):
            if not text.strip() and bg == "default":
                continue
            x = PAD_X + start * CW
            if bg != "default":
                parts.append(f'<rect x="{x:.1f}" y="{baseline - 13:.1f}" '
                             f'width="{len(text) * CW:.1f}" height="{LH:.1f}" '
                             f'fill="{BG_MAP.get(bg, "#2b303b")}"/>')
            if not text.strip():
                continue
            weight = ' font-weight="600"' if bold else ""
            parts.append(f'<text x="{x:.1f}" y="{baseline:.1f}" '
                         f'fill="{PALETTE.get(fg, PALETTE["default"])}"{weight} '
                         f'xml:space="preserve">{html.escape(text)}</text>')

    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def main():
    if not BIN.exists():
        sys.exit(f"{BIN} not found — run `cargo build --release` first")
    root = Path(tempfile.mkdtemp(prefix="gaff-demo-"))
    try:
        home, cfg = build_fixture(root)
        OUT.write_text(render(capture(home, cfg)))
        print(f"wrote {OUT.relative_to(REPO)} ({OUT.stat().st_size} bytes)")
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
