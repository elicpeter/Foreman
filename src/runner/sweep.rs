//! Deferred-sweep trigger logic.
//!
//! The sweep pipeline (phase 03) hands the deferred-checklist work to a
//! dedicated agent dispatch between regular phases. This module owns the
//! pure decision of *whether* the next phase boundary should run a sweep,
//! plus the shared `unchecked_count` helper every caller — the trigger,
//! the sweep prompt renderer, the status command, and phase 05's
//! staleness tracker — uses to count pending items the same way.
//!
//! The dispatch loop, prompt rendering, audit pass, and consecutive-sweep
//! accounting all live elsewhere; keeping this module pure (no I/O, no
//! agent dispatch, no shared mutable state) lets the trigger sit in the
//! runner's hot path without becoming hard to reason about.
//!
//! # Trigger rule
//!
//! `should_run_deferred_sweep` answers true iff:
//!
//! 1. Sweeps are enabled in [`crate::config::SweepConfig`].
//! 2. The unchecked-item count is at or above
//!    [`crate::config::SweepConfig::trigger_min_items`].
//! 3. The runner has not already chained
//!    [`crate::config::SweepConfig::max_consecutive`] sweeps in a row.
//!
//! Note that [`crate::config::SweepConfig::trigger_max_items`] is *not*
//! checked here. That field is advisory: it documents the expected upper
//! bound rather than gating behavior, since clamping a 7-easy-fix sweep to
//! the configured cap would simply re-defer the rest. The sweep agent's
//! prompt names the bound; how many items it actually addresses per
//! dispatch is the agent's call.

use crate::config::SweepConfig;
use crate::deferred::DeferredDoc;

/// Count the unchecked `## Deferred items` entries in `doc`.
///
/// Single source of truth for "how many items are pending". The status
/// command, the sweep trigger, the sweep prompt renderer, and phase 05's
/// staleness tracker all defer to this so they can never disagree on what
/// counts.
pub fn unchecked_count(doc: &DeferredDoc) -> usize {
    doc.items.iter().filter(|item| !item.done).count()
}

/// Decide whether the runner should dispatch a deferred-sweep pass at the
/// next phase boundary.
///
/// `consecutive_sweeps` is the number of sweep dispatches the runner has
/// already chained without an intervening real phase — the caller owns this
/// counter so it can persist across resumes.
pub fn should_run_deferred_sweep(
    deferred: &DeferredDoc,
    sweep_cfg: &SweepConfig,
    consecutive_sweeps: u32,
) -> bool {
    if !sweep_cfg.enabled {
        return false;
    }
    if consecutive_sweeps >= sweep_cfg.max_consecutive {
        return false;
    }
    let pending = unchecked_count(deferred) as u32;
    pending >= sweep_cfg.trigger_min_items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deferred::{DeferredItem, DeferredPhase};
    use crate::plan::PhaseId;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    fn doc_with_pending(n: usize) -> DeferredDoc {
        DeferredDoc {
            items: (0..n)
                .map(|i| DeferredItem {
                    text: format!("pending item {i}"),
                    done: false,
                })
                .collect(),
            phases: Vec::new(),
        }
    }

    #[test]
    fn unchecked_count_ignores_completed_and_phase_blocks() {
        let doc = DeferredDoc {
            items: vec![
                DeferredItem {
                    text: "pending one".into(),
                    done: false,
                },
                DeferredItem {
                    text: "done one".into(),
                    done: true,
                },
                DeferredItem {
                    text: "pending two".into(),
                    done: false,
                },
            ],
            phases: vec![DeferredPhase {
                source_phase: pid("07"),
                title: "rework".into(),
                body: "body".into(),
            }],
        };
        assert_eq!(unchecked_count(&doc), 2);
    }

    #[test]
    fn unchecked_count_zero_for_empty_doc() {
        assert_eq!(unchecked_count(&DeferredDoc::empty()), 0);
    }

    #[test]
    fn trigger_fires_at_threshold() {
        let cfg = SweepConfig::default();
        // Default trigger is 5; at 4 we should still skip, at 5 we trip.
        assert!(!should_run_deferred_sweep(
            &doc_with_pending(cfg.trigger_min_items as usize - 1),
            &cfg,
            0
        ));
        assert!(should_run_deferred_sweep(
            &doc_with_pending(cfg.trigger_min_items as usize),
            &cfg,
            0
        ));
        assert!(should_run_deferred_sweep(
            &doc_with_pending(cfg.trigger_min_items as usize + 3),
            &cfg,
            0
        ));
    }

    #[test]
    fn trigger_short_circuits_when_disabled() {
        let cfg = SweepConfig {
            enabled: false,
            ..SweepConfig::default()
        };
        // Even with way more than the trigger, disabled wins.
        assert!(!should_run_deferred_sweep(
            &doc_with_pending(cfg.trigger_min_items as usize * 4),
            &cfg,
            0
        ));
    }

    #[test]
    fn trigger_clamps_at_max_consecutive() {
        let cfg = SweepConfig::default();
        assert_eq!(cfg.max_consecutive, 1);
        // Below the cap → eligible to fire.
        assert!(should_run_deferred_sweep(
            &doc_with_pending(cfg.trigger_min_items as usize),
            &cfg,
            0
        ));
        // At the cap → must yield to a real phase before sweeping again.
        assert!(!should_run_deferred_sweep(
            &doc_with_pending(cfg.trigger_min_items as usize),
            &cfg,
            cfg.max_consecutive
        ));
        // Above the cap → still suppressed (defensive: max_consecutive
        // rises across phases when chained sweeps land back-to-back).
        assert!(!should_run_deferred_sweep(
            &doc_with_pending(cfg.trigger_min_items as usize),
            &cfg,
            cfg.max_consecutive + 5
        ));
    }

    #[test]
    fn trigger_respects_higher_max_consecutive() {
        let cfg = SweepConfig {
            max_consecutive: 3,
            ..SweepConfig::default()
        };
        assert!(should_run_deferred_sweep(
            &doc_with_pending(cfg.trigger_min_items as usize),
            &cfg,
            0
        ));
        assert!(should_run_deferred_sweep(
            &doc_with_pending(cfg.trigger_min_items as usize),
            &cfg,
            2
        ));
        assert!(!should_run_deferred_sweep(
            &doc_with_pending(cfg.trigger_min_items as usize),
            &cfg,
            3
        ));
    }

    #[test]
    fn trigger_max_items_does_not_gate() {
        // The advisory upper bound must not gate behavior — even a doc with
        // way more pending items than `trigger_max_items` should still fire
        // the sweep. The sweep agent decides how many to take per dispatch.
        let cfg = SweepConfig::default();
        let huge = doc_with_pending(cfg.trigger_max_items as usize * 4);
        assert!(should_run_deferred_sweep(&huge, &cfg, 0));
    }
}
