//! `ratatui` dashboard subscribed to runner events.
//!
//! [`App`] owns the visible state and folds [`crate::runner::Event`]s into
//! it; [`run`] is the entry point the CLI calls when `--tui` is set. The
//! integration is purely additive: the runner publishes on its broadcast
//! channel exactly as it does for the plain logger, and this module
//! subscribes alongside it.
//!
//! Quit behavior. The TUI runs concurrently with [`crate::runner::Runner::run`].
//! When the user hits `q` or `a` the host loop drops the runner future via
//! [`tokio::select`], which cancels every in-flight `await` chain inside the
//! runner — including the agent dispatch, which honors its own
//! [`tokio_util::sync::CancellationToken`]. The terminal is always restored,
//! even on panic or early return.

mod app;
pub mod grind;

pub use app::{Activity, AgentDisplay, App, PhaseStatus, UsageView, OUTPUT_BUFFER_LINES};

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::broadcast;
use tokio::time::sleep;

use crate::agent::backend::BackendKind;
use crate::agent::Agent;
use crate::config::{BackendOverrides, Config};
use crate::git::Git;
use crate::runner::{Event, RunSummary, Runner};

/// Drive a [`Runner`] with the TUI dashboard attached.
///
/// Subscribes to the runner's event stream, sets up the terminal in
/// alternate-screen / raw mode, and runs the input + render loop concurrently
/// with [`Runner::run`]. Returns whatever the runner returned, or `None`
/// when the user quit before the runner finished. The terminal is always
/// restored before this function returns — including on early-return
/// errors and unwinding panics — so mouse capture never outlives the run.
pub async fn run<A, G>(runner: &mut Runner<A, G>) -> Result<Option<RunSummary>>
where
    A: Agent + Send + Sync + 'static,
    G: Git + Send + Sync + 'static,
{
    let plan = runner.plan().clone();
    let state = runner.state().clone();
    let agent_display = build_agent_display(runner.config(), runner.agent().name());
    let usage_view = build_usage_view(runner.config());
    let stale_items = runner.stale_items();
    let rx = runner.subscribe();

    let mut guard = TerminalGuard::setup().context("tui: setting up terminal")?;
    let app = App::new(plan, state, agent_display, usage_view, stale_items);

    let outcome = tokio::select! {
        biased;
        result = run_loop(guard.terminal(), app, rx) => Outcome::User(result?),
        result = runner.run() => Outcome::Runner(result?),
    };

    guard.restore().context("tui: restoring terminal")?;

    match outcome {
        Outcome::Runner(summary) => Ok(Some(summary)),
        Outcome::User(UserOutcome::Quit) => Ok(None),
        Outcome::User(UserOutcome::ChannelClosed) => Ok(None),
    }
}

/// Resolve the per-role model strings the header should display.
///
/// A `[agent.<backend>] model = "..."` override wins over `[models].<role>`
/// when set — that mirrors the precedence the four backend adapters apply at
/// dispatch time (`with_model_override` beats `req.model`). An unknown
/// backend string in `cfg.agent.backend` is treated as the default
/// (Claude Code) for display purposes; the runner itself surfaces the parse
/// error before the TUI ever runs.
fn build_agent_display(cfg: &Config, agent_name: &str) -> AgentDisplay {
    let kind = cfg
        .agent
        .backend
        .as_deref()
        .and_then(|s| s.parse::<BackendKind>().ok())
        .unwrap_or_default();
    let overrides: &BackendOverrides = match kind {
        BackendKind::ClaudeCode => &cfg.agent.claude_code,
        BackendKind::Codex => &cfg.agent.codex,
        BackendKind::Aider => &cfg.agent.aider,
        BackendKind::Gemini => &cfg.agent.gemini,
    };
    let resolve = |role_default: &str| {
        overrides
            .model
            .as_deref()
            .unwrap_or(role_default)
            .to_string()
    };
    AgentDisplay {
        agent_name: agent_name.to_string(),
        implementer_model: resolve(&cfg.models.implementer),
        fixer_model: resolve(&cfg.models.fixer),
        auditor_model: resolve(&cfg.models.auditor),
    }
}

