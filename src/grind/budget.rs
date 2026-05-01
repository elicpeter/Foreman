//! Budget tracking and exit-code policy for `pitboss grind`.
//!
//! The runner consults [`BudgetTracker::check`] before each session dispatch
//! and again after every session completes. Once a budget trips, the runner
//! finishes the in-flight session (if any), records the exhaustion reason, and
//! exits with [`ExitCode::BudgetExhausted`].
//!
//! Budgets are resolved from three layered sources, with later sources
//! overriding earlier ones:
//!
//! 1. `[grind.budgets]` in `config.toml` — workspace-wide defaults.
//! 2. The selected plan's `PlanBudgets` block — plan-specific overrides.
//! 3. CLI flags (`--max-iterations`, `--until`, `--max-cost`, `--max-tokens`).
//!
//! The tracker also counts consecutive failed sessions; once
//! `consecutive_failure_limit` is reached the runner trips the
//! [`ExitCode::ConsecutiveFailures`] escape valve. Successful sessions reset
//! the counter; aborts and dirty sessions leave it untouched.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::run_dir::{SessionRecord, SessionStatus};
use crate::config::{Config, ModelPricing};

use super::plan::PlanBudgets;

/// Persisted snapshot of a [`BudgetTracker`]'s aggregated counters. Written
/// into `state.json` so a resumed run picks up the same cumulative spend.
/// Pure data — no clock, no policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    /// Sessions dispatched so far.
    pub iterations: u32,
    /// Cumulative input tokens billed across every recorded session.
    pub tokens_input: u64,
    /// Cumulative output tokens billed across every recorded session.
    pub tokens_output: u64,
    /// Cumulative cost in USD.
    pub cost_usd: f64,
    /// Current run of consecutive failed sessions (`Error` / `Timeout`).
    pub consecutive_failures: u32,
}

// `ExitCode` lives in `crate::cli::exit_code` now — it covers every subcommand,
// not just grind. Re-exported below so existing `crate::grind::ExitCode`
// imports keep working.
pub use crate::cli::exit_code::ExitCode;

/// Which budget tripped. Carried by [`BudgetCheck::Exhausted`] so the runner
/// can render a precise log line.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetReason {
    /// `max_iterations` reached. `count` is the session count at the moment
    /// the cap was hit; `cap` echoes the configured limit.
    MaxIterations {
        /// Sessions dispatched so far.
        count: u32,
        /// Configured cap.
        cap: u32,
    },
    /// `until` reached or passed.
    Until {
        /// Wall clock at the time of the check.
        now: DateTime<Utc>,
        /// Configured cutoff.
        until: DateTime<Utc>,
    },
    /// `max_tokens` cumulative cap reached.
    MaxTokens {
        /// Cumulative tokens (input + output) so far.
        used: u64,
        /// Configured cap.
        cap: u64,
    },
    /// `max_cost_usd` cumulative cap reached.
    MaxCost {
        /// Cumulative cost in USD so far.
        used: f64,
        /// Configured cap.
        cap: f64,
    },
}

impl std::fmt::Display for BudgetReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BudgetReason::MaxIterations { count, cap } => {
                write!(f, "max-iterations reached: {count} >= {cap}")
            }
            BudgetReason::Until { now, until } => write!(
                f,
                "until reached: {} >= {}",
                now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            BudgetReason::MaxTokens { used, cap } => {
                write!(f, "max-tokens reached: {used} >= {cap}")
            }
            BudgetReason::MaxCost { used, cap } => {
                write!(f, "max-cost reached: ${used:.4} >= ${cap:.4}")
            }
        }
    }
}

/// Result of [`BudgetTracker::check`].
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetCheck {
    /// All budgets still have headroom — keep dispatching.
    Ok,
    /// A budget tripped — finish any in-flight session and exit with
    /// [`ExitCode::BudgetExhausted`].
    Exhausted(BudgetReason),
}

