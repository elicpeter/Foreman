//! TUI application state and rendering.
//!
//! [`App`] folds [`crate::runner::Event`] into a snapshot of run progress —
//! per-phase status, attempt counters, current activity, and a capped buffer
//! of agent output lines. Rendering is a pure function of that snapshot, so
//! the same code path is exercised by the live dashboard and the snapshot
//! tests at the bottom of this file.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::ModelPricing;
use crate::plan::{PhaseId, Plan};
use crate::runner::{Event, HaltReason};
use crate::state::{RunState, TokenUsage};

/// Cap on the agent output buffer. Old lines are dropped once the cap is
/// reached so the dashboard cannot grow unbounded across a long run.
pub const OUTPUT_BUFFER_LINES: usize = 1000;

/// Height of the session-stats panel under the phase list. Sized to fit a
/// fixed eight content lines (elapsed / cost / tokens / dispatches plus a
/// `by role` heading and three role rows) inside its border.
const STATS_HEIGHT: u16 = 10;

/// Slice of [`crate::config::Config`] needed to price running token usage in
/// the session-stats panel. Built by [`crate::tui::run`] from the runner's
/// config so the App doesn't take a dependency on the full `Config` shape.
#[derive(Debug, Clone, Default)]
pub struct UsageView {
    /// Role name -> model id (mirrors [`crate::config::ModelRoles`]). Used
    /// to look up `pricing` when totaling cost.
    pub role_models: Vec<(String, String)>,
    /// Per-model price points, keyed by model id (clone of
    /// [`crate::config::Budgets::pricing`]).
    pub pricing: HashMap<String, ModelPricing>,
}

/// Static header chip describing the active agent backend and the per-role
/// model it dispatches with. The runner can mix models across roles when a
/// user splits Opus implementer / Sonnet auditor in `pitboss.toml`, so the
/// header tracks all three and renders the one belonging to the active
/// activity. `agent_name` mirrors [`crate::agent::Agent::name`].
#[derive(Debug, Clone)]
pub struct AgentDisplay {
    /// Backend identifier (e.g., `"claude-code"`, `"codex"`, `"aider"`,
    /// `"gemini"`, `"dry-run"`).
    pub agent_name: String,
    /// Model the implementer dispatch will use.
    pub implementer_model: String,
    /// Model the fixer dispatch will use.
    pub fixer_model: String,
    /// Model the auditor dispatch will use.
    pub auditor_model: String,
}

/// Per-phase status overlay computed from the runner event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseStatus {
    /// Phase is upcoming and the runner has not started it yet.
    Pending,
    /// Phase is the active dispatch.
    Running,
    /// Phase committed (or advanced without a commit, for excluded-only
    /// changes — both land in this variant).
    Completed,
    /// Phase halted with the carried halt reason.
    Failed(String),
}

/// Coarse current-activity indicator displayed in the header. Covers each
/// runner sub-pass distinctly so the user can tell at a glance whether the
/// implementer, fixer, auditor, or test runner is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// Run has not started dispatching agents yet.
    Idle,
    /// Implementer dispatch is in flight.
    Implementer,
    /// Fixer dispatch is in flight; carries the 1-based attempt index.
    Fixer(u32),
    /// Auditor dispatch is in flight.
    Auditor,
    /// Auditor was skipped because the staged diff was empty.
    AuditorSkipped,
    /// Test runner is active.
    Tests,
    /// Run finished cleanly — no further phases remain.
    Done,
    /// Run halted at the named phase with the carried halt summary.
    Halted(String),
}

impl fmt::Display for Activity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Activity::Idle => f.write_str("idle"),
            Activity::Implementer => f.write_str("implementer"),
            Activity::Fixer(n) => write!(f, "fixer (attempt {n})"),
            Activity::Auditor => f.write_str("auditor"),
            Activity::AuditorSkipped => f.write_str("auditor (skipped, no diff)"),
            Activity::Tests => f.write_str("running tests"),
            Activity::Done => f.write_str("finished"),
            Activity::Halted(s) => write!(f, "halted: {s}"),
        }
    }
}

