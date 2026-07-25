//! Drives the real binary under a pty and asserts on what it renders.
//!
//! The TUI is the product; `--once` only proves the data behind it. Nothing
//! short of a terminal exercises the layout, the selection or the exit path, so
//! this forks a pty, runs gaff in it, and reads the screen back.
//!
//! Two things make that dependable. The output is fed through a small screen
//! model rather than searched as a byte stream: ratatui redraws by diff, so after
//! a keypress only the cells that changed are emitted, and a stream search would
//! be asserting on fragments. And every read is deadline-guarded, so a wedged
//! child fails the test instead of hanging `cargo test`.

mod common;

use common::*;
use std::ffi::CString;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

const ROWS: u16 = 50;
const COLS: u16 = 140;

const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";

/// Two agents in one project, one waiting and one busy, so the row order is
/// fixed by priority rather than by anything as incidental as name.
fn fixture() -> Fixture {
    let fx = Fixture::new();
    let repo = fx.repo("alpha", "main");
    fx.add_session(
        &SessionSpec::new(A, &repo).name("first").status("waiting").status_age_mins(4).started_mins_ago(30),
    );
    fx.add_session(
        &SessionSpec::new(B, &repo).name("second").status("busy").status_age_mins(2).started_mins_ago(9),
    );
    fx.add_transcript(&repo, A, &ai_title("Centre the tables"));
    fx.add_transcript(&repo, B, &ai_title("Benchmark the solver"));
    fx
}

/// The TUI has to show the agents at all — this is the whole tool, and the path
/// `--once` cannot reach: real terminal size, real layout, real widgets.
#[test]
fn tui_renders_the_agents_it_finds() {
    let fx = fixture();
    let mut pty = Pty::spawn(&fx);

    // The footer is the last row ratatui paints, so its arrival is the signal
    // that a whole frame has been read rather than half of one.
    pty.wait_for("a fully painted frame", |s| {
        s.contains("Centre the tables") && s.contains("Benchmark the solver") && s.contains("j/k move")
    });

    let screen = pty.screen.text();
    assert!(screen.contains("2 agents in 1 project"), "header summary missing:\n{screen}");
    assert!(screen.contains("alpha"), "project heading missing:\n{screen}");
    assert!(screen.contains("~/projects/alpha"), "project path missing:\n{screen}");
    // Labels that exist nowhere but the detail pane, so this really is the pane
    // for the selected agent and not a stray match in the table.
    assert!(screen.contains("uptime"), "detail pane missing:\n{screen}");
    assert!(screen.contains("session"), "detail pane missing:\n{screen}");

    pty.send(b"q");
    assert_eq!(pty.wait_for_exit(), 0);
}

/// Moving the cursor is how you read the detail pane for a particular agent.
/// The selection must land on agents and step over the project headings between
/// them, or half the keypresses in a busy list would appear to do nothing.
#[test]
fn j_and_k_move_the_selection_between_agents() {
    let fx = fixture();
    let mut pty = Pty::spawn(&fx);

    // The first agent is selected on startup, not the heading above it.
    pty.wait_for("initial selection on the first agent", |s| {
        s.contains("j/k move") && s.selected_row().contains("first")
    });

    pty.send(b"j");
    pty.wait_for("selection on the second agent", |s| s.selected_row().contains("second"));
    assert!(!pty.screen.selected_row().contains("first"), "two rows selected at once");

    pty.send(b"k");
    pty.wait_for("selection back on the first agent", |s| s.selected_row().contains("first"));

    // Nothing above the first agent is selectable, so this must be a no-op
    // rather than parking the cursor on the heading.
    pty.send(b"k");
    std::thread::sleep(Duration::from_millis(300));
    pty.drain();
    assert!(pty.screen.selected_row().contains("first"), "cursor left the first agent");

    pty.send(b"q");
    assert_eq!(pty.wait_for_exit(), 0);
}

