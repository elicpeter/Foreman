//! Grind dashboard. Renders a live view of [`crate::grind::GrindRunner`]
//! while it rotates through prompts, opens parallel worktrees, and burns
//! through run-level budgets.
//!
//! # Layout
//!
//! ```text
//! ┌── pitboss grind ─────────────────────────────────────────────────────┐
//! │ run <id> branch <branch>                                              │
//! │ plan <name>   agent <name>   model <id>                               │
//! ├── sessions (M / N) ────┬── agent output ─────────────────────────────┤
//! │ + 0001 prompt-a (12s)  │ [0001] Reading scratchpad.md                │
//! │ x 0002 prompt-b (8s)   │ [0001] Editing src/foo.rs                   │
//! │ > 0003 prompt-a *      │ [0003] tool: Edit                            │
//! ├────────────────────────┴─────────────────────────────────────────────┤
//! │ sessions 3/10  tokens 18.5k/100k  cost $0.12/$5.00  next: prompt-a    │
//! └───────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Manual run-it-yourself
//!
//! TUI behavior is verified by hand. To exercise the dashboard against a real
//! agent, run:
//!
//! ```bash
//! pitboss grind --tui
//! ```
//!
//! and against the dry-run agent (no spend):
//!
//! ```bash
//! PITBOSS_AGENT_BACKEND=dry-run pitboss grind --tui
//! ```
//!
//! Resize the terminal, hit `p` to toggle the output stream, hit `q` /
//! `Ctrl-C` / `a` to quit. The CI suite covers rendering helpers and a 50-event
//! smoke test; the panic-free guarantee under live conditions is the user's
//! responsibility to spot-check.
//!
//! # Subscribing to runner events
//!
//! [`run`] subscribes to the runner's broadcast channel before calling
//! [`crate::grind::GrindRunner::run`]. The receive loop folds each
//! [`GrindEvent`] into [`GrindApp`] state and re-renders. A lagging
//! subscriber sees [`broadcast::error::RecvError::Lagged`] and drops
//! intermediate events; the runner never blocks on a slow TUI.

use std::collections::{HashMap, VecDeque};
use std::io;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::broadcast;
use tokio::time::sleep;

use crate::agent::Agent;
use crate::git::Git;
use crate::grind::{
    BudgetWarningKind, GrindEvent, GrindRunOutcome, GrindRunner, GrindShutdown, GrindStopReason,
    PlanBudgets, SessionStatus,
};

use super::{TerminalGuard, TICK_INTERVAL};

/// Cap on the agent output buffer for the grind dashboard. Mirrors
/// [`super::OUTPUT_BUFFER_LINES`] but lives separately so the two views can
/// tune independently without churn in the play dashboard.
pub const GRIND_OUTPUT_BUFFER_LINES: usize = 1000;

/// Cap on the recent-session ring shown on the left pane. A run that lasts
/// hours can produce hundreds of sessions; the dashboard never needs to
/// scroll past the top of the visible pane, so we keep the most recent
/// entries and drop the rest.
pub const GRIND_SESSION_LOG_LINES: usize = 200;

/// Drive a [`GrindRunner`] with the grind dashboard attached.
///
/// Subscribes to the runner's [`GrindEvent`] stream, sets up the terminal in
/// alternate-screen / raw mode, and runs the input + render loop concurrently
/// with [`GrindRunner::run`]. Returns whatever the runner returned, or
/// `None` when the user quit before the runner finished. The terminal is
/// always restored before this function returns — including on early-return
/// errors and unwinding panics — so mouse capture never outlives the run.
pub async fn run<A, G>(
    runner: &mut GrindRunner<A, G>,
    shutdown: GrindShutdown,
) -> Result<Option<GrindRunOutcome>>
where
    A: Agent + Send + Sync + 'static,
    G: Git + Send + Sync + 'static,
{
    let app = GrindApp::from_runner(runner);
    let rx = runner.subscribe();

    let mut guard = TerminalGuard::setup().context("tui::grind: setting up terminal")?;

    let outcome = tokio::select! {
        biased;
        result = run_loop(guard.terminal(), app, rx) => {
            let _ = result?;
            Outcome::User
        }
        result = runner.run(shutdown.clone()) => Outcome::Runner(result?),
    };

    guard.restore().context("tui::grind: restoring terminal")?;

    match outcome {
        Outcome::Runner(out) => Ok(Some(out)),
        Outcome::User => {
            // User quit before the runner finished. Trip the runner's drain
            // signal so it exits cleanly on the next iteration; the caller
            // is responsible for awaiting the runner future after the TUI
            // returns when it cares about the final outcome. With `select!`
            // dropping the runner future this branch is cosmetic, but the
            // drain flip keeps the contract honest if a future caller
            // changes the structure.
            shutdown.drain();
            Ok(None)
        }
    }
}

enum Outcome {
    Runner(GrindRunOutcome),
    /// User-driven exit. The variant carries no payload because both
    /// [`UserOutcome::Quit`] and [`UserOutcome::ChannelClosed`] map to the
    /// same caller-visible "no outcome" return.
    User,
}

