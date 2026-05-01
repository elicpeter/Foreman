//! TUI application state and rendering.
//!
//! [`App`] folds [`crate::runner::Event`] into a snapshot of run progress —
//! per-phase status, attempt counters, current activity, and a capped buffer
//! of agent output lines. Rendering is a pure function of that snapshot, so
//! the same code path is exercised by the live dashboard and the snapshot
//! tests at the bottom of this file.
//!
//! # Manual sweep / stale-items checklist
//!
//! The five phase 07 sweep frames are verified by hand against a real run.
//! To drive each frame:
//!
//! 1. **Sweep in flight.** Configure `[sweep] trigger_threshold = 1`, leave
//!    one unchecked `## Deferred items` entry in `deferred.md`, and run
//!    `pitboss play --tui`. After the first phase commits, the gate fires
//!    a sweep and the header should read `Sweep after phase NN — attempt 1`
//!    with `[implementer]` in the activity bracket.
//! 2. **Sweep auditor.** With `[sweep] audit_enabled = true`, watch the
//!    sweep dispatch land staged changes — the header should switch to
//!    `Sweep after phase NN — auditor` with `[auditor]` bracketed.
//! 3. **Sweep completed.** When `SweepCompleted` lands, a `[sweep] after NN:
//!    K resolved (commit)` row should appear in the agent output pane in
//!    the same cyan style as the `[commit]` rows.
//! 4. **Sweep halted.** Force a sweep auditor halt (e.g., a deferred item
//!    whose verification step the auditor will reject). The header switches
//!    to `[halted: ...]` and a red `[sweep:halt]` row joins the output pane.
//! 5. **Stale items panel.** Set `[sweep] escalate_after = 1`, leave the
//!    same unchecked item in place across two sweep dispatches. After the
//!    second sweep, the new collapsible "stale items" panel under
//!    "session" should list the item in yellow.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;
use tracing::debug;

use crate::config::ModelPricing;
use crate::plan::{PhaseId, Plan};
use crate::prompts::StaleItem;
use crate::runner::{AuditContextKind, Event, HaltReason};
use crate::state::{RunState, TokenUsage};

/// Cap on the agent output buffer. Old lines are dropped once the cap is
/// reached so the dashboard cannot grow unbounded across a long run.
pub const OUTPUT_BUFFER_LINES: usize = 1000;

/// Height of the session-stats panel under the phase list. Sized to fit a
/// fixed eight content lines (elapsed / cost / tokens / dispatches plus a
/// `by role` heading and three role rows) inside its border.
const STATS_HEIGHT: u16 = 10;

/// Height of the stale-items panel under the session-stats panel. Sized to
/// fit five content lines plus the title border. The panel auto-collapses
/// when the stale list is empty so this only contributes when there is
/// something to show.
const STALE_HEIGHT: u16 = 7;

// Cap on the number of stale items rendered in the panel. Shared with
// `pitboss status` via [`crate::runner::STALE_ITEMS_DISPLAY_CAP`] so the two
// operator surfaces stay in lockstep; both add a "+N more" footer past this.
use crate::runner::STALE_ITEMS_DISPLAY_CAP as STALE_PANEL_CAP;

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
/// user splits Opus implementer / Sonnet auditor in `config.toml`, so the
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
    /// Implementer dispatch for a regular phase is in flight.
    Implementer,
    /// Implementer dispatch for a deferred-sweep step is in flight.
    SweepImplementer,
    /// Fixer dispatch is in flight; carries the 1-based attempt index.
    Fixer(u32),
    /// Auditor dispatch for a regular phase is in flight.
    Auditor,
    /// Auditor dispatch for a deferred-sweep step is in flight.
    SweepAuditor,
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
            Activity::SweepImplementer => f.write_str("sweep:implementer"),
            Activity::Fixer(n) => write!(f, "fixer (attempt {n})"),
            Activity::Auditor => f.write_str("auditor"),
            Activity::SweepAuditor => f.write_str("sweep:auditor"),
            Activity::AuditorSkipped => f.write_str("auditor (skipped, no diff)"),
            Activity::Tests => f.write_str("running tests"),
            Activity::Done => f.write_str("finished"),
            Activity::Halted(s) => write!(f, "halted: {s}"),
        }
    }
}