/// Build the [`UsageView`] the session-stats panel uses to price running
/// token totals. The role/model mapping mirrors the precedence in
/// [`build_agent_display`]: a `[agent.<backend>] model = "..."` override wins
/// over `[models].<role>` so the panel costs each role against the same model
/// the dispatcher actually sends.
fn build_usage_view(cfg: &Config) -> UsageView {
    let kind = cfg
        .agent
        .backend
        .as_deref()
        .and_then(|s| s.parse::<BackendKind>().ok())
        .unwrap_or_default();
    let overrides: &BackendOverrides = match kind {
        BackendKind::ClaudeCode => &cfg.agent.claude_code,
        BackendKind::Codex => &cfg.agent.codex,
        BackendKind::Aider => &cfg.agent.aider,
        BackendKind::Gemini => &cfg.agent.gemini,
    };
    let resolve = |role_default: &str| {
        overrides
            .model
            .as_deref()
            .unwrap_or(role_default)
            .to_string()
    };
    let role_models = vec![
        ("planner".to_string(), resolve(&cfg.models.planner)),
        ("implementer".to_string(), resolve(&cfg.models.implementer)),
        ("fixer".to_string(), resolve(&cfg.models.fixer)),
        ("auditor".to_string(), resolve(&cfg.models.auditor)),
    ];
    UsageView {
        role_models,
        pricing: cfg.budgets.pricing.clone(),
    }
}

enum Outcome {
    Runner(RunSummary),
    User(UserOutcome),
}

enum UserOutcome {
    /// User pressed q or a.
    Quit,
    /// Runner dropped the broadcast channel (run completed via the other arm).
    /// Reported here only when this loop wins the race; in practice the
    /// runner arm wins and this is unreachable.
    ChannelClosed,
}

/// RAII wrapper around the terminal setup/teardown.
///
/// `Drop` does a best-effort restore so an unwinding panic or an early-return
/// `?` inside [`run`] does not leak raw mode / mouse capture into the user's
/// shell — that would cause the terminal to echo SGR mouse-tracking escape
/// sequences as visible input on every mouse movement after pitboss exits.
/// The explicit [`Self::restore`] path surfaces teardown errors when nothing
/// else has gone wrong; the `Drop` path swallows them because we cannot
/// usefully report errors during unwinding.
pub(crate) struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    active: bool,
}

impl TerminalGuard {
    pub(crate) fn setup() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self {
                terminal,
                active: true,
            }),
            Err(e) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
                let _ = disable_raw_mode();
                Err(e.into())
            }
        }
    }

    pub(crate) fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<io::Stdout>> {
        &mut self.terminal
    }

    pub(crate) fn restore(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        self.terminal.show_cursor()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

/// Frame interval. Aggressive enough for streaming agent output to feel
/// live; loose enough not to thrash the terminal when nothing is happening.
pub(crate) const TICK_INTERVAL: Duration = Duration::from_millis(80);

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
    mut events: broadcast::Receiver<Event>,
) -> Result<UserOutcome> {
    let mut input = EventStream::new();
    terminal.draw(|f| app.render(f))?;

    loop {
        tokio::select! {
            biased;
            // Drain runner events as they arrive — best-effort, lag tolerated.
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
            // Pump terminal input.
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
            // Cap the frame rate so a quiet run still re-renders periodically.
            _ = sleep(TICK_INTERVAL) => {}
        }

        terminal.draw(|f| app.render(f))?;

        if app.quit_requested() {
            return Ok(UserOutcome::Quit);
        }
    }
}