/// gaff runs in the alternate screen with raw mode on. Quitting without putting
/// both back leaves the user's shell mangled — invisible cursor, no echo — which
/// is the kind of damage a terminal program is judged on.
#[test]
fn q_quits_and_restores_the_terminal() {
    let fx = fixture();
    let mut pty = Pty::spawn(&fx);
    pty.wait_for("first paint", |s| s.contains("j/k move"));

    pty.send(b"q");
    assert_eq!(pty.wait_for_exit(), 0, "quit must be a clean exit");

    let raw = String::from_utf8_lossy(&pty.raw).into_owned();
    assert!(raw.contains("\x1b[?1049h"), "never entered the alternate screen");
    assert!(raw.contains("\x1b[?1049l"), "left the alternate screen behind");
    assert!(raw.contains("\x1b[?25h"), "left the cursor hidden");
}

// ---------------------------------------------------------------------------
// pty plumbing
// ---------------------------------------------------------------------------

/// How long any single wait may take before the test gives up. Generous, because
/// it only ever elapses on failure; the success path returns as soon as the
/// expected text appears.
const DEADLINE: Duration = Duration::from_secs(10);

struct Pty {
    master: RawFd,
    pid: libc::pid_t,
    screen: Screen,
    /// Unprocessed bytes, kept so the escape sequences themselves can be
    /// asserted on — the screen model deliberately throws them away.
    raw: Vec<u8>,
    status: Option<i32>,
    eof: bool,
}