/// Active sweep tracked by the dashboard.
///
/// Set on [`Event::SweepStarted`], updated when the sweep auditor fires, and
/// cleared on [`Event::SweepCompleted`] or [`Event::SweepHalted`]. While set,
/// the header takes its label from this struct rather than from the current
/// phase title so the operator can tell at a glance that a sweep — not a
/// regular phase dispatch — is in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SweepState {
    /// Phase id the sweep is firing after (mirrors
    /// [`Event::SweepStarted::after`]).
    after: PhaseId,
    /// 1-based total dispatch counter at `after` for this sweep, mirrored
    /// from the event payload.
    attempt: u32,
    /// `true` once the sweep auditor has been observed for this sweep —
    /// the header switches from `attempt N` to `auditor` while it runs.
    in_auditor: bool,
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
    /// Tracks an in-flight sweep so the header and panels can render its
    /// label distinctly. `None` outside of a sweep dispatch.
    sweep_state: Option<SweepState>,
    /// Stale `## Deferred items` entries (counters at or above
    /// `[sweep] escalate_after`). Sorted by descending attempts. Hydrated
    /// from [`RunState::deferred_item_attempts`] at startup and updated on
    /// [`Event::DeferredItemStale`] at runtime. Capped at the prompt-side
    /// max so a runaway map can't bloat the panel.
    stale_items: Vec<StaleItem>,
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
    /// supplies the pricing table the session-stats panel uses;
    /// `stale_items` rehydrates the stale-items panel from the runner's
    /// staleness tracker (`Runner::stale_items`) so a resumed run shows the
    /// same list `pitboss status` would.
    pub fn new(
        plan: Plan,
        state: RunState,
        agent_display: AgentDisplay,
        usage_view: UsageView,
        stale_items: Vec<StaleItem>,
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
            sweep_state: None,
            stale_items,
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
                self.sweep_state = None;
            }
            Event::FixerStarted {
                phase_id,
                fixer_attempt,
                attempt,
            } => {
                self.attempts.insert(phase_id, attempt);
                self.activity = Activity::Fixer(fixer_attempt);
            }
            Event::AuditorStarted { context, attempt } => {
                self.attempts.insert(context.phase_id.clone(), attempt);
                self.activity = match context.kind {
                    AuditContextKind::Phase => Activity::Auditor,
                    AuditContextKind::Sweep => Activity::SweepAuditor,
                };
                if context.kind == AuditContextKind::Sweep {
                    if let Some(sweep) = self.sweep_state.as_mut() {
                        sweep.in_auditor = true;
                    }
                }
            }
            Event::AuditorSkippedNoChanges { context } => {
                self.activity = Activity::AuditorSkipped;
                if context.kind == AuditContextKind::Sweep {
                    if let Some(sweep) = self.sweep_state.as_mut() {
                        sweep.in_auditor = true;
                    }
                }
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
            Event::SweepStarted {
                after,
                items_pending,
                attempt,
            } => {
                if !self.completed.contains(&after) {
                    debug!(
                        "tui: SweepStarted for phase {after} arrived without a preceding \
                         PhaseCommitted; rendering with the state we have"
                    );
                    self.push_output(format!(
                        "[tui:warn] SweepStarted({after}) without PhaseCommitted({after}); \
                         event stream out of order"
                    ));
                }
                self.attempts.insert(after.clone(), attempt);
                self.activity = Activity::SweepImplementer;
                self.sweep_state = Some(SweepState {
                    after: after.clone(),
                    attempt,
                    in_auditor: false,
                });
                self.push_output(format!(
                    "[sweep] after phase {after}: {items_pending} pending"
                ));
            }
            Event::SweepCompleted {
                after,
                resolved,
                commit,
            } => {
                if self.sweep_state.is_none() {
                    debug!(
                        "tui: SweepCompleted for phase {after} arrived without a preceding \
                         SweepStarted; rendering with the state we have"
                    );
                    self.push_output(format!(
                        "[tui:warn] SweepCompleted({after}) without SweepStarted({after}); \
                         event stream out of order"
                    ));
                }
                self.sweep_state = None;
                // The sweep has finished; clear the sweep-specific
                // activity so the chip doesn't misleadingly read
                // `[sweep:implementer]` while we wait for the next
                // PhaseStarted / TestStarted / RunFinished. Idle is the
                // accurate "between dispatches" state.
                self.activity = Activity::Idle;
                let line = match commit {
                    Some(c) => format!("[sweep] after {after}: {resolved} items resolved ({c})"),
                    None => format!("[sweep] after {after}: {resolved} items resolved"),
                };
                self.push_output(line);
            }
            Event::SweepHalted { after, reason } => {
                self.sweep_state = None;
                self.activity = Activity::Halted(format_halt(&reason));
                self.push_output(format!("[sweep:halt] after phase {after}: {reason}"));
            }
            Event::DeferredItemStale { text, attempts } => {
                self.upsert_stale_item(text, attempts);
            }
        }
    }

    fn push_output(&mut self, line: String) {
        if self.output.len() == OUTPUT_BUFFER_LINES {
            self.output.pop_front();
        }
        self.output.push_back(line);
    }

    /// Insert a stale item or update its attempt count. Keeps the list sorted
    /// by descending attempts (text ascending as a deterministic tiebreaker)
    /// and capped at the prompt-side max so the panel can't grow unbounded
    /// when many items go stale in the same run.
    fn upsert_stale_item(&mut self, text: String, attempts: u32) {
        if let Some(existing) = self.stale_items.iter_mut().find(|s| s.text == text) {
            existing.attempts = attempts;
        } else {
            self.stale_items.push(StaleItem { text, attempts });
        }
        self.stale_items
            .sort_by(|a, b| b.attempts.cmp(&a.attempts).then(a.text.cmp(&b.text)));
        self.stale_items
            .truncate(crate::runner::STALE_ITEMS_PROMPT_CAP);
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
        let line2 = if let Some(sweep) = &self.sweep_state {
            let label = if sweep.in_auditor {
                format!("Sweep after phase {} — auditor", sweep.after)
            } else {
                format!(
                    "Sweep after phase {} — attempt {}",
                    sweep.after, sweep.attempt
                )
            };
            Line::from(vec![
                Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled("[", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}", self.activity),
                    Style::default().fg(act_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled("]", Style::default().fg(Color::DarkGray)),
            ])
        } else {
            let title = self
                .plan
                .phase(&self.current_phase)
                .map(|p| p.title.as_str())
                .unwrap_or("");
            Line::from(vec![
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
            ])
        };
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
            Activity::Auditor | Activity::SweepAuditor | Activity::AuditorSkipped => {
                &self.agent_display.auditor_model
            }
            _ => &self.agent_display.implementer_model,
        }
    }

    fn render_body(&self, frame: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);
        // Layout heuristic: stale items get squeezed first, then the session
        // stats. The phase list always renders so the operator can see where
        // they are in the run even on a tiny terminal.
        let height = cols[0].height;
        let want_stale = !self.stale_items.is_empty() && height >= STATS_HEIGHT + STALE_HEIGHT + 4;
        let want_stats = height >= STATS_HEIGHT + 4;
        if want_stale {
            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(0),
                    Constraint::Length(STATS_HEIGHT),
                    Constraint::Length(STALE_HEIGHT),
                ])
                .split(cols[0]);
            self.render_phases(frame, left[0]);
            self.render_stats(frame, left[1]);
            self.render_stale(frame, left[2]);
        } else if want_stats {
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

    fn render_stale(&self, frame: &mut Frame, area: Rect) {
        let dim = Style::default().fg(Color::DarkGray);
        let stale_style = Style::default().fg(Color::Yellow);
        let count_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);

        let total = self.stale_items.len();
        let take = total.min(STALE_PANEL_CAP);
        let mut lines: Vec<Line> = Vec::with_capacity(take + 1);
        for item in self.stale_items.iter().take(take) {
            let inner_width = area.width.saturating_sub(2) as usize;
            let prefix = format!(" {}x ", item.attempts);
            let avail = inner_width.saturating_sub(prefix.len());
            let truncated = truncate_for_panel(&item.text, avail);
            lines.push(Line::from(vec![
                Span::styled(prefix, count_style),
                Span::styled(truncated, stale_style),
            ]));
        }
        if total > take {
            let extra = total - take;
            lines.push(Line::from(Span::styled(format!(" +{extra} more"), dim)));
        }

        let mut title = format!(" stale items ({total}) ");
        if total == 0 {
            // Defensive: render_stale shouldn't be called with no items, but
            // if it is, keep the title accurate rather than rendering an
            // empty panel with a stale `(N)` count.
            title = " stale items ".to_string();
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(stale_style)
            .title(Span::styled(title, count_style));
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
        // Each raw line wraps to >=1 visual rows, so the last `inner_height`
        // raw lines always cover the visible pane. Take that slice as our
        // candidate set, then use `line_count` plus `scroll` to anchor the
        // most recent visual rows to the bottom (otherwise wrapped lines push
        // the latest output off the bottom edge and clip it).
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
        // line_count returns wrapped content rows + the block's top/bottom
        // borders (2 with Borders::ALL). Subtract those to get content rows,
        // then scroll past whatever doesn't fit so the tail stays visible.
        let total_with_borders = para.line_count(inner_width);
        let content_rows = total_with_borders.saturating_sub(2);
        let scroll_y = u16::try_from(content_rows.saturating_sub(inner_height)).unwrap_or(u16::MAX);
        let para = para.scroll((scroll_y, 0));
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

fn truncate_for_panel(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let collapsed: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else if max <= 1 {
        collapsed.chars().take(max).collect()
    } else {
        let head: String = collapsed.chars().take(max - 1).collect();
        format!("{head}…")
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
    } else if s.starts_with("[sweep:halt]") {
        Line::from(Span::styled(
            s.to_owned(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else if s.starts_with("[sweep]") {
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
        Activity::Implementer | Activity::SweepImplementer => Color::Cyan,
        Activity::Fixer(_) => Color::Yellow,
        Activity::Auditor | Activity::SweepAuditor | Activity::AuditorSkipped => Color::Blue,
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
    use crate::runner::{AuditContext, EventDiscriminants};
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
            "pitboss/play/20260430T120000Z",
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
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
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
            context: AuditContext {
                phase_id: pid("01"),
                kind: AuditContextKind::Phase,
            },
            attempt: 3,
        });
        assert_eq!(app.current_model(), "claude-sonnet-4-6");

        app.handle_event(Event::TestStarted);
        // Tests don't dispatch a role; chip falls back to implementer.
        assert_eq!(app.current_model(), "claude-opus-4-7");
    }

    #[test]
    fn render_keeps_latest_line_visible_when_earlier_lines_wrap() {
        // Regression: a wrapping line near the bottom of the output pane used
        // to push the most recent lines off the bottom edge, making them
        // invisible — looked like the view had scrolled up by accident.
        let started_at = fixed_started_at();
        let mut app = App::new(
            three_phase_plan(),
            fresh_state_at(started_at),
            fixture_agent(),
            fixture_usage_view(),
            Vec::new(),
        );
        app.set_now(started_at);
        // Push enough single-line entries to fill the pane, plus a long line
        // partway down that is guaranteed to wrap at the output pane's width.
        for i in 0..12 {
            app.handle_event(Event::AgentStdout(format!("line {i}")));
        }
        app.handle_event(Event::AgentStdout(
            "LONGWORD ".repeat(40).trim_end().to_string(),
        ));
        app.handle_event(Event::AgentStdout("MIDDLE".into()));
        app.handle_event(Event::AgentStdout("LATEST".into()));

        let snap = render_to_string(&app, 80, 20);
        // Both of the most recent entries must be in the rendered frame, even
        // though the wrapping LONGWORD line consumed extra visual rows.
        assert!(snap.contains("LATEST"), "rendered frame:\n{snap}");
        assert!(snap.contains("MIDDLE"), "rendered frame:\n{snap}");
    }

    #[test]
    fn output_buffer_drops_oldest_when_full() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
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

    /// Build a fully-populated event of every variant. A new variant added to
    /// `Event` without a matching arm here causes a compile error, which in
    /// turn forces a TUI handler review at the same time. This is the
    /// "exhaustiveness test" called for in phase 07.
    fn one_of_each_event() -> Vec<Event> {
        vec![
            Event::PhaseStarted {
                phase_id: pid("01"),
                title: "Project foundation".into(),
                attempt: 1,
            },
            Event::FixerStarted {
                phase_id: pid("01"),
                fixer_attempt: 1,
                attempt: 2,
            },
            Event::AuditorStarted {
                context: AuditContext {
                    phase_id: pid("01"),
                    kind: AuditContextKind::Phase,
                },
                attempt: 3,
            },
            Event::AuditorSkippedNoChanges {
                context: AuditContext {
                    phase_id: pid("01"),
                    kind: AuditContextKind::Phase,
                },
            },
            Event::AgentStdout("line".into()),
            Event::AgentStderr("err".into()),
            Event::AgentToolUse("Read".into()),
            Event::TestStarted,
            Event::TestFinished {
                passed: true,
                summary: "1 passed".into(),
            },
            Event::TestsSkipped,
            Event::PhaseCommitted {
                phase_id: pid("01"),
                commit: Some(crate::git::CommitId::new("deadbeef")),
            },
            Event::SweepStarted {
                after: pid("01"),
                items_pending: 3,
                attempt: 1,
            },
            Event::AuditorStarted {
                context: AuditContext {
                    phase_id: pid("01"),
                    kind: AuditContextKind::Sweep,
                },
                attempt: 2,
            },
            Event::AuditorSkippedNoChanges {
                context: AuditContext {
                    phase_id: pid("01"),
                    kind: AuditContextKind::Sweep,
                },
            },
            Event::SweepCompleted {
                after: pid("01"),
                resolved: 3,
                commit: Some(crate::git::CommitId::new("cafebabe")),
            },
            Event::DeferredItemStale {
                text: "polish error message".into(),
                attempts: 3,
            },
            Event::SweepHalted {
                after: pid("01"),
                reason: HaltReason::TestsFailed("boom".into()),
            },
            Event::PhaseHalted {
                phase_id: pid("01"),
                reason: HaltReason::TestsFailed("boom".into()),
            },
            Event::UsageUpdated(TokenUsage::default()),
            Event::RunFinished,
        ]
    }

    /// Structural exhaustiveness: every `Event` variant must appear in
    /// `one_of_each_event()`. The set of discriminants iterated by
    /// `EventDiscriminants::iter()` (generated by `strum::EnumDiscriminants`)
    /// is the source of truth — adding a new variant to `Event` without
    /// seeding it here trips this assertion at test time, replacing the
    /// hand-maintained `match` that used to live here.
    #[test]
    fn one_of_each_event_covers_every_variant() {
        use std::collections::HashSet;
        use strum::IntoEnumIterator;

        let seeded: HashSet<EventDiscriminants> = one_of_each_event()
            .iter()
            .map(EventDiscriminants::from)
            .collect();
        let expected: HashSet<EventDiscriminants> = EventDiscriminants::iter().collect();

        let missing: Vec<_> = expected.difference(&seeded).collect();
        assert!(
            missing.is_empty(),
            "one_of_each_event() is missing variants: {missing:?}",
        );
    }

    #[test]
    fn dispatch_every_event_variant_in_sequence_updates_state_as_expected() {
        // Drive one of every Event through `handle_event` and assert the
        // resulting App-state transitions. Catches a missing arm or an arm
        // that drops a side-effect.
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            Vec::new(),
        );
        for event in one_of_each_event() {
            app.handle_event(event);
        }
        // The last terminal event was RunFinished — activity ends in Done.
        assert_eq!(app.activity, Activity::Done);
        // The DeferredItemStale event seeded the stale list.
        assert_eq!(app.stale_items.len(), 1);
        assert_eq!(app.stale_items[0].text, "polish error message");
        assert_eq!(app.stale_items[0].attempts, 3);
        // Sweep state was cleared by SweepHalted (and again by RunFinished
        // not touching it).
        assert!(app.sweep_state.is_none());
        // Phase 01 ended up Failed because PhaseHalted was the last
        // phase-status event we sent for it.
        assert!(matches!(
            app.phase_status[&pid("01")],
            PhaseStatus::Failed(_)
        ));
        // Output buffer absorbed at least the commit, sweep, halt, and tests
        // lines; sanity check we got the expected style markers.
        let joined: String = app.output_lines().cloned().collect::<Vec<_>>().join("\n");
        assert!(joined.contains("[commit] phase 01"), "{joined}");
        assert!(joined.contains("[sweep] after phase 01"), "{joined}");
        assert!(joined.contains("[sweep] after 01"), "{joined}");
        assert!(joined.contains("[sweep:halt]"), "{joined}");
        assert!(joined.contains("[halt] phase 01"), "{joined}");
        assert!(joined.contains("[tests passed]"), "{joined}");
        assert!(joined.contains("[tests] no runner detected"), "{joined}");
    }

    #[test]
    fn sweep_started_sets_sweep_state_and_sweep_implementer_activity() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            Vec::new(),
        );
        app.handle_event(Event::PhaseStarted {
            phase_id: pid("01"),
            title: "Project foundation".into(),
            attempt: 1,
        });
        app.handle_event(Event::PhaseCommitted {
            phase_id: pid("01"),
            commit: Some(crate::git::CommitId::new("abc1234")),
        });
        app.handle_event(Event::SweepStarted {
            after: pid("01"),
            items_pending: 2,
            attempt: 2,
        });
        let sweep = app.sweep_state.clone().expect("sweep state set");
        assert_eq!(sweep.after, pid("01"));
        assert_eq!(sweep.attempt, 2);
        assert!(!sweep.in_auditor);
        assert_eq!(app.activity, Activity::SweepImplementer);
        assert_eq!(app.attempts.get(&pid("01")).copied(), Some(2));
    }

    #[test]
    fn sweep_auditor_started_flips_in_auditor_flag() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            Vec::new(),
        );
        app.handle_event(Event::SweepStarted {
            after: pid("01"),
            items_pending: 2,
            attempt: 1,
        });
        app.handle_event(Event::AuditorStarted {
            context: AuditContext {
                phase_id: pid("01"),
                kind: AuditContextKind::Sweep,
            },
            attempt: 2,
        });
        let sweep = app.sweep_state.clone().expect("sweep state set");
        assert!(sweep.in_auditor);
        assert_eq!(app.activity, Activity::SweepAuditor);
    }

    #[test]
    fn sweep_completed_clears_sweep_state_and_logs_resolved_line() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            Vec::new(),
        );
        app.handle_event(Event::SweepStarted {
            after: pid("01"),
            items_pending: 3,
            attempt: 1,
        });
        app.handle_event(Event::SweepCompleted {
            after: pid("01"),
            resolved: 3,
            commit: Some(crate::git::CommitId::new("cafebabe")),
        });
        assert!(app.sweep_state.is_none());
        let last = app.output.back().unwrap();
        assert!(last.contains("3 items resolved"), "got: {last}");
    }

    #[test]
    fn sweep_halted_clears_sweep_state_and_sets_halted_activity() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            Vec::new(),
        );
        app.handle_event(Event::SweepStarted {
            after: pid("01"),
            items_pending: 1,
            attempt: 1,
        });
        app.handle_event(Event::SweepHalted {
            after: pid("01"),
            reason: HaltReason::TestsFailed("boom".into()),
        });
        assert!(app.sweep_state.is_none());
        assert!(matches!(app.activity, Activity::Halted(_)));
        let last = app.output.back().unwrap();
        assert!(last.starts_with("[sweep:halt]"), "got: {last}");
    }

    #[test]
    fn phase_started_after_sweep_clears_sweep_state() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            Vec::new(),
        );
        app.handle_event(Event::SweepStarted {
            after: pid("01"),
            items_pending: 1,
            attempt: 1,
        });
        app.handle_event(Event::PhaseStarted {
            phase_id: pid("02"),
            title: "Domain types".into(),
            attempt: 1,
        });
        assert!(app.sweep_state.is_none());
    }

    #[test]
    fn deferred_item_stale_event_inserts_and_updates_panel_list() {
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            Vec::new(),
        );
        app.handle_event(Event::DeferredItemStale {
            text: "polish error message".into(),
            attempts: 3,
        });
        app.handle_event(Event::DeferredItemStale {
            text: "drop unused stub".into(),
            attempts: 5,
        });
        // Re-emit for the same item with a higher count: expected to update
        // in place and rebubble to the top of the sort.
        app.handle_event(Event::DeferredItemStale {
            text: "polish error message".into(),
            attempts: 6,
        });
        assert_eq!(app.stale_items.len(), 2);
        assert_eq!(app.stale_items[0].text, "polish error message");
        assert_eq!(app.stale_items[0].attempts, 6);
        assert_eq!(app.stale_items[1].text, "drop unused stub");
        assert_eq!(app.stale_items[1].attempts, 5);
    }

    #[test]
    fn stale_items_hydrated_from_constructor() {
        let app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            vec![
                StaleItem {
                    text: "polish error message".into(),
                    attempts: 3,
                },
                StaleItem {
                    text: "drop unused stub".into(),
                    attempts: 4,
                },
            ],
        );
        assert_eq!(app.stale_items.len(), 2);
    }

    #[test]
    fn out_of_order_sweep_completed_without_started_does_not_panic() {
        // Defensive: the runner should never emit SweepCompleted before
        // SweepStarted, but if it did (e.g., a desync from an external
        // subscriber injecting events in tests), the App must not panic
        // and must surface a `[tui:warn]` line so the operator notices
        // (the underlying tracing::debug is invisible under the alternate
        // screen).
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            Vec::new(),
        );
        app.handle_event(Event::SweepCompleted {
            after: pid("01"),
            resolved: 0,
            commit: None,
        });
        assert!(app.sweep_state.is_none());
        let lines: Vec<String> = app.output_lines().cloned().collect();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("[tui:warn] SweepCompleted(01) without SweepStarted(01)")),
            "expected a [tui:warn] line; output was: {lines:?}",
        );
        assert!(
            lines.iter().any(|l| l.contains("0 items resolved")),
            "expected resolved-count line; output was: {lines:?}",
        );
    }

    #[test]
    fn out_of_order_sweep_started_without_phase_committed_does_not_panic() {
        // Defensive: a SweepStarted before any PhaseCommitted (or a fresh
        // run with no completed phases) sets sweep_state without panicking,
        // and surfaces a `[tui:warn]` line so the desync is visible at
        // runtime instead of buried in `RUST_LOG`.
        let mut app = App::new(
            three_phase_plan(),
            fresh_state(),
            fixture_agent(),
            UsageView::default(),
            Vec::new(),
        );
        app.handle_event(Event::SweepStarted {
            after: pid("01"),
            items_pending: 1,
            attempt: 1,
        });
        let sweep = app.sweep_state.clone().expect("sweep state set");
        assert_eq!(sweep.after, pid("01"));
        let lines: Vec<String> = app.output_lines().cloned().collect();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("[tui:warn] SweepStarted(01) without PhaseCommitted(01)")),
            "expected a [tui:warn] line; output was: {lines:?}",
        );
    }

    fn sweep_after_first_phase_apparatus(now_offset_seconds: i64) -> App {
        let started_at = fixed_started_at();
        let mut app = App::new(
            three_phase_plan(),
            fresh_state_at(started_at),
            fixture_agent(),
            fixture_usage_view(),
            Vec::new(),
        );
        app.set_now(started_at + chrono::Duration::seconds(now_offset_seconds));
        app.handle_event(Event::PhaseStarted {
            phase_id: pid("01"),
            title: "Project foundation".into(),
            attempt: 1,
        });
        app.handle_event(Event::PhaseCommitted {
            phase_id: pid("01"),
            commit: Some(crate::git::CommitId::new("abc1234")),
        });
        app
    }

    #[test]
    fn render_sweep_in_flight() {
        let mut app = sweep_after_first_phase_apparatus(120);
        app.handle_event(Event::SweepStarted {
            after: pid("01"),
            items_pending: 3,
            attempt: 2,
        });
        app.handle_event(Event::AgentStdout("Reading deferred.md".into()));
        app.handle_event(Event::AgentStdout("Editing src/lib.rs".into()));
        let snap = render_to_string(&app, 120, 30);
        insta::assert_snapshot!("sweep_in_flight", snap);
    }

    #[test]
    fn render_sweep_auditor() {
        let mut app = sweep_after_first_phase_apparatus(140);
        app.handle_event(Event::SweepStarted {
            after: pid("01"),
            items_pending: 3,
            attempt: 2,
        });
        app.handle_event(Event::AuditorStarted {
            context: AuditContext {
                phase_id: pid("01"),
                kind: AuditContextKind::Sweep,
            },
            attempt: 3,
        });
        app.handle_event(Event::AgentStdout("Reviewing sweep diff".into()));
        let snap = render_to_string(&app, 120, 30);
        insta::assert_snapshot!("sweep_auditor", snap);
    }

    #[test]
    fn render_sweep_completed() {
        let mut app = sweep_after_first_phase_apparatus(180);
        app.handle_event(Event::SweepStarted {
            after: pid("01"),
            items_pending: 3,
            attempt: 2,
        });
        app.handle_event(Event::SweepCompleted {
            after: pid("01"),
            resolved: 3,
            commit: Some(crate::git::CommitId::new("def5678")),
        });
        let snap = render_to_string(&app, 120, 30);
        insta::assert_snapshot!("sweep_completed", snap);
    }

    #[test]
    fn render_sweep_halted() {
        let mut app = sweep_after_first_phase_apparatus(200);
        app.handle_event(Event::SweepStarted {
            after: pid("01"),
            items_pending: 2,
            attempt: 2,
        });
        app.handle_event(Event::AuditorStarted {
            context: AuditContext {
                phase_id: pid("01"),
                kind: AuditContextKind::Sweep,
            },
            attempt: 3,
        });
        app.handle_event(Event::SweepHalted {
            after: pid("01"),
            reason: HaltReason::TestsFailed("12 failed".into()),
        });
        let snap = render_to_string(&app, 120, 30);
        insta::assert_snapshot!("sweep_halted", snap);
    }

    #[test]
    fn render_stale_items_panel() {
        let started_at = fixed_started_at();
        let mut app = App::new(
            three_phase_plan(),
            fresh_state_at(started_at),
            fixture_agent(),
            fixture_usage_view(),
            vec![
                StaleItem {
                    text: "polish error message in fixer".into(),
                    attempts: 4,
                },
                StaleItem {
                    text: "drop unused stub from prompts".into(),
                    attempts: 3,
                },
            ],
        );
        app.set_now(started_at + chrono::Duration::seconds(300));
        app.handle_event(Event::PhaseStarted {
            phase_id: pid("02"),
            title: "Domain types".into(),
            attempt: 1,
        });
        let snap = render_to_string(&app, 120, 30);
        insta::assert_snapshot!("stale_items_panel", snap);
    }
}