/// Aggregating budget tracker. Sessions feed
/// [`BudgetTracker::record_session`] after they resolve; the runner calls
/// [`BudgetTracker::check`] before each dispatch and again after each
/// completion.
///
/// The tracker is intentionally pure — no IO, no clock injection except via
/// the `now` parameter on [`BudgetTracker::check_with_now`]. Tests inject a
/// fixed instant; production calls [`BudgetTracker::check`] which reads
/// [`Utc::now`].
#[derive(Debug, Clone)]
pub struct BudgetTracker {
    budgets: PlanBudgets,
    consecutive_failure_limit: u32,
    iterations: u32,
    tokens_input: u64,
    tokens_output: u64,
    cost_usd: f64,
    consecutive_failures: u32,
}

impl BudgetTracker {
    /// Build a fresh tracker. `consecutive_failure_limit` of zero is treated
    /// as "disabled" (the escape valve never fires); callers that want
    /// `consecutive_failure_limit = 0` to mean "halt on first failure" should
    /// pass `1` instead.
    pub fn new(budgets: PlanBudgets, consecutive_failure_limit: u32) -> Self {
        Self {
            budgets,
            consecutive_failure_limit,
            iterations: 0,
            tokens_input: 0,
            tokens_output: 0,
            cost_usd: 0.0,
            consecutive_failures: 0,
        }
    }

    /// Build a tracker pre-loaded with a previously persisted [`BudgetSnapshot`].
    /// Used by `pitboss grind --resume` to keep cumulative counters aligned
    /// with sessions that landed before the kill.
    pub fn from_snapshot(
        budgets: PlanBudgets,
        consecutive_failure_limit: u32,
        snapshot: BudgetSnapshot,
    ) -> Self {
        Self {
            budgets,
            consecutive_failure_limit,
            iterations: snapshot.iterations,
            tokens_input: snapshot.tokens_input,
            tokens_output: snapshot.tokens_output,
            cost_usd: snapshot.cost_usd,
            consecutive_failures: snapshot.consecutive_failures,
        }
    }

    /// Capture the tracker's aggregated counters into a [`BudgetSnapshot`].
    pub fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            iterations: self.iterations,
            tokens_input: self.tokens_input,
            tokens_output: self.tokens_output,
            cost_usd: self.cost_usd,
            consecutive_failures: self.consecutive_failures,
        }
    }

    /// Feed a finished session into the tracker. Increments the iteration
    /// count, folds in tokens and cost, and updates the consecutive-failure
    /// counter.
    ///
    /// `Ok` and `Dirty` reset the consecutive counter; `Error` and `Timeout`
    /// increment it. `Aborted` leaves the counter untouched — the run is
    /// stopping anyway, and aborts are not the kind of failure the escape
    /// valve is meant to guard against.
    pub fn record_session(&mut self, record: &SessionRecord) {
        self.iterations = self.iterations.saturating_add(1);
        self.tokens_input = self.tokens_input.saturating_add(record.tokens.input);
        self.tokens_output = self.tokens_output.saturating_add(record.tokens.output);
        self.cost_usd += record.cost_usd;

        match record.status {
            SessionStatus::Ok | SessionStatus::Dirty => {
                self.consecutive_failures = 0;
            }
            SessionStatus::Error | SessionStatus::Timeout => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            }
            SessionStatus::Aborted => {}
        }
    }

    /// Number of sessions dispatched so far.
    pub fn iterations(&self) -> u32 {
        self.iterations
    }

    /// Cumulative token count (input + output) across every recorded session.
    pub fn total_tokens(&self) -> u64 {
        self.tokens_input.saturating_add(self.tokens_output)
    }

    /// Cumulative cost in USD across every recorded session.
    pub fn total_cost_usd(&self) -> f64 {
        self.cost_usd
    }

    /// Current run of consecutive failed sessions.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// `true` once the consecutive-failure counter meets or exceeds the
    /// configured limit. Disabled (always `false`) when the limit is zero.
    pub fn consecutive_failure_limit_reached(&self) -> bool {
        self.consecutive_failure_limit > 0
            && self.consecutive_failures >= self.consecutive_failure_limit
    }

    /// Production [`BudgetTracker::check_with_now`] using [`Utc::now`].
    pub fn check(&self) -> BudgetCheck {
        self.check_with_now(Utc::now())
    }

    /// Compare current totals against the configured budgets. Returns
    /// [`BudgetCheck::Exhausted`] for the first cap that trips, in this
    /// order: max-iterations, until, max-tokens, max-cost.
    pub fn check_with_now(&self, now: DateTime<Utc>) -> BudgetCheck {
        if let Some(cap) = self.budgets.max_iterations {
            if self.iterations >= cap {
                return BudgetCheck::Exhausted(BudgetReason::MaxIterations {
                    count: self.iterations,
                    cap,
                });
            }
        }
        if let Some(until) = self.budgets.until {
            if now >= until {
                return BudgetCheck::Exhausted(BudgetReason::Until { now, until });
            }
        }
        if let Some(cap) = self.budgets.max_tokens {
            let used = self.total_tokens();
            if used >= cap {
                return BudgetCheck::Exhausted(BudgetReason::MaxTokens { used, cap });
            }
        }
        if let Some(cap) = self.budgets.max_cost_usd {
            if self.cost_usd >= cap {
                return BudgetCheck::Exhausted(BudgetReason::MaxCost {
                    used: self.cost_usd,
                    cap,
                });
            }
        }
        BudgetCheck::Ok
    }
}

