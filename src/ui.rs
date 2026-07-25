//! Rendering. A table of live agents over a detail pane for the selection.

use crate::app::{App, DisplayRow};
use crate::registry;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;
use std::time::Duration;

/// Colour by status. Anything unrecognised renders in grey rather than being
/// hidden — the status set belongs to the CLI and may grow.
fn status_style(status: &str) -> Style {
    match status {
        // Blocked on you: the whole reason to keep this open.
        "waiting" => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        "error" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "busy" | "running" => Style::default().fg(Color::Yellow),
        "idle" | "ready" => Style::default().fg(Color::DarkGray),
        _ => Style::default().fg(Color::Gray),
    }
}

/// "4m", "2h10m", "3d" — compact enough for a narrow column.
pub fn humanize(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => {
            let (h, m) = (secs / 3600, (secs % 3600) / 60);
            if m == 0 { format!("{h}h") } else { format!("{h}h{m}m") }
        }
        _ => format!("{}d", secs / 86400),
    }
}

/// Trim to `width`, marking elision with an ellipsis.
fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let kept: String = s.chars().take(width - 1).collect();
    format!("{kept}…")
}

/// Path shortened from the left — the tail of a path carries the identity.
fn shorten_path(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let tail: String = chars[chars.len() - (width - 1)..].iter().collect();
    format!("…{tail}")
}