/// Returns `true` when the key requests an immediate quit.
fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Phase, PhaseId, Plan};
    use crate::state::RunState;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    fn fixture_app() -> App {
        let plan = Plan::new(
            pid("01"),
            vec![Phase {
                id: pid("01"),
                title: "first".into(),
                body: String::new(),
            }],
        );
        let state = RunState::new("rid", "branch", pid("01"));
        let agent_display = AgentDisplay {
            agent_name: "claude-code".into(),
            implementer_model: "claude-opus-4-7".into(),
            fixer_model: "claude-sonnet-4-6".into(),
            auditor_model: "claude-sonnet-4-6".into(),
        };
        App::new(plan, state, agent_display, UsageView::default(), Vec::new())
    }

    #[test]
    fn q_requests_quit() {
        let mut app = fixture_app();
        let quit = handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::empty());
        assert!(quit);
    }

    #[test]
    fn a_requests_quit() {
        let mut app = fixture_app();
        let quit = handle_key(&mut app, KeyCode::Char('a'), KeyModifiers::empty());
        assert!(quit);
    }

    #[test]
    fn ctrl_c_requests_quit() {
        let mut app = fixture_app();
        let quit = handle_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(quit);
    }

    #[test]
    fn p_toggles_pause_without_quitting() {
        let mut app = fixture_app();
        assert!(!app.is_paused());
        let quit = handle_key(&mut app, KeyCode::Char('p'), KeyModifiers::empty());
        assert!(!quit);
        assert!(app.is_paused());
        let quit = handle_key(&mut app, KeyCode::Char('p'), KeyModifiers::empty());
        assert!(!quit);
        assert!(!app.is_paused());
    }

    #[test]
    fn unknown_key_is_a_no_op() {
        let mut app = fixture_app();
        let quit = handle_key(&mut app, KeyCode::Char('x'), KeyModifiers::empty());
        assert!(!quit);
        assert!(!app.is_paused());
    }

    #[test]
    fn build_agent_display_uses_role_models_when_no_backend_override() {
        // Bare `[models]` with no per-backend `model` override: every role's
        // header chip resolves to the role-level model verbatim.
        let mut cfg = Config::default();
        cfg.models.implementer = "claude-opus-4-7".into();
        cfg.models.fixer = "claude-sonnet-4-6".into();
        cfg.models.auditor = "claude-haiku-4-5".into();
        let display = build_agent_display(&cfg, "claude-code");
        assert_eq!(display.agent_name, "claude-code");
        assert_eq!(display.implementer_model, "claude-opus-4-7");
        assert_eq!(display.fixer_model, "claude-sonnet-4-6");
        assert_eq!(display.auditor_model, "claude-haiku-4-5");
    }

    #[test]
    fn build_agent_display_applies_backend_model_override_to_every_role() {
        // The `[agent.<backend>] model = "..."` override wins over the
        // role-level `[models]` table at dispatch time, so the header chip
        // must follow the same precedence — otherwise the displayed model
        // would lie about what the backend is actually invoking.
        let mut cfg = Config::default();
        cfg.agent.backend = Some("codex".into());
        cfg.agent.codex.model = Some("gpt-5-codex".into());
        cfg.models.implementer = "claude-opus-4-7".into();
        cfg.models.fixer = "claude-sonnet-4-6".into();
        cfg.models.auditor = "claude-haiku-4-5".into();
        let display = build_agent_display(&cfg, "codex");
        assert_eq!(display.implementer_model, "gpt-5-codex");
        assert_eq!(display.fixer_model, "gpt-5-codex");
        assert_eq!(display.auditor_model, "gpt-5-codex");
    }

    #[test]
    fn build_agent_display_falls_back_to_default_backend_for_unknown_string() {
        // An invalid `agent.backend` string at the TUI layer is non-fatal —
        // the runner has already accepted the config by the time we render,
        // and the header just degrades to the default backend's overrides
        // (which are empty by default, so the `[models]` table wins).
        let mut cfg = Config::default();
        cfg.agent.backend = Some("not-a-backend".into());
        cfg.models.implementer = "x-impl".into();
        let display = build_agent_display(&cfg, "claude-code");
        assert_eq!(display.implementer_model, "x-impl");
    }

    #[test]
    fn build_agent_display_unused_backend_overrides_do_not_leak() {
        // Setting `[agent.aider] model = ...` while running with
        // `backend = "codex"` must not leak the aider override into the
        // header — only the *active* backend's overrides apply.
        let mut cfg = Config::default();
        cfg.agent.backend = Some("codex".into());
        cfg.agent.aider.model = Some("aider-only-model".into());
        cfg.models.implementer = "role-default".into();
        let display = build_agent_display(&cfg, "codex");
        assert_eq!(display.implementer_model, "role-default");
    }
}