/// Terminal-side dashboard state. Built once from a snapshot of the
/// [`RunState`] and [`Plan`] that the runner is about to drive, then mutated
/// by [`App::handle_event`] as the runner emits events.
pub struct App {
    run_id: String,
    branch: String,
    plan: Plan,
    /// The phase the runner is currently working on. Updates on
    /// [`Event::PhaseStarted`] so the header tracks the actual dispatch even
    /// after the in-memory plan advances.
    current_phase: PhaseId,
    phase_status: HashMap<PhaseId, PhaseStatus>,
    completed: Vec<PhaseId>,
    attempts: HashMap<PhaseId, u32>,
    activity: Activity,
    /// Active backend / per-role model strings rendered in the header. Static
    /// for the run; the rendered value tracks `activity` to show the model
    /// the currently dispatched role is using.
    agent_display: AgentDisplay,
    /// Pricing + role/model mapping used by the session-stats panel.
    usage_view: UsageView,
    /// Running token totals, replaced wholesale on each [`Event::UsageUpdated`].
    token_usage: TokenUsage,
    /// When this run was first started — used to compute elapsed time in the
    /// stats panel. Captured from [`RunState::started_at`] at construction.
    started_at: DateTime<Utc>,
    /// Optional override for the "now" timestamp in the stats panel. `None`
    /// in production (we just call [`Utc::now`]); set in tests to make the
    /// snapshot deterministic since the elapsed display would otherwise drift
    /// across runs.
    now_override: Option<DateTime<Utc>>,
    output: VecDeque<String>,
    /// User toggled "pause output" — UI-side only; new agent lines are
    /// dropped while paused so the user can read what is on screen without
    /// it scrolling out from under them.
    paused: bool,
    /// Set once the user requests quit — the host loop reads this to decide
    /// when to break out and cancel the runner.
    quit_requested: bool,
}

impl App {
    /// Build a fresh `App` from the snapshot the host runner is about to
    /// drive. `plan` is held as-is (it serves as the static phase list);
    /// `state` seeds the run-level header fields; `agent_display` populates
    /// the static agent / per-role model chip in the header; `usage_view`
    /// supplies the pricing table the session-stats panel uses.
    pub fn new(
        plan: Plan,
        state: RunState,
        agent_display: AgentDisplay,
        usage_view: UsageView,
    ) -> Self {
        let mut phase_status = HashMap::new();
        for phase in &plan.phases {
            phase_status.insert(phase.id.clone(), PhaseStatus::Pending);
        }
        for done in &state.completed {
            phase_status.insert(done.clone(), PhaseStatus::Completed);
        }
        Self {
            run_id: state.run_id.clone(),
            branch: state.branch.clone(),
            current_phase: plan.current_phase.clone(),
            phase_status,
            completed: state.completed.clone(),
            attempts: state.attempts.clone(),
            activity: Activity::Idle,
            agent_display,
            usage_view,
            token_usage: state.token_usage.clone(),
            started_at: state.started_at,
            now_override: None,
            output: VecDeque::with_capacity(OUTPUT_BUFFER_LINES),
            paused: false,
            quit_requested: false,
            plan,
        }
    }

    /// Borrow the loaded plan. Useful for the host to size the phase list.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// `true` once the user has requested quit (via `q` or `a`). The host
    /// drains and disposes of the [`Frame`] loop on the next tick.
    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    /// Mark quit. Idempotent.
    pub fn request_quit(&mut self) {
        self.quit_requested = true;
    }

    /// Override the "now" timestamp the stats panel reads for elapsed time.
    /// Test-only — production calls [`Utc::now`] directly so the panel ticks
    /// with the wall clock.
    #[cfg(test)]
    pub fn set_now(&mut self, now: DateTime<Utc>) {
        self.now_override = Some(now);
    }