enum UserOutcome {
    /// User pressed q, a, or Ctrl-C.
    Quit,
    /// Runner dropped the broadcast channel (run completed via the other arm).
    /// Reported here only when this loop wins the race; in practice the
    /// runner arm wins and this is unreachable.
    ChannelClosed,
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: GrindApp,
    mut events: broadcast::Receiver<GrindEvent>,
) -> Result<UserOutcome> {
    let mut input = EventStream::new();
    terminal.draw(|f| app.render(f))?;

    loop {
        tokio::select! {
            biased;
            ev = events.recv() => {
                match ev {
                    Ok(event) => app.handle_event(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        terminal.draw(|f| app.render(f))?;
                        return Ok(UserOutcome::ChannelClosed);
                    }
                }
            }
            input_event = input.next() => {
                match input_event {
                    Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        if handle_key(&mut app, key.code, key.modifiers) {
                            return Ok(UserOutcome::Quit);
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(UserOutcome::Quit),
                }
            }
            _ = sleep(TICK_INTERVAL) => {}
        }

        terminal.draw(|f| app.render(f))?;
        if app.quit_requested() {
            return Ok(UserOutcome::Quit);
        }
    }
}

/// Returns `true` when the key requests an immediate quit.
fn handle_key(app: &mut GrindApp, code: KeyCode, mods: KeyModifiers) -> bool {
    match code {
        KeyCode::Char('q') | KeyCode::Char('a') => true,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => true,
        KeyCode::Char('p') => {
            app.toggle_pause();
            false
        }
        _ => false,
    }
}

/// In-flight or finished session row tracked by [`GrindApp`].
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// 1-based session sequence within the run.
    pub seq: u32,
    /// Prompt name dispatched in this session.
    pub prompt: String,
    /// `true` if the session took the parallel-worktree path.
    pub parallel_safe: bool,
    /// Wall-clock start time. `None` if the row was created from a finished
    /// record without a SessionStarted event preceding it (shouldn't happen
    /// in practice but the helper is defensive).
    pub started_at: Option<DateTime<Utc>>,
    /// Wall-clock end time, set once the session resolves.
    pub ended_at: Option<DateTime<Utc>>,
    /// Resolved status, or `None` while the session is in flight.
    pub status: Option<SessionStatus>,
    /// Tokens consumed by this session (`(input, output)`).
    pub tokens: (u64, u64),
    /// Cost charged to this session.
    pub cost_usd: f64,
}

impl SessionRow {
    /// `true` while the session has started but not yet finished.
    pub fn in_flight(&self) -> bool {
        self.status.is_none()
    }

    /// Wall-clock duration of this session (or how long it has been running),
    /// computed from `now`. Saturates to zero for a session that has not yet
    /// recorded a started_at.
    pub fn duration_secs(&self, now: DateTime<Utc>) -> i64 {
        let started = match self.started_at {
            Some(s) => s,
            None => return 0,
        };
        let end = self.ended_at.unwrap_or(now);
        (end - started).num_seconds().max(0)
    }
}

/// Terminal-side dashboard state for `pitboss grind`. Folds [`GrindEvent`]s
/// into a snapshot the [`GrindApp::render`] code path can render; the same
/// pure render is exercised by the unit tests at the bottom of this file.
pub struct GrindApp {
    run_id: String,
    branch: String,
    plan_name: String,
    agent_name: String,
    started_at: DateTime<Utc>,
    /// Plan-resolved run-level budgets (sessions / tokens / cost / until).
    budgets: PlanBudgets,
    /// Sessions in completion order, with the most recent at the back. Bounded
    /// at [`GRIND_SESSION_LOG_LINES`].
    sessions: VecDeque<SessionRow>,
    /// Quick lookup table from seq -> index in `sessions`. Lets event handlers
    /// update an in-flight row without rescanning the deque.
    session_index: HashMap<u32, usize>,
    /// Cumulative iteration / token / cost counters folded from
    /// [`GrindEvent::SessionFinished`].
    iterations: u32,
    tokens_input: u64,
    tokens_output: u64,
    cost_usd: f64,
    /// Most recent scheduler pick. Drives the footer's "next" hint.
    next_pick: Option<String>,
    /// Most recent budget warnings. Used to flash the relevant footer cell.
    warnings: Vec<BudgetWarningKind>,
    /// Resolved stop reason after [`GrindEvent::RunFinished`]; `None` while
    /// the run is still in flight.
    stop_reason: Option<GrindStopReason>,
    /// Bounded buffer of agent output lines. Each line is prefixed with a
    /// `[seq] ` tag so parallel sessions can be told apart.
    output: VecDeque<String>,
    /// Snapshot now-override for tests — production uses [`Utc::now`] so the
    /// elapsed cell ticks with the wall clock.
    now_override: Option<DateTime<Utc>>,
    /// Toggled by `p` — drops new agent output lines while paused.
    paused: bool,
    /// Set once the user requests quit (q / a / Ctrl-C).
    quit_requested: bool,
}

impl GrindApp {
    /// Build a fresh dashboard from a runner. Captures the run id, branch,
    /// plan name, agent display name, started_at, and the resolved budgets;
    /// no events are folded yet.
    pub fn from_runner<A: Agent + 'static, G: Git + 'static>(
        runner: &GrindRunner<A, G>,
    ) -> Self {
        Self::new(
            runner.run_id().to_string(),
            runner.branch().to_string(),
            runner.plan().name.clone(),
            runner.agent().name().to_string(),
            runner.started_at(),
            runner.budgets().clone(),
        )
    }