/// Resolve the budgets a run should enforce. Order of precedence (later wins):
/// 1. `[grind.budgets]` from `config.toml`.
/// 2. The plan's `PlanBudgets`.
/// 3. CLI flag overrides.
pub fn resolve_budgets(
    config_budgets: &PlanBudgets,
    plan_budgets: &PlanBudgets,
    cli: &PlanBudgets,
) -> PlanBudgets {
    let mut out = config_budgets.clone();
    if plan_budgets.max_iterations.is_some() {
        out.max_iterations = plan_budgets.max_iterations;
    }
    if plan_budgets.until.is_some() {
        out.until = plan_budgets.until;
    }
    if plan_budgets.max_tokens.is_some() {
        out.max_tokens = plan_budgets.max_tokens;
    }
    if plan_budgets.max_cost_usd.is_some() {
        out.max_cost_usd = plan_budgets.max_cost_usd;
    }
    if cli.max_iterations.is_some() {
        out.max_iterations = cli.max_iterations;
    }
    if cli.until.is_some() {
        out.until = cli.until;
    }
    if cli.max_tokens.is_some() {
        out.max_tokens = cli.max_tokens;
    }
    if cli.max_cost_usd.is_some() {
        out.max_cost_usd = cli.max_cost_usd;
    }
    out
}