    /// Toggle the "pause output stream" flag. While paused, agent stdout /
    /// stderr / tool-use events are dropped instead of appended so the user
    /// can read what is on screen.
    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
    }

    /// `true` while the output stream is paused.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Snapshot of the agent output buffer, oldest line first. Exposed for
    /// tests; the renderer iterates the same buffer directly.
    pub fn output_lines(&self) -> impl Iterator<Item = &String> {
        self.output.iter()
    }

    /// Fold a runner event into the dashboard state.
    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::PhaseStarted {
                phase_id, attempt, ..
            } => {
                self.phase_status
                    .insert(phase_id.clone(), PhaseStatus::Running);
                self.attempts.insert(phase_id.clone(), attempt);
                self.current_phase = phase_id;
                self.activity = Activity::Implementer;
            }
            Event::FixerStarted {
                phase_id,
                fixer_attempt,
                attempt,
            } => {
                self.attempts.insert(phase_id, attempt);
                self.activity = Activity::Fixer(fixer_attempt);
            }
            Event::AuditorStarted { phase_id, attempt } => {
                self.attempts.insert(phase_id, attempt);
                self.activity = Activity::Auditor;
            }
            Event::AuditorSkippedNoChanges { .. } => {
                self.activity = Activity::AuditorSkipped;
            }
            Event::AgentStdout(line) => {
                if !self.paused {
                    self.push_output(line);
                }
            }
            Event::AgentStderr(line) => {
                if !self.paused {
                    self.push_output(format!("err: {line}"));
                }
            }
            Event::AgentToolUse(name) => {
                if !self.paused {
                    self.push_output(format!("tool: {name}"));
                }
            }
            Event::TestStarted => {
                self.activity = Activity::Tests;
            }
            Event::TestFinished { passed, summary } => {
                let label = if passed {
                    "tests passed"
                } else {
                    "tests failed"
                };
                self.push_output(format!("[{label}] {summary}"));
            }
            Event::TestsSkipped => {
                self.push_output("[tests] no runner detected; skipped".to_string());
            }
            Event::PhaseCommitted { phase_id, commit } => {
                self.phase_status
                    .insert(phase_id.clone(), PhaseStatus::Completed);
                if !self.completed.contains(&phase_id) {
                    self.completed.push(phase_id.clone());
                }
                let line = match commit {
                    Some(c) => format!("[commit] phase {phase_id}: {c}"),
                    None => format!("[commit] phase {phase_id}: no code changes"),
                };
                self.push_output(line);
            }
            Event::PhaseHalted { phase_id, reason } => {
                self.phase_status
                    .insert(phase_id.clone(), PhaseStatus::Failed(reason.to_string()));
                self.activity = Activity::Halted(format_halt(&reason));
                self.push_output(format!("[halt] phase {phase_id}: {reason}"));
            }
            Event::RunFinished => {
                self.activity = Activity::Done;
            }
            Event::UsageUpdated(usage) => {
                self.token_usage = usage;
            }
        }
    }

    fn push_output(&mut self, line: String) {
        if self.output.len() == OUTPUT_BUFFER_LINES {
            self.output.pop_front();
        }
        self.output.push_back(line);
    }

    /// Render the entire dashboard. Pure function of `&self` so the same
    /// code drives the live terminal and the snapshot tests.
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);
        self.render_header(frame, layout[0]);
        self.render_body(frame, layout[1]);
        self.render_footer(frame, layout[2]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let title = self
            .plan
            .phase(&self.current_phase)
            .map(|p| p.title.as_str())
            .unwrap_or("");
        let line1 = Line::from(vec![
            Span::styled(
                "pitboss",
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
        let act_color = activity_color(&self.activity);
        let line2 = Line::from(vec![
            Span::styled("phase ", Style::default().fg(Color::Gray)),
            Span::styled(
                self.current_phase.to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" — ", Style::default().fg(Color::Gray)),
            Span::styled(title.to_string(), Style::default().fg(Color::White)),
            Span::raw("   "),
            Span::styled("[", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", self.activity),
                Style::default().fg(act_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled("]", Style::default().fg(Color::DarkGray)),
        ]);
        let line3 = Line::from(vec![
            Span::styled("agent ", Style::default().fg(Color::Gray)),
            Span::styled(
                self.agent_display.agent_name.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("model ", Style::default().fg(Color::Gray)),
            Span::styled(
                self.current_model().to_string(),
                Style::default().fg(Color::Yellow),
            ),
        ]);
        let block = Block::default().borders(Borders::BOTTOM);
        let para = Paragraph::new(vec![line1, line2, line3]).block(block);
        frame.render_widget(para, area);
    }

    /// Resolve the model string the active activity dispatches with. Idle /
    /// Tests / Done / Halted aren't role-specific so they fall back to the
    /// implementer's model — what's about to run, or what mostly drove the run.
    fn current_model(&self) -> &str {
        match &self.activity {
            Activity::Fixer(_) => &self.agent_display.fixer_model,
            Activity::Auditor | Activity::AuditorSkipped => &self.agent_display.auditor_model,
            _ => &self.agent_display.implementer_model,
        }
    }

    fn render_body(&self, frame: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);
        // Drop the stats panel on terminals too short to fit it without
        // squeezing the phase list to nothing — the panel needs `STATS_HEIGHT`
        // rows plus a couple for the phase list to remain useful.
        if cols[0].height >= STATS_HEIGHT + 4 {
            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(STATS_HEIGHT)])
                .split(cols[0]);
            self.render_phases(frame, left[0]);
            self.render_stats(frame, left[1]);
        } else {
            self.render_phases(frame, cols[0]);
        }
        self.render_output(frame, cols[1]);
    }

    fn render_phases(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .plan
            .phases
            .iter()
            .map(|phase| {
                let status = self
                    .phase_status
                    .get(&phase.id)
                    .cloned()
                    .unwrap_or(PhaseStatus::Pending);
                let glyph = status_glyph(&status);
                let attempts = self.attempts.get(&phase.id).copied().unwrap_or(0);
                let tail = if attempts > 0 {
                    format!("  ({attempts}x)")
                } else {
                    String::new()
                };
                let glyph_style = status_style(&status);
                let (id_style, title_style) = match &status {
                    PhaseStatus::Running => (
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    PhaseStatus::Completed => (
                        Style::default().fg(Color::Green),
                        Style::default().fg(Color::Gray),
                    ),
                    PhaseStatus::Failed(_) => (
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Red),
                    ),
                    PhaseStatus::Pending => (
                        Style::default().fg(Color::DarkGray),
                        Style::default().fg(Color::DarkGray),
                    ),
                };
                let line = Line::from(vec![
                    Span::styled(format!("{glyph} "), glyph_style),
                    Span::styled(format!("{} ", phase.id), id_style),
                    Span::styled(phase.title.clone(), title_style),
                    Span::styled(tail, Style::default().fg(Color::DarkGray)),
                ]);
                ListItem::new(line)
            })
            .collect();
        let border_style = if self
            .phase_status
            .values()
            .any(|s| matches!(s, PhaseStatus::Running))
        {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(
                format!(
                    " phases ({}/{}) ",
                    self.completed.len(),
                    self.plan.phases.len()
                ),
                Style::default().fg(Color::Gray),
            ));
        let list = List::new(items).block(block);
        frame.render_widget(list, area);
    }

    fn render_stats(&self, frame: &mut Frame, area: Rect) {
        let label = Style::default().fg(Color::Gray);
        let value = Style::default().fg(Color::White);
        let dim = Style::default().fg(Color::DarkGray);

        let now = self.now_override.unwrap_or_else(Utc::now);
        let elapsed = format_elapsed(now - self.started_at);
        let total_in = self.token_usage.input;
        let total_out = self.token_usage.output;
        let total_usd = self.total_usd();
        let dispatches: u32 = self.attempts.values().copied().sum();

        let mut lines: Vec<Line> = Vec::with_capacity(8);
        lines.push(Line::from(vec![
            Span::styled(" elapsed   ", label),
            Span::styled(elapsed, Style::default().fg(Color::Cyan)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" cost      ", label),
            Span::styled(
                format_usd(total_usd),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" tokens    ", label),
            Span::styled(format_tokens(total_in), value),
            Span::styled(" / ", dim),
            Span::styled(format_tokens(total_out), value),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" dispatch  ", label),
            Span::styled(dispatches.to_string(), value),
        ]));
        lines.push(Line::from(Span::styled(" by role", dim)));

        for role in ["implementer", "fixer", "auditor"] {
            let usage = self.token_usage.by_role.get(role);
            let (rin, rout) = usage.map(|u| (u.input, u.output)).unwrap_or((0, 0));
            let role_usd = self.role_usd(role, rin, rout);
            let role_color = role_color(role);
            let short = role_short(role);
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(format!("{short:<4}"), Style::default().fg(role_color)),
                Span::raw(" "),
                Span::styled(format_tokens(rin), value),
                Span::styled("/", dim),
                Span::styled(format_tokens(rout), value),
                Span::raw(" "),
                Span::styled(format_usd(role_usd), Style::default().fg(Color::Green)),
            ]));
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(" session ", label));
        let para = Paragraph::new(lines).block(block);
        frame.render_widget(para, area);
    }

    fn role_usd(&self, role: &str, input: u64, output: u64) -> f64 {
        let Some((_, model)) = self.usage_view.role_models.iter().find(|(r, _)| r == role) else {
            return 0.0;
        };
        let Some(price) = self.usage_view.pricing.get(model) else {
            return 0.0;
        };
        price.cost_usd(input, output)
    }

    fn total_usd(&self) -> f64 {
        let mut total = 0.0;
        for (role, model) in &self.usage_view.role_models {
            let Some(usage) = self.token_usage.by_role.get(role) else {
                continue;
            };
            let Some(price) = self.usage_view.pricing.get(model) else {
                continue;
            };
            total += price.cost_usd(usage.input, usage.output);
        }
        total
    }

    fn render_output(&self, frame: &mut Frame, area: Rect) {
        // Show the last N lines that fit in the pane (subtract the borders).
        let inner_height = area.height.saturating_sub(2) as usize;
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
        frame.render_widget(para, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
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

fn style_output_line(s: &str) -> Line<'static> {
    if s.starts_with("err: ") {
        Line::from(Span::styled(s.to_owned(), Style::default().fg(Color::Red)))
    } else if s.starts_with("tool: ") {
        Line::from(Span::styled(
            s.to_owned(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM),
        ))
    } else if s.starts_with("[tests passed]") {
        Line::from(Span::styled(
            s.to_owned(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
    } else if s.starts_with("[tests failed]") {
        Line::from(Span::styled(
            s.to_owned(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else if s.starts_with("[commit]") {
        Line::from(Span::styled(s.to_owned(), Style::default().fg(Color::Cyan)))
    } else if s.starts_with("[halt]") {
        Line::from(Span::styled(
            s.to_owned(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else if s.starts_with("[tests]") {
        Line::from(Span::styled(
            s.to_owned(),
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(Span::styled(
            s.to_owned(),
            Style::default().fg(Color::White),
        ))
    }
}

fn status_glyph(s: &PhaseStatus) -> &'static str {
    match s {
        PhaseStatus::Pending => "·",
        PhaseStatus::Running => ">",
        PhaseStatus::Completed => "+",
        PhaseStatus::Failed(_) => "x",
    }
}

fn status_style(s: &PhaseStatus) -> Style {
    match s {
        PhaseStatus::Pending => Style::default().fg(Color::DarkGray),
        PhaseStatus::Running => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        PhaseStatus::Completed => Style::default().fg(Color::Green),
        PhaseStatus::Failed(_) => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

fn activity_color(a: &Activity) -> Color {
    match a {
        Activity::Idle => Color::DarkGray,
        Activity::Implementer => Color::Cyan,
        Activity::Fixer(_) => Color::Yellow,
        Activity::Auditor | Activity::AuditorSkipped => Color::Blue,
        Activity::Tests => Color::Magenta,
        Activity::Done => Color::Green,
        Activity::Halted(_) => Color::Red,
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

fn role_short(role: &str) -> &'static str {
    match role {
        "implementer" => "impl",
        "fixer" => "fix",
        "auditor" => "aud",
        "planner" => "plan",
        _ => "role",
    }
}

fn role_color(role: &str) -> Color {
    match role {
        "implementer" => Color::Cyan,
        "fixer" => Color::Yellow,
        "auditor" => Color::Blue,
        "planner" => Color::Magenta,
        _ => Color::Gray,
    }
}

fn format_halt(reason: &HaltReason) -> String {
    match reason {
        HaltReason::PlanTampered => "plan tampered".to_string(),
        HaltReason::DeferredInvalid(_) => "deferred invalid".to_string(),
        HaltReason::TestsFailed(_) => "tests failed".to_string(),
        HaltReason::AgentFailure(_) => "agent failure".to_string(),
        HaltReason::BudgetExceeded(_) => "budget exceeded".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::plan::{Phase, PhaseId};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    fn three_phase_plan() -> Plan {
        Plan::new(
            pid("01"),
            vec![
                Phase {
                    id: pid("01"),
                    title: "Project foundation".into(),
                    body: String::new(),
                },
                Phase {
                    id: pid("02"),
                    title: "Domain types".into(),
                    body: String::new(),
                },
                Phase {
                    id: pid("03"),
                    title: "Plan parser".into(),
                    body: String::new(),
                },
            ],
        )
    }

    fn fresh_state() -> RunState {
        RunState::new(
            "20260430T120000Z",
            "pitboss/run-20260430T120000Z",
            pid("01"),
        )
    }

    fn fixed_started_at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-04-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Snapshot-friendly `RunState` with a fixed `started_at`. Without this
    /// the wall-clock-driven `started_at` from [`RunState::new`] would make
    /// the elapsed-time line in the stats panel non-deterministic.
    fn fresh_state_at(started_at: DateTime<Utc>) -> RunState {
        let mut s = fresh_state();
        s.started_at = started_at;
        s
    }

    fn fixture_agent() -> AgentDisplay {
        AgentDisplay {
            agent_name: "claude-code".into(),
            implementer_model: "claude-opus-4-7".into(),
            fixer_model: "claude-sonnet-4-6".into(),
            auditor_model: "claude-sonnet-4-6".into(),
        }
    }

    fn fixture_usage_view() -> UsageView {
        let mut pricing = HashMap::new();
        pricing.insert(
            "claude-opus-4-7".to_string(),
            ModelPricing {
                input_per_million_usd: 15.0,
                output_per_million_usd: 75.0,
            },
        );
        pricing.insert(
            "claude-sonnet-4-6".to_string(),
            ModelPricing {
                input_per_million_usd: 3.0,
                output_per_million_usd: 15.0,
            },
        );
        UsageView {
            role_models: vec![
                ("planner".into(), "claude-opus-4-7".into()),
                ("implementer".into(), "claude-opus-4-7".into()),
                ("fixer".into(), "claude-sonnet-4-6".into()),
                ("auditor".into(), "claude-sonnet-4-6".into()),
            ],
            pricing,
        }
    }

    fn render_to_string(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        buffer_to_string(terminal.backend().buffer())
    }

    fn buffer_to_string(buf: &Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn handle_phase_started_marks_phase_running_and_sets_activity() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
        );
        app.handle_event(Event::PhaseStarted {
            phase_id: pid("01"),
            title: "Project foundation".into(),
            attempt: 1,
        });
        assert_eq!(app.activity, Activity::Implementer);
        assert_eq!(app.phase_status[&pid("01")], PhaseStatus::Running);
        assert_eq!(app.attempts.get(&pid("01")).copied(), Some(1));
    }

    #[test]
    fn fixer_started_sets_activity_with_attempt_index() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
        );
        app.handle_event(Event::FixerStarted {
            phase_id: pid("01"),
            fixer_attempt: 2,
            attempt: 3,
        });
        assert_eq!(app.activity, Activity::Fixer(2));
        assert_eq!(app.attempts.get(&pid("01")).copied(), Some(3));
    }

    #[test]
    fn phase_committed_moves_phase_to_completed() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
        );
        app.handle_event(Event::PhaseStarted {
            phase_id: pid("01"),
            title: "Project foundation".into(),
            attempt: 1,
        });
        app.handle_event(Event::PhaseCommitted {
            phase_id: pid("01"),
            commit: None,
        });
        assert_eq!(app.phase_status[&pid("01")], PhaseStatus::Completed);
        assert!(app.completed.contains(&pid("01")));
    }

    #[test]
    fn phase_halted_marks_failure_and_sets_halted_activity() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
        );
        app.handle_event(Event::PhaseHalted {
            phase_id: pid("02"),
            reason: HaltReason::TestsFailed("boom".into()),
        });
        match &app.phase_status[&pid("02")] {
            PhaseStatus::Failed(msg) => assert!(msg.contains("tests failed")),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(matches!(app.activity, Activity::Halted(_)));
    }

    #[test]
    fn agent_output_is_appended_until_paused() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
        );
        app.handle_event(Event::AgentStdout("first line".into()));
        app.handle_event(Event::AgentStdout("second".into()));
        let lines: Vec<&String> = app.output_lines().collect();
        assert_eq!(lines.len(), 2);

        app.toggle_pause();
        app.handle_event(Event::AgentStdout("dropped".into()));
        let lines: Vec<&String> = app.output_lines().collect();
        assert_eq!(lines.len(), 2, "pause must drop new agent lines");

        app.toggle_pause();
        app.handle_event(Event::AgentStdout("third".into()));
        let lines: Vec<&String> = app.output_lines().collect();
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn header_model_chip_tracks_active_role() {
        // The header's `model <id>` chip must follow the dispatched role so a
        // mixed-model run (e.g., Opus implementer + Sonnet auditor) shows the
        // truthful identifier at every moment of the dispatch loop.
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
        );
        // Idle / pre-dispatch falls back to the implementer's model.
        assert_eq!(app.current_model(), "claude-opus-4-7");

        app.handle_event(Event::PhaseStarted {
            phase_id: pid("01"),
            title: "Project foundation".into(),
            attempt: 1,
        });
        assert_eq!(app.current_model(), "claude-opus-4-7");

        app.handle_event(Event::FixerStarted {
            phase_id: pid("01"),
            fixer_attempt: 1,
            attempt: 2,
        });
        assert_eq!(app.current_model(), "claude-sonnet-4-6");

        app.handle_event(Event::AuditorStarted {
            phase_id: pid("01"),
            attempt: 3,
        });
        assert_eq!(app.current_model(), "claude-sonnet-4-6");

        app.handle_event(Event::TestStarted);
        // Tests don't dispatch a role; chip falls back to implementer.
        assert_eq!(app.current_model(), "claude-opus-4-7");
    }

    #[test]
    fn output_buffer_drops_oldest_when_full() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
        );
        for i in 0..(OUTPUT_BUFFER_LINES + 5) {
            app.handle_event(Event::AgentStdout(format!("line {i}")));
        }
        assert_eq!(app.output.len(), OUTPUT_BUFFER_LINES);
        // First five must have been dropped.
        let first = app.output.front().unwrap();
        assert_eq!(first, "line 5");
    }

    #[test]
    fn render_initial_layout_80x20() {
        let started_at = fixed_started_at();
        let mut app = App::new(
            three_phase_plan(),
            fresh_state_at(started_at),
            fixture_agent(),
            fixture_usage_view(),
        );
        app.set_now(started_at);
        let snap = render_to_string(&app, 80, 20);
        insta::assert_snapshot!("initial_80x20", snap);
    }

    #[test]
    fn render_mid_run_with_output_120x30() {
        let started_at = fixed_started_at();
        let mut app = App::new(
            three_phase_plan(),
            fresh_state_at(started_at),
            fixture_agent(),
            fixture_usage_view(),
        );
        // 2 minutes 14 seconds into the run.
        app.set_now(started_at + chrono::Duration::seconds(134));
        app.handle_event(Event::PhaseStarted {
            phase_id: pid("01"),
            title: "Project foundation".into(),
            attempt: 1,
        });
        app.handle_event(Event::AgentStdout("Reading plan.md".into()));
        app.handle_event(Event::AgentStdout("Editing src/lib.rs".into()));
        app.handle_event(Event::TestStarted);
        app.handle_event(Event::TestFinished {
            passed: true,
            summary: "12 passed".into(),
        });
        app.handle_event(Event::PhaseCommitted {
            phase_id: pid("01"),
            commit: Some(crate::git::CommitId::new("abc1234")),
        });
        app.handle_event(Event::PhaseStarted {
            phase_id: pid("02"),
            title: "Domain types".into(),
            attempt: 1,
        });
        app.handle_event(Event::AgentStdout("Defining PhaseId".into()));
        let mut usage = TokenUsage {
            input: 32_000 + 8_000 + 5_200,
            output: 5_200 + 1_900 + 1_000,
            ..Default::default()
        };
        usage.by_role.insert(
            "implementer".into(),
            crate::state::RoleUsage {
                input: 32_000,
                output: 5_200,
            },
        );
        usage.by_role.insert(
            "fixer".into(),
            crate::state::RoleUsage {
                input: 8_000,
                output: 1_900,
            },
        );
        usage.by_role.insert(
            "auditor".into(),
            crate::state::RoleUsage {
                input: 5_200,
                output: 1_000,
            },
        );
        app.handle_event(Event::UsageUpdated(usage));

        let snap = render_to_string(&app, 120, 30);
        insta::assert_snapshot!("mid_run_120x30", snap);
    }

    #[test]
    fn render_halted_state_80x20() {
        let started_at = fixed_started_at();
        let mut app = App::new(
            three_phase_plan(),
            fresh_state_at(started_at),
            fixture_agent(),
            fixture_usage_view(),
        );
        app.set_now(started_at + chrono::Duration::seconds(45));
        app.handle_event(Event::PhaseStarted {
            phase_id: pid("02"),
            title: "Domain types".into(),
            attempt: 1,
        });
        app.handle_event(Event::PhaseHalted {
            phase_id: pid("02"),
            reason: HaltReason::PlanTampered,
        });
        let snap = render_to_string(&app, 80, 20);
        insta::assert_snapshot!("halted_80x20", snap);
    }

    #[test]
    fn usage_updated_replaces_running_totals() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            fixture_usage_view(),
        );
        assert_eq!(app.token_usage.input, 0);
        let mut usage = TokenUsage {
            input: 1_234,
            output: 56,
            ..Default::default()
        };
        usage.by_role.insert(
            "implementer".into(),
            crate::state::RoleUsage {
                input: 1_234,
                output: 56,
            },
        );
        app.handle_event(Event::UsageUpdated(usage));
        assert_eq!(app.token_usage.input, 1_234);
        assert_eq!(app.token_usage.output, 56);
        let usd = app.total_usd();
        // 1234 input @ $15/M = 0.01851; 56 output @ $75/M = 0.0042; total ~0.02271.
        assert!((usd - 0.022_71).abs() < 1e-4, "got {usd}");
    }

    #[test]
    fn formatters_round_trip_token_and_usd_buckets() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(1_234_000), "1.23M");

        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(0.001), "<$0.01");
        assert_eq!(format_usd(0.43), "$0.43");
        assert_eq!(format_usd(123.4), "$123");

        assert_eq!(format_elapsed(chrono::Duration::seconds(0)), "0s");
        assert_eq!(format_elapsed(chrono::Duration::seconds(45)), "45s");
        assert_eq!(format_elapsed(chrono::Duration::seconds(125)), "2m 05s");
        assert_eq!(format_elapsed(chrono::Duration::seconds(3_725)), "1h 02m");
    }
}