impl Pty {
    fn spawn(fx: &Fixture) -> Pty {
        let bin = CString::new(env!("CARGO_BIN_EXE_gaff")).unwrap();
        let argv = [bin.as_ptr(), std::ptr::null()];

        // Everything the child needs is allocated before the fork: between fork
        // and exec only async-signal-safe calls are legal, and this process is
        // multi-threaded (the test harness).
        let env: Vec<CString> = [
            format!("CLAUDE_CONFIG_DIR={}", fx.config.display()),
            format!("HOME={}", fx.home.display()),
            "TERM=xterm-256color".to_string(),
            format!("LINES={ROWS}"),
            format!("COLUMNS={COLS}"),
        ]
        .into_iter()
        .map(|s| CString::new(s).unwrap())
        .collect();
        let mut envp: Vec<*const libc::c_char> = env.iter().map(|s| s.as_ptr()).collect();
        envp.push(std::ptr::null());

        let mut ws =
            libc::winsize { ws_row: ROWS, ws_col: COLS, ws_xpixel: 0, ws_ypixel: 0 };
        let mut master: libc::c_int = -1;

        // SAFETY: the child branch execs immediately and touches nothing else;
        // the pointers handed to execve outlive the call.
        let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) };
        assert!(pid >= 0, "forkpty: {}", std::io::Error::last_os_error());
        if pid == 0 {
            unsafe {
                libc::execve(bin.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }

        // Size is already set through forkpty; doing it again on the master is
        // harmless and keeps the sizing explicit at the one place it matters —
        // a 24x80 default would truncate the columns these tests read.
        unsafe {
            libc::ioctl(master, libc::TIOCSWINSZ, &mut ws);
            let flags = libc::fcntl(master, libc::F_GETFL);
            libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        Pty {
            master,
            pid,
            screen: Screen::new(ROWS as usize, COLS as usize),
            raw: Vec::new(),
            status: None,
            eof: false,
        }
    }

    /// Read whatever is available, waiting at most `timeout` for the first byte.
    fn pump(&mut self, timeout: Duration) {
        // SAFETY: a zeroed fd_set is a valid empty set; both pointers are live.
        unsafe {
            let mut set: libc::fd_set = std::mem::zeroed();
            libc::FD_ZERO(&mut set);
            libc::FD_SET(self.master, &mut set);
            let mut tv = libc::timeval {
                tv_sec: timeout.as_secs() as _,
                tv_usec: timeout.subsec_micros() as _,
            };
            if libc::select(self.master + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv) <= 0
            {
                return;
            }

            let mut buf = [0u8; 65536];
            let n = libc::read(self.master, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
            match n {
                // The slave closes when the child exits; on macOS that surfaces
                // as EIO rather than a clean zero-length read.
                0 => self.eof = true,
                n if n < 0 => {
                    let err = std::io::Error::last_os_error().raw_os_error();
                    if err != Some(libc::EAGAIN) && err != Some(libc::EWOULDBLOCK) {
                        self.eof = true;
                    }
                }
                n => {
                    let bytes = &buf[..n as usize];
                    self.raw.extend_from_slice(bytes);
                    self.screen.feed(bytes);
                }
            }
        }
    }

    /// Consume anything already buffered without waiting.
    fn drain(&mut self) {
        for _ in 0..64 {
            let before = self.raw.len();
            self.pump(Duration::from_millis(0));
            if self.raw.len() == before {
                return;
            }
        }
    }

    /// Read until the screen satisfies `pred`, or fail with the screen attached.
    ///
    /// gaff redraws on a one-second tick, so the stream never falls silent and
    /// waiting for quiet would never return; this waits for content instead.
    fn wait_for(&mut self, what: &str, pred: impl Fn(&Screen) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            if pred(&self.screen) {
                return;
            }
            if self.eof {
                break;
            }
            self.pump(Duration::from_millis(50));
        }
        panic!(
            "timed out waiting for {what} ({} bytes read, eof={}); screen was:\n{}",
            self.raw.len(),
            self.eof,
            self.screen.text()
        );
    }

    fn send(&mut self, bytes: &[u8]) {
        // SAFETY: writing borrowed bytes to a fd we own.
        let n = unsafe { libc::write(self.master, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
        assert_eq!(n, bytes.len() as isize, "short write to pty");
    }

    /// Wait for the child to exit, returning its exit code. Keeps draining the
    /// pty meanwhile so the child can never block on a full buffer on its way out.
    fn wait_for_exit(&mut self) -> i32 {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            let mut status = 0;
            // SAFETY: reaping our own child.
            let rc = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if rc == self.pid {
                self.status = Some(status);
                return libc::WEXITSTATUS(status);
            }
            self.pump(Duration::from_millis(50));
        }
        panic!("gaff did not exit; screen was:\n{}", self.screen.text());
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // A failing assertion unwinds past `wait_for_exit`, so the child has to
        // be cleaned up here or a failed test leaks a process holding the fixture.
        if self.status.is_none() {
            // SAFETY: signalling and reaping our own child.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                // Polled rather than blocking: a drop that can hang is a drop
                // that can wedge the whole test run.
                if unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) } != 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        unsafe { libc::close(self.master) };
    }
}

// ---------------------------------------------------------------------------
// screen model
// ---------------------------------------------------------------------------

#[derive(PartialEq)]
enum Esc {
    Ground,
    Escape,
    Csi,
    /// A sequence whose next byte is a parameter we do not care about, such as
    /// a charset designation.
    SkipOne,
    Osc,
}

/// A character grid driven by the subset of ANSI that crossterm emits: absolute
/// cursor moves, relative moves, erases, and text. Styling is dropped — the
/// tests assert on what is legible, not on how it is coloured.
struct Screen {
    rows: usize,
    cols: usize,
    grid: Vec<Vec<char>>,
    cx: usize,
    cy: usize,
    state: Esc,
    params: String,
    /// A read can split a multi-byte character; hold the remainder for next time.
    pending: Vec<u8>,
}

impl Screen {
    fn new(rows: usize, cols: usize) -> Screen {
        Screen {
            rows,
            cols,
            grid: vec![vec![' '; cols]; rows],
            cx: 0,
            cy: 0,
            state: Esc::Ground,
            params: String::new(),
            pending: Vec::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let taken = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) => match e.error_len() {
                // Genuinely invalid, not merely truncated: skip the bad byte.
                Some(bad) => e.valid_up_to() + bad,
                None => e.valid_up_to(),
            },
        };
        let chunk: String = String::from_utf8_lossy(&self.pending[..taken]).into_owned();
        self.pending.drain(..taken);
        for c in chunk.chars() {
            self.put(c);
        }
    }

    fn put(&mut self, c: char) {
        match self.state {
            Esc::SkipOne => self.state = Esc::Ground,
            Esc::Osc => {
                if c == '\x07' || c == '\x1b' {
                    self.state = Esc::Ground;
                }
            }
            Esc::Escape => {
                self.state = match c {
                    '[' => {
                        self.params.clear();
                        Esc::Csi
                    }
                    ']' => Esc::Osc,
                    '(' | ')' | '#' | '%' => Esc::SkipOne,
                    _ => Esc::Ground,
                };
            }
            Esc::Csi => {
                if ('\x40'..='\x7e').contains(&c) {
                    let params = std::mem::take(&mut self.params);
                    self.csi(&params, c);
                    self.state = Esc::Ground;
                } else {
                    self.params.push(c);
                }
            }
            Esc::Ground => match c {
                '\x1b' => self.state = Esc::Escape,
                '\r' => self.cx = 0,
                '\n' => self.newline(),
                '\x08' => self.cx = self.cx.saturating_sub(1),
                '\t' => self.cx = ((self.cx / 8) + 1) * 8,
                c if (c as u32) < 0x20 => {}
                c => self.write(c),
            },
        }
    }

    fn csi(&mut self, params: &str, final_byte: char) {
        // `?`-prefixed sequences are private modes (cursor visibility, alternate
        // screen); none of them move the cursor or change the text.
        if params.starts_with('?') {
            return;
        }
        let n = |i: usize, default: usize| -> usize {
            params.split(';').nth(i).and_then(|p| p.parse().ok()).filter(|v| *v > 0).unwrap_or(default)
        };
        match final_byte {
            'H' | 'f' => {
                self.cy = (n(0, 1) - 1).min(self.rows - 1);
                self.cx = (n(1, 1) - 1).min(self.cols - 1);
            }
            'A' => self.cy = self.cy.saturating_sub(n(0, 1)),
            'B' => self.cy = (self.cy + n(0, 1)).min(self.rows - 1),
            'C' => self.cx = (self.cx + n(0, 1)).min(self.cols - 1),
            'D' => self.cx = self.cx.saturating_sub(n(0, 1)),
            'G' => self.cx = (n(0, 1) - 1).min(self.cols - 1),
            'd' => self.cy = (n(0, 1) - 1).min(self.rows - 1),
            'J' => {
                let mode = n(0, 0);
                let (from, to) = match mode {
                    0 => (self.cy, self.rows),
                    1 => (0, self.cy + 1),
                    _ => (0, self.rows),
                };
                for y in from..to {
                    self.grid[y] = vec![' '; self.cols];
                }
            }
            'K' => {
                let mode = n(0, 0);
                let (from, to) = match mode {
                    0 => (self.cx, self.cols),
                    1 => (0, self.cx + 1),
                    _ => (0, self.cols),
                };
                for x in from..to.min(self.cols) {
                    self.grid[self.cy][x] = ' ';
                }
            }
            _ => {}
        }
    }

    fn newline(&mut self) {
        if self.cy + 1 < self.rows {
            self.cy += 1;
        } else {
            self.grid.remove(0);
            self.grid.push(vec![' '; self.cols]);
        }
    }

    fn write(&mut self, c: char) {
        if self.cx >= self.cols {
            self.cx = 0;
            self.newline();
        }
        self.grid[self.cy][self.cx] = c;
        self.cx += 1;
    }

    fn text(&self) -> String {
        self.grid
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn contains(&self, needle: &str) -> bool {
        self.grid.iter().any(|row| row.iter().collect::<String>().contains(needle))
    }

    /// The selected row, identified by the highlight symbol ratatui draws in its
    /// first cell. Empty when nothing is selected.
    fn selected_row(&self) -> String {
        self.grid
            .iter()
            .map(|row| row.iter().collect::<String>())
            .find(|line| line.trim_start().starts_with('▌'))
            .unwrap_or_default()
    }
}