    /// Build a dashboard from raw inputs. Useful in tests where wiring up a
    /// full [`GrindRunner`] is overkill.
    pub fn new(
        run_id: String,
        branch: String,
        plan_name: String,
        agent_name: String,
        started_at: DateTime<Utc>,
        budgets: PlanBudgets,
    ) -> Self {
        Self {
            run_id,
            branch,
            plan_name,
            agent_name,
            started_at,
            budgets,
            sessions: VecDeque::with_capacity(GRIND_SESSION_LOG_LINES),
            session_index: HashMap::new(),
            iterations: 0,
            tokens_input: 0,
            tokens_output: 0,
            cost_usd: 0.0,
            next_pick: None,
            warnings: Vec::new(),
            stop_reason: None,
            output: VecDeque::with_capacity(GRIND_OUTPUT_BUFFER_LINES),
            now_override: None,
            paused: false,
            quit_requested: false,
        }
    }

    /// Test-only "now" override for snapshot determinism. Production calls
    /// [`Utc::now`] directly so the footer's elapsed-time cell ticks with the
    /// wall clock.
    #[cfg(test)]
    pub fn set_now(&mut self, now: DateTime<Utc>) {
        self.now_override = Some(now);
    }

    /// `true` once the user has requested quit. The host loop reads this on
    /// every tick.
    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    /// Force-set quit. Idempotent.
    pub fn request_quit(&mut self) {
        self.quit_requested = true;
    }