pub fn draw(f: &mut Frame, app: &App, state: &mut TableState, err: Option<&str>) {
    const DETAIL_H: u16 = 8;

    // Keep the table just tall enough for its rows so the detail pane sits
    // directly beneath it, rather than leaving a gap when few agents are up.
    let spare = f.area().height.saturating_sub(1 + DETAIL_H + 1);
    let needed = app.rows.len() as u16 + 1; // +1 for the column header
    let table_h = needed.clamp(2, spare.max(2));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),        // header
            Constraint::Length(table_h),  // table
            Constraint::Length(DETAIL_H), // detail
            Constraint::Min(0),           // slack, so the footer stays at the bottom
            Constraint::Length(1),        // footer
        ])
        .split(f.area());

    draw_header(f, chunks[0], app);
    draw_table(f, chunks[1], app, state);
    draw_detail(f, chunks[2], app, state);
    draw_footer(f, chunks[4], err);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let agents = &app.agents;
    let waiting = agents.iter().filter(|a| a.session.status_str() == "waiting").count();
    let busy = agents
        .iter()
        .filter(|a| matches!(a.session.status_str(), "busy" | "running"))
        .count();
    let projects = app.rows.iter().filter(|r| matches!(r, DisplayRow::Project { .. })).count();

    let mut spans = vec![
        Span::styled(" gaff ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(format!(
            "  {} agent{} in {} project{}",
            agents.len(),
            if agents.len() == 1 { "" } else { "s" },
            projects,
            if projects == 1 { "" } else { "s" }
        )),
    ];
    if busy > 0 {
        spans.push(Span::styled(format!("  {busy} busy"), status_style("busy")));
    }
    if waiting > 0 {
        spans.push(Span::styled(format!("  {waiting} waiting"), status_style("waiting")));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_table(f: &mut Frame, area: Rect, app: &App, state: &mut TableState) {
    if app.agents.is_empty() {
        let msg = Paragraph::new("\n  No Claude Code sessions running.")
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(msg, area);
        return;
    }

    // Widths for the flexible columns, derived from what the table leaves over.
    let inner = area.width.saturating_sub(2) as usize;
    let fixed = 16 + 9 + 22 + 7 + 5; // name, status, branch, age, gaps
    let flexible = inner.saturating_sub(fixed);
    let loc_w = (flexible * 2 / 5).clamp(10, 32);
    let sum_w = flexible.saturating_sub(loc_w).max(10);

    let header = Row::new(vec!["NAME", "STATUS", "WHERE", "BRANCH", "DOING", "FOR"])
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .rows
        .iter()
        .map(|row| match row {
            DisplayRow::Project { name, path, count } => {
                // A heading spanning the first cells; the rest stay empty so the
                // agent columns beneath it stay aligned.
                Row::new(vec![
                    Cell::from(Line::from(vec![
                        Span::styled(
                            truncate(name, 15),
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        ),
                        // A count is only worth the ink when there is more than one.
                        Span::styled(
                            if *count > 1 { format!(" ×{count}") } else { String::new() },
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])),
                    Cell::from(""),
                    Cell::from(shorten_path(path, loc_w + 24))
                        .style(Style::default().fg(Color::DarkGray)),
                ])
            }
            DisplayRow::Agent(i) => {
                let a = &app.agents[*i];
                let status = a.session.status_str();
                let summary = a
                    .info
                    .title
                    .clone()
                    .or_else(|| a.info.last_prompt.clone())
                    .unwrap_or_else(|| "—".into());

                // Time held in the current status — for a waiting agent, how
                // long it has been blocked on you.
                let age = a.status_age().map(humanize).unwrap_or_else(|| "—".into());

                Row::new(vec![
                    // Indented so members read as belonging to the heading above.
                    Cell::from(format!("  {}", truncate(a.session.display_name(), 14)))
                        .style(Style::default().add_modifier(Modifier::BOLD)),
                    Cell::from(truncate(status, 9)).style(status_style(status)),
                    Cell::from(shorten_path(&a.location(), loc_w))
                        .style(Style::default().fg(if a.is_worktree() { Color::Magenta } else { Color::Blue })),
                    Cell::from(truncate(a.branch.as_deref().unwrap_or("—"), 22))
                        .style(Style::default().fg(Color::Cyan)),
                    Cell::from(truncate(&summary, sum_w)),
                    Cell::from(age).style(status_style(status)),
                ])
            }
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(9),
            Constraint::Length(loc_w as u16),
            Constraint::Length(22),
            Constraint::Min(10),
            Constraint::Length(7),
        ],
    )
    .header(header)
    .row_highlight_style(Style::default().bg(Color::Rgb(40, 44, 60)).add_modifier(Modifier::BOLD))
    .highlight_symbol("▌");

    f.render_stateful_widget(table, area, state);
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App, state: &TableState) {
    let block = Block::default().borders(Borders::TOP).border_style(Style::default().fg(Color::DarkGray));

    let Some(agent) = state.selected().and_then(|i| app.agent_at(i)) else {
        f.render_widget(block, area);
        return;
    };

    let label = |s: &str| Span::styled(format!("{s:<10}"), Style::default().fg(Color::DarkGray));
    let mut lines = vec![Line::from(vec![
        label("cwd"),
        Span::styled(registry::tildify(&agent.session.cwd), Style::default().fg(Color::Blue)),
    ])];

    // Worktree lineage: where it came from, and what it branched off.
    if let Some(wt) = &agent.info.worktree {
        if let Some(orig) = &wt.original_cwd {
            lines.push(Line::from(vec![
                label("launched"),
                Span::styled(registry::tildify(orig), Style::default().fg(Color::Magenta)),
                Span::styled(
                    format!(
                        "  (off {})",
                        wt.original_branch.as_deref().unwrap_or("?")
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }

    lines.push(Line::from(vec![
        label("session"),
        Span::styled(agent.session.session_id.clone(), Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("   pid {}   v{}", agent.session.pid, agent.session.version.as_deref().unwrap_or("?")),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    let uptime = agent.uptime().map(humanize).unwrap_or_else(|| "?".into());
    let idle = agent.idle_for().map(humanize).unwrap_or_else(|| "?".into());
    lines.push(Line::from(vec![
        label("uptime"),
        Span::raw(uptime),
        Span::styled(
            format!(
                "   last wrote {idle} ago   {}   {}",
                agent.session.kind.as_deref().unwrap_or("?"),
                agent.session.entrypoint.as_deref().unwrap_or("?")
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    if let Some(prompt) = &agent.info.last_prompt {
        lines.push(Line::from(vec![label("prompt"), Span::raw(prompt.clone())]));
    }

    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: true }), area);
}

fn draw_footer(f: &mut Frame, area: Rect, err: Option<&str>) {
    let line = match err {
        Some(e) => Line::from(Span::styled(format!(" {e}"), Style::default().fg(Color::Red))),
        None => Line::from(Span::styled(
            " j/k move   r refresh   q quit",
            Style::default().fg(Color::DarkGray),
        )),
    };
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(secs: u64) -> String {
        humanize(Duration::from_secs(secs))
    }

    /// The `FOR` column is seven cells wide and is the number the list is read
    /// for. Each arm boundary is a place where the unit flips, so an off-by-one
    /// here shows a waiting agent as "0s" when it has been blocked a minute.
    #[test]
    fn humanize_switches_unit_at_each_boundary() {
        assert_eq!(h(0), "0s");
        assert_eq!(h(59), "59s");
        assert_eq!(h(60), "1m");
        assert_eq!(h(3599), "59m");
        assert_eq!(h(3600), "1h");
        assert_eq!(h(86399), "23h59m");
        assert_eq!(h(86400), "1d");
    }

    /// Whole hours drop the minutes so the common case stays short; anything
    /// else keeps them, because "2h" for two hours and fifty minutes is a lie
    /// the user acts on.
    #[test]
    fn humanize_keeps_minutes_only_when_they_are_nonzero() {
        assert_eq!(h(7200), "2h");
        assert_eq!(h(3660), "1h1m");
        assert_eq!(h(9000), "2h30m");
    }

    /// Past a day the count keeps growing rather than saturating — a session
    /// left up for a week should say so.
    #[test]
    fn humanize_counts_whole_days_beyond_the_first() {
        assert_eq!(h(90_000), "1d");
        assert_eq!(h(7 * 86400 + 3600), "7d");
    }

    /// Columns are fixed-width, so a cell wider than its budget pushes every
    /// column to its right out of alignment for the whole table.
    #[test]
    fn truncate_never_exceeds_its_width() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly-10", 10), "exactly-10", "a string at the limit is left alone");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("abcdefghij", 5).chars().count(), 5);
    }

    /// Truncation counts characters, not bytes: the tool renders `⑂` and `…`,
    /// and a byte-based slice would both mis-size the cell and panic on a
    /// boundary split.
    #[test]
    fn truncate_is_char_based_not_byte_based() {
        let s = "⑂ centred-tables";
        assert!(s.len() > s.chars().count(), "the fixture really is multi-byte");
        let got = truncate(s, 8);
        assert_eq!(got.chars().count(), 8);
        assert!(got.starts_with("⑂ "));
    }

    /// A one-cell column still has to render something; taking `width - 1`
    /// characters would underflow.
    #[test]
    fn truncate_degrades_to_an_ellipsis_in_a_tiny_column() {
        assert_eq!(truncate("abcdefghij", 1), "…");
        assert_eq!(truncate("abcdefghij", 0), "…");
    }

    /// Paths are elided from the left because the tail names the thing: two
    /// worktrees of the same repo differ only in their last component.
    #[test]
    fn shorten_path_keeps_the_tail() {
        assert_eq!(shorten_path("~/projects/gaff", 20), "~/projects/gaff");
        let got = shorten_path("~/projects/writing/site/.claude/worktrees/centred", 12);
        assert_eq!(got.chars().count(), 12);
        assert!(got.starts_with('…'), "elision is marked at the start");
        assert!(got.ends_with("centred"), "the identifying tail survives");
    }

    /// Same char-vs-byte hazard as `truncate`, and here the slice is an index
    /// arithmetic on a `Vec<char>`, so getting it wrong panics rather than
    /// merely mis-renders.
    #[test]
    fn shorten_path_is_char_based_and_survives_a_tiny_column() {
        let s = "~/projects/⑂ centré";
        assert!(s.len() > s.chars().count(), "the fixture really is multi-byte");
        assert_eq!(shorten_path(s, 7).chars().count(), 7);
        assert_eq!(shorten_path(s, 7), "…centré");
        assert_eq!(shorten_path(s, 1), "…");
        assert_eq!(shorten_path(s, 0), "…");
    }

    /// The status set belongs to Claude Code and may grow. A value we have
    /// never seen must still render — in grey, visibly present — rather than
    /// being dropped or panicking the whole table.
    #[test]
    fn unknown_status_still_gets_a_visible_style() {
        let unknown = status_style("teleporting");
        assert_eq!(unknown.fg, Some(Color::Gray));
        assert_ne!(unknown, status_style("waiting"), "and is not mistaken for one we know");
        assert_eq!(status_style("unknown"), unknown, "including the registry's own fallback");
    }

    /// The two statuses worth interrupting for must not blend into the rest.
    #[test]
    fn waiting_and_error_are_styled_distinctly() {
        assert_eq!(status_style("waiting").fg, Some(Color::Green));
        assert_eq!(status_style("error").fg, Some(Color::Red));
        assert_eq!(status_style("busy"), status_style("running"));
        assert_eq!(status_style("idle"), status_style("ready"));
    }
}