/// Compute the USD cost of a single agent dispatch. Uses
/// [`Config::budgets`]'s `pricing` table keyed by `model`. Returns `0.0` when
/// the model is missing from the table — pitboss never fabricates a price.
pub fn session_cost_usd(config: &Config, model: &str, input: u64, output: u64) -> f64 {
    let Some(price) = config.budgets.pricing.get(model) else {
        return 0.0;
    };
    let p: ModelPricing = *price;
    p.cost_usd(input, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TokenUsage;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn record(
        seq: u32,
        status: SessionStatus,
        input: u64,
        output: u64,
        cost: f64,
    ) -> SessionRecord {
        let started: DateTime<Utc> = "2026-04-30T18:00:00Z".parse().unwrap();
        let ended: DateTime<Utc> = "2026-04-30T18:01:00Z".parse().unwrap();
        SessionRecord {
            seq,
            run_id: "rid".into(),
            prompt: "p".into(),
            started_at: started,
            ended_at: ended,
            status,
            summary: None,
            commit: None,
            tokens: TokenUsage {
                input,
                output,
                by_role: HashMap::new(),
            },
            cost_usd: cost,
            transcript_path: PathBuf::from("transcripts/session-0001.log"),
        }
    }

    #[test]
    fn empty_tracker_checks_ok() {
        let t = BudgetTracker::new(PlanBudgets::default(), 3);
        assert_eq!(t.check(), BudgetCheck::Ok);
        assert_eq!(t.iterations(), 0);
        assert_eq!(t.total_tokens(), 0);
        assert_eq!(t.total_cost_usd(), 0.0);
        assert_eq!(t.consecutive_failures(), 0);
    }

    #[test]
    fn max_iterations_trips_after_record() {
        let mut t = BudgetTracker::new(
            PlanBudgets {
                max_iterations: Some(2),
                ..Default::default()
            },
            3,
        );
        assert_eq!(t.check(), BudgetCheck::Ok);
        t.record_session(&record(1, SessionStatus::Ok, 0, 0, 0.0));
        assert_eq!(t.check(), BudgetCheck::Ok);
        t.record_session(&record(2, SessionStatus::Ok, 0, 0, 0.0));
        assert_eq!(
            t.check(),
            BudgetCheck::Exhausted(BudgetReason::MaxIterations { count: 2, cap: 2 })
        );
    }

    #[test]
    fn until_trips_when_clock_passes_cutoff() {
        let until: DateTime<Utc> = "2026-04-30T19:00:00Z".parse().unwrap();
        let t = BudgetTracker::new(
            PlanBudgets {
                until: Some(until),
                ..Default::default()
            },
            3,
        );
        let before: DateTime<Utc> = "2026-04-30T18:30:00Z".parse().unwrap();
        let after: DateTime<Utc> = "2026-04-30T19:00:01Z".parse().unwrap();
        assert_eq!(t.check_with_now(before), BudgetCheck::Ok);
        match t.check_with_now(after) {
            BudgetCheck::Exhausted(BudgetReason::Until { .. }) => {}
            other => panic!("expected Until exhaustion, got {other:?}"),
        }
    }

    #[test]
    fn max_tokens_trips_at_or_above_cap() {
        let mut t = BudgetTracker::new(
            PlanBudgets {
                max_tokens: Some(1000),
                ..Default::default()
            },
            3,
        );
        t.record_session(&record(1, SessionStatus::Ok, 400, 200, 0.0));
        assert_eq!(t.check(), BudgetCheck::Ok);
        t.record_session(&record(2, SessionStatus::Ok, 400, 0, 0.0));
        match t.check() {
            BudgetCheck::Exhausted(BudgetReason::MaxTokens { used, cap }) => {
                assert_eq!(used, 1000);
                assert_eq!(cap, 1000);
            }
            other => panic!("expected MaxTokens, got {other:?}"),
        }
    }

    #[test]
    fn max_cost_trips_at_or_above_cap() {
        let mut t = BudgetTracker::new(
            PlanBudgets {
                max_cost_usd: Some(1.0),
                ..Default::default()
            },
            3,
        );
        t.record_session(&record(1, SessionStatus::Ok, 0, 0, 0.4));
        t.record_session(&record(2, SessionStatus::Ok, 0, 0, 0.6));
        match t.check() {
            BudgetCheck::Exhausted(BudgetReason::MaxCost { used, cap }) => {
                assert!((used - 1.0).abs() < 1e-9);
                assert_eq!(cap, 1.0);
            }
            other => panic!("expected MaxCost, got {other:?}"),
        }
    }

    #[test]
    fn consecutive_failures_reset_on_success() {
        let mut t = BudgetTracker::new(PlanBudgets::default(), 3);
        t.record_session(&record(1, SessionStatus::Error, 0, 0, 0.0));
        t.record_session(&record(2, SessionStatus::Timeout, 0, 0, 0.0));
        assert_eq!(t.consecutive_failures(), 2);
        assert!(!t.consecutive_failure_limit_reached());
        t.record_session(&record(3, SessionStatus::Ok, 0, 0, 0.0));
        assert_eq!(t.consecutive_failures(), 0);
        t.record_session(&record(4, SessionStatus::Error, 0, 0, 0.0));
        t.record_session(&record(5, SessionStatus::Error, 0, 0, 0.0));
        t.record_session(&record(6, SessionStatus::Error, 0, 0, 0.0));
        assert!(t.consecutive_failure_limit_reached());
    }

    #[test]
    fn dirty_does_not_count_as_failure() {
        let mut t = BudgetTracker::new(PlanBudgets::default(), 3);
        t.record_session(&record(1, SessionStatus::Error, 0, 0, 0.0));
        t.record_session(&record(2, SessionStatus::Dirty, 0, 0, 0.0));
        assert_eq!(t.consecutive_failures(), 0);
    }

    #[test]
    fn aborted_leaves_counter_alone() {
        let mut t = BudgetTracker::new(PlanBudgets::default(), 3);
        t.record_session(&record(1, SessionStatus::Error, 0, 0, 0.0));
        t.record_session(&record(2, SessionStatus::Aborted, 0, 0, 0.0));
        assert_eq!(t.consecutive_failures(), 1);
    }

    #[test]
    fn consecutive_failure_limit_zero_disables_escape_valve() {
        let mut t = BudgetTracker::new(PlanBudgets::default(), 0);
        for seq in 1..=10 {
            t.record_session(&record(seq, SessionStatus::Error, 0, 0, 0.0));
        }
        assert!(!t.consecutive_failure_limit_reached());
    }

    #[test]
    fn iteration_check_takes_priority_over_token_check() {
        // Same input that trips both — the iteration cap is reported because
        // that's the cheaper / cleaner halt reason and the order is
        // documented.
        let mut t = BudgetTracker::new(
            PlanBudgets {
                max_iterations: Some(1),
                max_tokens: Some(1),
                ..Default::default()
            },
            3,
        );
        t.record_session(&record(1, SessionStatus::Ok, 100, 0, 0.0));
        match t.check() {
            BudgetCheck::Exhausted(BudgetReason::MaxIterations { .. }) => {}
            other => panic!("expected MaxIterations to win, got {other:?}"),
        }
    }

    #[test]
    fn resolve_budgets_layers_config_plan_cli() {
        let config = PlanBudgets {
            max_iterations: Some(10),
            max_tokens: Some(1_000_000),
            ..Default::default()
        };
        let plan = PlanBudgets {
            max_iterations: Some(5),
            max_cost_usd: Some(2.0),
            ..Default::default()
        };
        let cli = PlanBudgets {
            max_tokens: Some(50_000),
            ..Default::default()
        };
        let r = resolve_budgets(&config, &plan, &cli);
        // CLI's max_tokens wins.
        assert_eq!(r.max_tokens, Some(50_000));
        // Plan's max_iterations wins over config.
        assert_eq!(r.max_iterations, Some(5));
        // Plan supplied max_cost; config did not.
        assert_eq!(r.max_cost_usd, Some(2.0));
        // Nothing supplied until.
        assert_eq!(r.until, None);
    }

    #[test]
    fn resolve_budgets_cli_clears_nothing() {
        // Resolution is additive: a CLI struct with all-`None` fields leaves
        // earlier layers untouched. (To "unset" a budget the user removes the
        // line from the source layer.)
        let config = PlanBudgets {
            max_iterations: Some(10),
            ..Default::default()
        };
        let plan = PlanBudgets::default();
        let cli = PlanBudgets::default();
        let r = resolve_budgets(&config, &plan, &cli);
        assert_eq!(r.max_iterations, Some(10));
    }

    #[test]
    fn session_cost_usd_uses_pricing_table() {
        let mut config = Config::default();
        // Default opus pricing covers the default model id.
        let cost = session_cost_usd(&config, "claude-opus-4-7", 1_000_000, 0);
        // 1M input * $15/M = $15.
        assert!((cost - 15.0).abs() < 1e-9, "cost: {cost}");
        // Unknown model → 0.
        assert_eq!(
            session_cost_usd(&config, "no-such-model", 1_000_000, 0),
            0.0
        );
        // Add a custom price → cost reflects it.
        config.budgets.pricing.insert(
            "custom".into(),
            ModelPricing {
                input_per_million_usd: 1.0,
                output_per_million_usd: 2.0,
            },
        );
        let cost = session_cost_usd(&config, "custom", 1_000_000, 500_000);
        // 1M * $1 + 0.5M * $2 = $2.
        assert!((cost - 2.0).abs() < 1e-9, "cost: {cost}");
    }
}