    /// Toggle output-pause. While paused, `Agent*` events are dropped instead
    /// of appended to the output buffer so the user can read what's on screen.
    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
    }

    /// `true` while the output stream is paused.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Number of session rows currently tracked.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Borrow the most recent scheduler pick (`None` if the scheduler hasn't
    /// fired yet or returned `None`).
    pub fn next_pick(&self) -> Option<&str> {
        self.next_pick.as_deref()
    }

    /// Borrow the resolved stop reason once the run finishes; `None` while
    /// the run is still active.
    pub fn stop_reason(&self) -> Option<&GrindStopReason> {
        self.stop_reason.as_ref()
    }

    /// Snapshot of the agent output buffer, oldest first. Exposed for tests.
    pub fn output_lines(&self) -> impl Iterator<Item = &String> {
        self.output.iter()
    }

    /// Borrow the session ring, oldest first.
    pub fn sessions(&self) -> impl Iterator<Item = &SessionRow> {
        self.sessions.iter()
    }

    /// Fold a [`GrindEvent`] into the dashboard state.
    pub fn handle_event(&mut self, event: GrindEvent) {
        match event {
            GrindEvent::SessionStarted {
                seq,
                prompt,
                parallel_safe,
            } => {
                let now = self.now();
                let row = SessionRow {
                    seq,
                    prompt,
                    parallel_safe,
                    started_at: Some(now),
                    ended_at: None,
                    status: None,
                    tokens: (0, 0),
                    cost_usd: 0.0,
                };
                self.push_session(row);
            }
            GrindEvent::AgentStdout { seq, line } => {
                if !self.paused {
                    self.push_output(format!("[{seq:04}] {line}"));
                }
            }
            GrindEvent::AgentStderr { seq, line } => {
                if !self.paused {
                    self.push_output(format!("[{seq:04}] err: {line}"));
                }
            }
            GrindEvent::AgentToolUse { seq, name } => {
                if !self.paused {
                    self.push_output(format!("[{seq:04}] tool: {name}"));
                }
            }
            GrindEvent::HookFired {
                seq,
                kind,
                success,
                description,
            } => {
                let label = if success { "ok" } else { "fail" };
                self.push_output(format!(
                    "[{seq:04}] hook {} {} ({})",
                    kind.label(),
                    label,
                    description
                ));
            }
            GrindEvent::SummaryCaptured { seq, summary } => {
                let one_line = summary.lines().next().unwrap_or(summary.as_str());
                self.push_output(format!("[{seq:04}] summary: {one_line}"));
            }
            GrindEvent::SessionFinished { record } => {
                self.iterations = self.iterations.saturating_add(1);
                self.tokens_input = self.tokens_input.saturating_add(record.tokens.input);
                self.tokens_output = self.tokens_output.saturating_add(record.tokens.output);
                self.cost_usd += record.cost_usd;
                let seq = record.seq;
                let status = record.status;
                let ended_at = record.ended_at;
                let started_at = record.started_at;
                let tokens = (record.tokens.input, record.tokens.output);
                let cost = record.cost_usd;
                let prompt = record.prompt.clone();
                let parallel_safe = self
                    .session_index
                    .get(&seq)
                    .and_then(|i| self.sessions.get(*i))
                    .map(|r| r.parallel_safe)
                    .unwrap_or(false);
                if let Some(idx) = self.session_index.get(&seq).copied() {
                    if let Some(row) = self.sessions.get_mut(idx) {
                        row.status = Some(status);
                        row.ended_at = Some(ended_at);
                        row.tokens = tokens;
                        row.cost_usd = cost;
                    }
                } else {
                    // No prior SessionStarted (shouldn't happen but be
                    // defensive: fold the row from the record alone).
                    let row = SessionRow {
                        seq,
                        prompt,
                        parallel_safe,
                        started_at: Some(started_at),
                        ended_at: Some(ended_at),
                        status: Some(status),
                        tokens,
                        cost_usd: cost,
                    };
                    self.push_session(row);
                }
            }
            GrindEvent::BudgetWarning { kind } => {
                self.warnings.push(kind);
                self.push_output(format!("[budget] warn: {}", format_warning(&kind)));
            }
            GrindEvent::SchedulerPicked { pick, .. } => {
                self.next_pick = pick;
            }
            GrindEvent::RunFinished { stop_reason } => {
                self.stop_reason = Some(stop_reason);
            }
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.now_override.unwrap_or_else(Utc::now)
    }

    fn push_session(&mut self, row: SessionRow) {
        // If the bounded ring is full, evict the oldest row and recompute the
        // index map. The eviction rate is at most "one record per session"
        // and the cap is small enough that the rebuild cost is negligible.
        if self.sessions.len() == GRIND_SESSION_LOG_LINES {
            if let Some(old) = self.sessions.pop_front() {
                self.session_index.remove(&old.seq);
            }
            // Re-key remaining rows to their new positions.
            self.session_index.clear();
            for (i, r) in self.sessions.iter().enumerate() {
                self.session_index.insert(r.seq, i);
            }
        }
        self.session_index.insert(row.seq, self.sessions.len());
        self.sessions.push_back(row);
    }

    fn push_output(&mut self, line: String) {
        if self.output.len() == GRIND_OUTPUT_BUFFER_LINES {
            self.output.pop_front();
        }
        self.output.push_back(line);
    }

    /// Render the entire dashboard. Pure on `&self` so the same code path
    /// drives the live terminal and the snapshot tests.
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(2),
                Constraint::Length(1),
            ])
            .split(area);
        self.render_header(frame, layout[0]);
        self.render_body(frame, layout[1]);
        self.render_footer(frame, layout[2]);
        self.render_keybar(frame, layout[3]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let line1 = Line::from(vec![
            Span::styled(
                "pitboss grind",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("run {}", self.run_id),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("  "),
            Span::styled(
                format!("branch {}", self.branch),
                Style::default().fg(Color::Magenta),
            ),
        ]);
        let line2 = Line::from(vec![
            Span::styled("plan ", Style::default().fg(Color::Gray)),
            Span::styled(
                self.plan_name.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled("agent ", Style::default().fg(Color::Gray)),
            Span::styled(
                self.agent_name.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                format!("elapsed {}", format_elapsed(self.now() - self.started_at)),
                Style::default().fg(Color::Cyan),
            ),
        ]);
        let block = Block::default().borders(Borders::BOTTOM);
        let para = Paragraph::new(vec![line1, line2]).block(block);
        frame.render_widget(para, area);
    }

    fn render_body(&self, frame: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);
        self.render_sessions(frame, cols[0]);
        self.render_output(frame, cols[1]);
    }

    fn render_sessions(&self, frame: &mut Frame, area: Rect) {
        let inner_height = area.height.saturating_sub(2) as usize;
        let take = inner_height.max(1);
        // Show the most recent rows, oldest at the top so the active sessions
        // fall to the bottom — same scrolling convention as the play view.
        let start = self.sessions.len().saturating_sub(take);
        let now = self.now();
        let items: Vec<ListItem> = self
            .sessions
            .iter()
            .skip(start)
            .map(|row| ListItem::new(format_session_row(row, now)))
            .collect();
        let in_flight = self.sessions.iter().filter(|r| r.in_flight()).count();
        let border_style = if in_flight > 0 {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let title = format!(
            " sessions ({} total{}) ",
            self.sessions.len(),
            if in_flight > 0 {
                format!(", {in_flight} active")
            } else {
                String::new()
            }
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(title, Style::default().fg(Color::Gray)));
        let list = List::new(items).block(block);
        frame.render_widget(list, area);
    }

    fn render_output(&self, frame: &mut Frame, area: Rect) {
        let inner_height = area.height.saturating_sub(2) as usize;
        let inner_width = area.width.saturating_sub(2);
        let take = inner_height.max(1);
        let start = self.output.len().saturating_sub(take);
        let lines: Vec<Line> = self
            .output
            .iter()
            .skip(start)
            .map(|s| style_output_line(s))
            .collect();
        let (title_str, title_style) = if self.paused {
            (
                " agent output [paused] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (" agent output ", Style::default().fg(Color::Gray))
        };
        let border_style = Style::default().fg(Color::DarkGray);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(title_str, title_style));
        let para = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        let total_with_borders = para.line_count(inner_width);
        let content_rows = total_with_borders.saturating_sub(2);
        let scroll_y = u16::try_from(content_rows.saturating_sub(inner_height)).unwrap_or(u16::MAX);
        let para = para.scroll((scroll_y, 0));
        frame.render_widget(para, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let label = Style::default().fg(Color::Gray);
        let value = Style::default().fg(Color::White);

        // Sessions cell.
        let sessions_text = match self.budgets.max_iterations {
            Some(cap) => format!("{}/{}", self.iterations, cap),
            None => format!("{}", self.iterations),
        };
        let sessions_style = warning_style_if(
            value,
            self.warnings
                .iter()
                .any(|w| matches!(w, BudgetWarningKind::Iterations { .. })),
        );

        // Tokens cell.
        let tokens_total = self.tokens_input.saturating_add(self.tokens_output);
        let tokens_text = match self.budgets.max_tokens {
            Some(cap) => format!("{}/{}", format_tokens(tokens_total), format_tokens(cap)),
            None => format_tokens(tokens_total),
        };
        let tokens_style = warning_style_if(
            value,
            self.warnings
                .iter()
                .any(|w| matches!(w, BudgetWarningKind::Tokens { .. })),
        );

        // Cost cell.
        let cost_text = match self.budgets.max_cost_usd {
            Some(cap) => format!("{}/{}", format_usd(self.cost_usd), format_usd(cap)),
            None => format_usd(self.cost_usd),
        };
        let cost_style = warning_style_if(
            Style::default().fg(Color::Green),
            self.warnings
                .iter()
                .any(|w| matches!(w, BudgetWarningKind::Cost { .. })),
        );

        // Until cell.
        let until_text = match self.budgets.until {
            Some(until) => {
                let remaining = until - self.now();
                if remaining.num_seconds() <= 0 {
                    "0s".to_string()
                } else {
                    format_elapsed(remaining)
                }
            }
            None => "—".to_string(),
        };
        let until_style = warning_style_if(
            value,
            self.warnings
                .iter()
                .any(|w| matches!(w, BudgetWarningKind::Until { .. })),
        );

        let next_text = self
            .next_pick
            .clone()
            .unwrap_or_else(|| "(none)".to_string());

        let line1 = Line::from(vec![
            Span::styled("sessions ", label),
            Span::styled(sessions_text, sessions_style),
            Span::raw("   "),
            Span::styled("tokens ", label),
            Span::styled(tokens_text, tokens_style),
            Span::raw("   "),
            Span::styled("cost ", label),
            Span::styled(cost_text, cost_style),
            Span::raw("   "),
            Span::styled("until ", label),
            Span::styled(until_text, until_style),
        ]);

        let line2 = match &self.stop_reason {
            None => Line::from(vec![
                Span::styled("next ", label),
                Span::styled(next_text, Style::default().fg(Color::Cyan)),
            ]),
            Some(reason) => {
                let (text, color) = stop_reason_display(reason);
                Line::from(vec![
                    Span::styled("status ", label),
                    Span::styled(
                        text,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ])
            }
        };

        let para = Paragraph::new(vec![line1, line2]);
        frame.render_widget(para, area);
    }

    fn render_keybar(&self, frame: &mut Frame, area: Rect) {
        let pause_label = if self.paused { "resume" } else { "pause" };
        let key_style = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        let hint_style = Style::default().fg(Color::Gray);
        let line = Line::from(vec![
            Span::styled("q", key_style),
            Span::styled(" quit", hint_style),
            Span::raw("   "),
            Span::styled("p", key_style),
            Span::styled(format!(" {pause_label}"), hint_style),
            Span::raw("   "),
            Span::styled("a", key_style),
            Span::styled(" abort", hint_style),
        ])
        .alignment(Alignment::Left);
        let para = Paragraph::new(line);
        frame.render_widget(para, area);
    }
}

/// Format a single session row for the left pane. Pure helper so the unit
/// tests can pin the formatting independently of any IO. Handles in-flight
/// (`status: None`), resolved Ok / Error / Timeout / Aborted / Dirty, and
/// the parallel-safe marker (`*` suffix).
pub fn format_session_row(row: &SessionRow, now: DateTime<Utc>) -> Line<'static> {
    let glyph = match row.status {
        None => ">",
        Some(SessionStatus::Ok) => "+",
        Some(SessionStatus::Error) => "x",
        Some(SessionStatus::Timeout) => "t",
        Some(SessionStatus::Aborted) => "a",
        Some(SessionStatus::Dirty) => "~",
    };
    let glyph_style = match row.status {
        None => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        Some(SessionStatus::Ok) => Style::default().fg(Color::Green),
        Some(SessionStatus::Error) => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Some(SessionStatus::Timeout) => Style::default().fg(Color::Yellow),
        Some(SessionStatus::Aborted) => Style::default().fg(Color::Yellow),
        Some(SessionStatus::Dirty) => Style::default().fg(Color::Yellow),
    };
    let id_style = match row.status {
        None => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        Some(_) => Style::default().fg(Color::White),
    };
    let prompt_style = match row.status {
        None => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        Some(SessionStatus::Ok) => Style::default().fg(Color::Gray),
        Some(SessionStatus::Error) => Style::default().fg(Color::Red),
        Some(SessionStatus::Timeout) => Style::default().fg(Color::Yellow),
        Some(SessionStatus::Aborted) => Style::default().fg(Color::Yellow),
        Some(SessionStatus::Dirty) => Style::default().fg(Color::Yellow),
    };
    let parallel_marker = if row.parallel_safe { " *" } else { "" };
    let secs = row.duration_secs(now);
    let tail = format!("  {}s{}", secs, parallel_marker);
    Line::from(vec![
        Span::styled(format!("{glyph} "), glyph_style),
        Span::styled(format!("{:04} ", row.seq), id_style),
        Span::styled(row.prompt.clone(), prompt_style),
        Span::styled(tail, Style::default().fg(Color::DarkGray)),
    ])
}

fn style_output_line(s: &str) -> Line<'static> {
    if s.starts_with("[budget]") {
        Line::from(Span::styled(
            s.to_owned(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
    } else if let Some(rest) = strip_seq_prefix(s) {
        if rest.starts_with("err: ") {
            Line::from(Span::styled(s.to_owned(), Style::default().fg(Color::Red)))
        } else if rest.starts_with("tool: ") {
            Line::from(Span::styled(
                s.to_owned(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM),
            ))
        } else if rest.starts_with("hook ") {
            Line::from(Span::styled(s.to_owned(), Style::default().fg(Color::Cyan)))
        } else if rest.starts_with("summary: ") {
            Line::from(Span::styled(
                s.to_owned(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(Span::styled(
                s.to_owned(),
                Style::default().fg(Color::White),
            ))
        }
    } else {
        Line::from(Span::styled(
            s.to_owned(),
            Style::default().fg(Color::White),
        ))
    }
}

/// Strip a leading `[NNNN] ` session prefix and return the remainder. `None`
/// when the line has no such prefix.
fn strip_seq_prefix(s: &str) -> Option<&str> {
    if !s.starts_with('[') {
        return None;
    }
    let close = s.find(']')?;
    let prefix = &s[1..close];
    if prefix.len() == 4 && prefix.bytes().all(|b| b.is_ascii_digit()) {
        s.get(close + 2..)
    } else {
        None
    }
}

/// Compute the percent of a budget consumed, clamped to `[0, 100]`. Pure
/// helper exported for tests; used internally to render the warning flash.
pub fn budget_percent(used: u64, cap: u64) -> u8 {
    if cap == 0 {
        return 0;
    }
    let pct = (used as f64 / cap as f64 * 100.0).round();
    if pct > 100.0 {
        100
    } else if pct < 0.0 {
        0
    } else {
        pct as u8
    }
}

/// Compute the percent of a USD budget consumed, clamped to `[0, 100]`.
pub fn budget_percent_usd(used: f64, cap: f64) -> u8 {
    if cap <= 0.0 {
        return 0;
    }
    let pct = (used / cap * 100.0).round();
    if pct > 100.0 {
        100
    } else if pct < 0.0 {
        0
    } else {
        pct as u8
    }
}

fn warning_style_if(base: Style, warned: bool) -> Style {
    if warned {
        base.fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

fn stop_reason_display(reason: &GrindStopReason) -> (String, Color) {
    match reason {
        GrindStopReason::Completed => ("completed".to_string(), Color::Green),
        GrindStopReason::Drained => ("drained".to_string(), Color::Yellow),
        GrindStopReason::Aborted => ("aborted".to_string(), Color::Red),
        GrindStopReason::BudgetExhausted(reason) => {
            (format!("budget exhausted ({reason})"), Color::Yellow)
        }
        GrindStopReason::ConsecutiveFailureLimit { limit } => (
            format!("consecutive-failure-limit (limit={limit})"),
            Color::Red,
        ),
    }
}

fn format_warning(kind: &BudgetWarningKind) -> String {
    match kind {
        BudgetWarningKind::Iterations { used, cap } => {
            format!("sessions {}/{} ({}%)", used, cap, budget_percent(u64::from(*used), u64::from(*cap)))
        }
        BudgetWarningKind::Tokens { used, cap } => {
            format!(
                "tokens {}/{} ({}%)",
                format_tokens(*used),
                format_tokens(*cap),
                budget_percent(*used, *cap)
            )
        }
        BudgetWarningKind::Cost { used, cap } => {
            format!(
                "cost {}/{} ({}%)",
                format_usd(*used),
                format_usd(*cap),
                budget_percent_usd(*used, *cap)
            )
        }
        BudgetWarningKind::Until {
            elapsed_secs,
            window_secs,
        } => {
            let frac = if *window_secs > 0 {
                (*elapsed_secs as f64 / *window_secs as f64 * 100.0).round() as u8
            } else {
                0
            };
            format!("time {}% elapsed", frac.min(100))
        }
    }
}

fn format_elapsed(d: chrono::Duration) -> String {
    let total = d.num_seconds().max(0);
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_usd(usd: f64) -> String {
    if usd <= 0.0 {
        "$0.00".to_string()
    } else if usd < 0.01 {
        "<$0.01".to_string()
    } else if usd < 100.0 {
        format!("${:.2}", usd)
    } else {
        format!("${:.0}", usd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::grind::{GrindEvent, SessionStatus};
    use crate::state::TokenUsage;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn fixture_started_at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 30, 12, 0, 0).unwrap()
    }

    fn fixture_app() -> GrindApp {
        let mut app = GrindApp::new(
            "20260430T120000Z-aaaa".into(),
            "pitboss/grind/20260430T120000Z-aaaa".into(),
            "default".into(),
            "claude-code".into(),
            fixture_started_at(),
            PlanBudgets {
                max_iterations: Some(10),
                max_tokens: Some(100_000),
                max_cost_usd: Some(5.0),
                until: None,
            },
        );
        app.set_now(fixture_started_at());
        app
    }

    fn fixture_record(seq: u32, status: SessionStatus) -> crate::grind::SessionRecord {
        crate::grind::SessionRecord {
            seq,
            run_id: "rid".into(),
            prompt: format!("prompt-{seq}"),
            started_at: fixture_started_at(),
            ended_at: fixture_started_at() + chrono::Duration::seconds(45),
            status,
            summary: Some("did the thing".into()),
            commit: None,
            tokens: TokenUsage {
                input: 1_500,
                output: 300,
                ..Default::default()
            },
            cost_usd: 0.025,
            transcript_path: PathBuf::from("transcripts/session-0001.log"),
        }
    }

    #[test]
    fn budget_percent_basic_buckets() {
        assert_eq!(budget_percent(0, 100), 0);
        assert_eq!(budget_percent(50, 100), 50);
        assert_eq!(budget_percent(80, 100), 80);
        assert_eq!(budget_percent(100, 100), 100);
        // Clamp above 100 — used can exceed cap when the runner overshoots.
        assert_eq!(budget_percent(150, 100), 100);
        // Zero cap means percent is 0 (no budget configured).
        assert_eq!(budget_percent(99, 0), 0);
    }

    #[test]
    fn budget_percent_usd_handles_floats() {
        assert_eq!(budget_percent_usd(0.0, 5.0), 0);
        assert_eq!(budget_percent_usd(2.5, 5.0), 50);
        assert_eq!(budget_percent_usd(4.0, 5.0), 80);
        assert_eq!(budget_percent_usd(5.0, 5.0), 100);
        assert_eq!(budget_percent_usd(8.0, 5.0), 100);
        assert_eq!(budget_percent_usd(1.0, 0.0), 0);
        assert_eq!(budget_percent_usd(1.0, -1.0), 0);
    }

    #[test]
    fn format_session_row_marks_in_flight_with_caret_glyph() {
        let row = SessionRow {
            seq: 3,
            prompt: "fp-hunter".into(),
            parallel_safe: false,
            started_at: Some(fixture_started_at()),
            ended_at: None,
            status: None,
            tokens: (0, 0),
            cost_usd: 0.0,
        };
        let line = format_session_row(&row, fixture_started_at() + chrono::Duration::seconds(7));
        let text: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.starts_with("> "), "expected `> ` in-flight glyph: {text}");
        assert!(text.contains("0003"));
        assert!(text.contains("fp-hunter"));
        assert!(text.contains("7s"));
    }

    #[test]
    fn format_session_row_uses_status_glyph_when_finished() {
        let mut row = SessionRow {
            seq: 12,
            prompt: "lint".into(),
            parallel_safe: true,
            started_at: Some(fixture_started_at()),
            ended_at: Some(fixture_started_at() + chrono::Duration::seconds(31)),
            status: Some(SessionStatus::Ok),
            tokens: (10, 5),
            cost_usd: 0.0,
        };
        let line = format_session_row(&row, fixture_started_at() + chrono::Duration::seconds(40));
        let text: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.starts_with("+ "), "expected `+ ` ok glyph: {text}");
        // parallel_safe sessions get the `*` marker.
        assert!(text.contains(" *"), "expected parallel marker in {text}");
        // duration is locked to ended_at - started_at, not now - started_at.
        assert!(text.contains("31s"), "duration not locked: {text}");

        row.status = Some(SessionStatus::Error);
        let line = format_session_row(&row, fixture_started_at());
        let text: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.starts_with("x "), "expected `x ` error glyph: {text}");
    }

    #[test]
    fn handle_session_started_appends_in_flight_row() {
        let mut app = fixture_app();
        app.handle_event(GrindEvent::SessionStarted {
            seq: 1,
            prompt: "alpha".into(),
            parallel_safe: false,
        });
        assert_eq!(app.session_count(), 1);
        let row = app.sessions().next().unwrap();
        assert!(row.in_flight());
        assert_eq!(row.prompt, "alpha");
    }

    #[test]
    fn handle_session_finished_updates_existing_row() {
        let mut app = fixture_app();
        app.handle_event(GrindEvent::SessionStarted {
            seq: 1,
            prompt: "alpha".into(),
            parallel_safe: false,
        });
        app.handle_event(GrindEvent::SessionFinished {
            record: fixture_record(1, SessionStatus::Ok),
        });
        assert_eq!(app.session_count(), 1);
        let row = app.sessions().next().unwrap();
        assert!(!row.in_flight());
        assert_eq!(row.status, Some(SessionStatus::Ok));
        assert_eq!(app.iterations, 1);
        assert_eq!(app.tokens_input, 1_500);
        assert_eq!(app.tokens_output, 300);
        assert!((app.cost_usd - 0.025).abs() < 1e-9);
    }

    #[test]
    fn agent_output_uses_seq_prefix_and_respects_pause() {
        let mut app = fixture_app();
        app.handle_event(GrindEvent::AgentStdout {
            seq: 7,
            line: "hello world".into(),
        });
        app.toggle_pause();
        app.handle_event(GrindEvent::AgentStdout {
            seq: 7,
            line: "dropped".into(),
        });
        app.toggle_pause();
        app.handle_event(GrindEvent::AgentStderr {
            seq: 7,
            line: "boom".into(),
        });

        let lines: Vec<&String> = app.output_lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "[0007] hello world");
        assert_eq!(lines[1], "[0007] err: boom");
    }

    #[test]
    fn scheduler_picked_updates_next_pick() {
        let mut app = fixture_app();
        app.handle_event(GrindEvent::SchedulerPicked {
            rotation: 1,
            pick: Some("triage".into()),
        });
        assert_eq!(app.next_pick(), Some("triage"));
        app.handle_event(GrindEvent::SchedulerPicked {
            rotation: 2,
            pick: None,
        });
        assert_eq!(app.next_pick(), None);
    }

    #[test]
    fn budget_warning_records_kind_and_logs_into_output() {
        let mut app = fixture_app();
        app.handle_event(GrindEvent::BudgetWarning {
            kind: BudgetWarningKind::Tokens {
                used: 81_000,
                cap: 100_000,
            },
        });
        assert_eq!(app.warnings.len(), 1);
        let last = app.output_lines().last().unwrap();
        assert!(last.starts_with("[budget]"), "got {last}");
        assert!(last.contains("tokens"));
        assert!(last.contains("81%"), "expected percent in {last}");
    }

    #[test]
    fn run_finished_records_stop_reason() {
        let mut app = fixture_app();
        app.handle_event(GrindEvent::RunFinished {
            stop_reason: GrindStopReason::Completed,
        });
        assert!(matches!(app.stop_reason(), Some(GrindStopReason::Completed)));
    }

    #[test]
    fn session_ring_evicts_oldest_when_full() {
        let mut app = fixture_app();
        for seq in 1..=(GRIND_SESSION_LOG_LINES as u32 + 5) {
            app.handle_event(GrindEvent::SessionStarted {
                seq,
                prompt: "p".into(),
                parallel_safe: false,
            });
        }
        assert_eq!(app.session_count(), GRIND_SESSION_LOG_LINES);
        let first = app.sessions().next().unwrap();
        assert_eq!(first.seq, 6, "oldest five should have been evicted");
    }

    #[test]
    fn fifty_event_smoke_test_renders_without_panic() {
        // Acceptance: drives the event stream and asserts no panic over 50
        // events. Each render call writes to a [`TestBackend`], proving that
        // the layout / paragraph machinery handles every event variant
        // without a `Layout::split` panic, an out-of-bounds scroll, or an
        // unwrap on a missing index.
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = fixture_app();
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let events: Vec<GrindEvent> = build_smoke_events();
        assert_eq!(events.len(), 50, "smoke test must drive exactly 50 events");

        for event in events {
            app.handle_event(event);
            terminal
                .draw(|f| app.render(f))
                .expect("render must not panic");
        }
        // Final render after RunFinished — proves the terminal-state branch
        // also renders cleanly.
        terminal.draw(|f| app.render(f)).unwrap();
    }

    fn build_smoke_events() -> Vec<GrindEvent> {
        let mut events: Vec<GrindEvent> = Vec::with_capacity(50);
        // 1 SchedulerPicked + 1 SessionStarted + 5 agent events + 1 hook +
        // 1 summary + 1 SessionFinished = 10 events per session × 4 sessions
        // = 40, plus 1 BudgetWarning, 1 final SchedulerPicked None, and
        // 1 RunFinished, plus 7 trailing AgentStdout lines that don't belong
        // to an active session (defensive: handler must tolerate them).
        for seq in 1..=4u32 {
            events.push(GrindEvent::SchedulerPicked {
                rotation: u64::from(seq),
                pick: Some(format!("prompt-{seq}")),
            });
            events.push(GrindEvent::SessionStarted {
                seq,
                prompt: format!("prompt-{seq}"),
                parallel_safe: seq.is_multiple_of(2),
            });
            for n in 0..5 {
                events.push(GrindEvent::AgentStdout {
                    seq,
                    line: format!("line {n} from session {seq}"),
                });
            }
            events.push(GrindEvent::HookFired {
                seq,
                kind: crate::grind::HookKind::PostSession,
                success: true,
                description: "ok".into(),
            });
            events.push(GrindEvent::SummaryCaptured {
                seq,
                summary: format!("summary {seq}"),
            });
            events.push(GrindEvent::SessionFinished {
                record: fixture_record(seq, SessionStatus::Ok),
            });
        }
        events.push(GrindEvent::BudgetWarning {
            kind: BudgetWarningKind::Cost {
                used: 4.0,
                cap: 5.0,
            },
        });
        events.push(GrindEvent::SchedulerPicked {
            rotation: 5,
            pick: None,
        });
        // Defensive trailing events with seqs that have no started row.
        for seq in 100..=106u32 {
            events.push(GrindEvent::AgentStdout {
                seq,
                line: "trailing".into(),
            });
        }
        events.push(GrindEvent::RunFinished {
            stop_reason: GrindStopReason::Completed,
        });
        events
    }

    #[test]
    fn key_handlers_quit_and_pause() {
        let mut app = fixture_app();
        assert!(handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::empty()));
        let mut app = fixture_app();
        assert!(handle_key(&mut app, KeyCode::Char('a'), KeyModifiers::empty()));
        let mut app = fixture_app();
        assert!(handle_key(
            &mut app,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        ));
        let mut app = fixture_app();
        assert!(!handle_key(
            &mut app,
            KeyCode::Char('p'),
            KeyModifiers::empty()
        ));
        assert!(app.is_paused());
        assert!(!handle_key(
            &mut app,
            KeyCode::Char('p'),
            KeyModifiers::empty()
        ));
        assert!(!app.is_paused());
    }

    #[test]
    fn format_warning_text_includes_each_kind() {
        assert!(format_warning(&BudgetWarningKind::Iterations { used: 8, cap: 10 })
            .contains("sessions 8/10"));
        assert!(format_warning(&BudgetWarningKind::Tokens {
            used: 80_000,
            cap: 100_000,
        })
        .contains("tokens"));
        assert!(format_warning(&BudgetWarningKind::Cost {
            used: 4.0,
            cap: 5.0,
        })
        .contains("cost"));
        assert!(format_warning(&BudgetWarningKind::Until {
            elapsed_secs: 80,
            window_secs: 100,
        })
        .contains("80%"));
    }

    #[test]
    fn strip_seq_prefix_handles_well_formed_and_malformed_lines() {
        assert_eq!(strip_seq_prefix("[0001] hello"), Some("hello"));
        assert_eq!(strip_seq_prefix("[budget] hi"), None);
        assert_eq!(strip_seq_prefix("no prefix here"), None);
        assert_eq!(strip_seq_prefix("[12] short"), None);
    }
}
