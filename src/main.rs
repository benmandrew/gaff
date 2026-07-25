//! gaff — a TUI listing live Claude Code agents.
//!
//! Everything shown is read from Claude Code's own on-disk state:
//!   ~/.claude/sessions/<pid>.json   live process registry
//!   ~/.claude/projects/*/*.jsonl    session transcripts
//!
//! Both are internal to the CLI and undocumented, so parsing is deliberately
//! lenient: unknown fields are ignored and missing ones degrade the display.

mod app;
mod git;
mod registry;
mod transcript;
mod ui;

use anyhow::Result;
use app::App;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::TableState;
use ratatui::Terminal;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// Filesystem events arrive in bursts; collapse them into one refresh.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Redraw cadence for relative times ("4m ago"). Costs no I/O.
const TICK: Duration = Duration::from_secs(1);

enum Msg {
    Fs,
    Tick,
    Input(Event),
}

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--once") {
        return print_once();
    }

    let (tx, rx) = mpsc::channel();

    spawn_input_thread(tx.clone());
    spawn_tick_thread(tx.clone());

    let mut app = App::new();
    app.refresh();

    // Watcher must outlive the loop or its background thread is dropped.
    let mut watcher = build_watcher(tx.clone())?;
    let mut watched: HashSet<PathBuf> = HashSet::new();
    sync_watches(&mut watcher, &mut watched, &app);

    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal, &mut app, &rx, &mut watcher, &mut watched);
    restore_terminal(&mut terminal)?;
    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    rx: &Receiver<Msg>,
    watcher: &mut RecommendedWatcher,
    watched: &mut HashSet<PathBuf>,
) -> Result<()> {
    let mut table = TableState::default();
    table.select(app.first_selectable());

    let mut dirty = false;
    let mut last_refresh = Instant::now();
    let mut redraw = true;

    loop {
        if redraw {
            terminal.draw(|f| ui::draw(f, app, &mut table, app.error.as_deref()))?;
            redraw = false;
        }

        match rx.recv_timeout(DEBOUNCE) {
            Ok(Msg::Fs) => dirty = true,
            Ok(Msg::Tick) => redraw = true,
            Ok(Msg::Input(Event::Resize(_, _))) => redraw = true,
            Ok(Msg::Input(Event::Key(key))) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(()),
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                    (KeyCode::Char('r'), _) => dirty = true,
                    (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                        move_selection(&mut table, app, 1);
                        redraw = true;
                    }
                    (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                        move_selection(&mut table, app, -1);
                        redraw = true;
                    }
                    (KeyCode::Char('g'), _) | (KeyCode::Home, _) => {
                        table.select(app.first_selectable());
                        redraw = true;
                    }
                    (KeyCode::Char('G'), _) | (KeyCode::End, _) => {
                        table.select(app.last_selectable());
                        redraw = true;
                    }
                    _ => {}
                }
            }
            Ok(Msg::Input(_)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }

        if dirty && last_refresh.elapsed() >= DEBOUNCE {
            // Sorting can reorder rows under the cursor; keep the same agent selected.
            let anchor = table.selected().and_then(|i| app.agent_at(i)).map(|a| a.session.session_id.clone());

            app.refresh();
            sync_watches(watcher, watched, app);

            table.select(
                anchor
                    .as_deref()
                    .and_then(|id| app.row_of_session(id))
                    // The selected agent exited: fall back to the nearest row.
                    .or_else(|| {
                        let near = table.selected().unwrap_or(0).min(app.rows.len().saturating_sub(1));
                        app.seek_selectable(near, -1).or_else(|| app.first_selectable())
                    }),
            );

            dirty = false;
            last_refresh = Instant::now();
            redraw = true;
        }
    }
}

/// Print the agent list once and exit — composable with pipes, and the way to
/// sanity-check what the TUI is reading without a terminal.
fn print_once() -> Result<()> {
    let mut app = App::new();
    app.refresh();
    if let Some(err) = &app.error {
        eprintln!("warning: {err}");
    }

    for row in &app.rows {
        match row {
            app::DisplayRow::Project { path, count, .. } => println!("\n{path}  ({count})"),
            app::DisplayRow::Agent(i) => {
                let agent = &app.agents[*i];
                println!(
                    "  {:<14} {:<8} {:<4} {:<24} {:<26} {}",
                    agent.session.display_name(),
                    agent.session.status_str(),
                    agent.status_age().map(ui::humanize).unwrap_or_default(),
                    agent.location(),
                    agent.branch.as_deref().unwrap_or("—"),
                    agent.info.title.as_deref().unwrap_or("—"),
                );
            }
        }
    }
    Ok(())
}

/// Step the cursor by one agent, stepping over project headings rather than
/// letting them be selected.
fn move_selection(state: &mut TableState, app: &App, delta: isize) {
    let Some(current) = state.selected() else {
        state.select(app.first_selectable());
        return;
    };
    let next = current as isize + delta;
    if next < 0 || next as usize >= app.rows.len() {
        return;
    }
    // Keep the cursor where it is if there is no further agent this way.
    if let Some(target) = app.seek_selectable(next as usize, delta) {
        state.select(Some(target));
    }
}

fn build_watcher(tx: Sender<Msg>) -> Result<RecommendedWatcher> {
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            // A closed receiver just means we're shutting down.
            let _ = tx.send(Msg::Fs);
        }
    })?;
    Ok(watcher)
}

/// Watch the registry dir plus the project dirs of live transcripts, adding and
/// dropping watches as the session set changes.
fn sync_watches(watcher: &mut RecommendedWatcher, watched: &mut HashSet<PathBuf>, app: &App) {
    let desired: HashSet<PathBuf> = app.watch_dirs().into_iter().collect();

    for stale in watched.difference(&desired).cloned().collect::<Vec<_>>() {
        let _ = watcher.unwatch(&stale);
        watched.remove(&stale);
    }
    for new in desired.difference(watched).cloned().collect::<Vec<_>>() {
        // A dir that vanished between listing and watching is not an error.
        if watcher.watch(&new, RecursiveMode::NonRecursive).is_ok() {
            watched.insert(new);
        }
    }
}

fn spawn_input_thread(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });
}

fn spawn_tick_thread(tx: Sender<Msg>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        if tx.send(Msg::Tick).is_err() {
            break;
        }
    });
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
