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

use std::collections::{HashMap, HashSet};

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

/// Update the per-item staleness counter map after a sweep dispatch.
///
/// Behavior:
///
/// - For each text in `pre_texts ∩ post_unchecked_texts` (items that survived
///   the sweep without being checked off), increment its entry by 1, inserting
///   at 1 if absent.
/// - Prune entries whose key is not in `post_unchecked_texts` (resolved items
///   should not carry a counter, and an item the agent rewrote — old text gone,
///   new text present — has its old key dropped here so a future sweep can
///   start the new key from scratch).
///
/// Returns the list of `(text, new_attempt_count)` pairs whose counter just
/// crossed the `escalate_after` threshold (`prev < escalate_after &&
/// new_count >= escalate_after`). The transition-only signal lets the caller
/// emit [`crate::runner::Event::DeferredItemStale`] exactly once per item, not
/// on every subsequent sweep where the counter remains above the threshold.
///
/// `escalate_after = 0` is treated like `1` so a misconfigured zero never
/// makes the threshold unreachable; `SweepConfig` validation rejects zero, so
/// this only matters for direct callers.
pub fn update_sweep_staleness(
    attempts: &mut HashMap<String, u32>,
    pre_texts: &HashSet<String>,
    post_unchecked_texts: &HashSet<String>,
    escalate_after: u32,
) -> Vec<(String, u32)> {
    let threshold = escalate_after.max(1);
    let mut crossed: Vec<(String, u32)> = Vec::new();
    for text in pre_texts.intersection(post_unchecked_texts) {
        let prev = attempts.get(text).copied().unwrap_or(0);
        let new_count = prev.saturating_add(1);
        attempts.insert(text.clone(), new_count);
        if prev < threshold && new_count >= threshold {
            crossed.push((text.clone(), new_count));
        }
    }
    attempts.retain(|k, _| post_unchecked_texts.contains(k));
    crossed
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

    fn texts<I: IntoIterator<Item = &'static str>>(items: I) -> HashSet<String> {
        items.into_iter().map(str::to_string).collect()
    }

    #[test]
    fn staleness_increments_intersection_only() {
        let mut map = HashMap::new();
        let pre = texts(["a", "b", "c"]);
        let post = texts(["a", "b"]);
        let crossed = update_sweep_staleness(&mut map, &pre, &post, 3);
        assert_eq!(map.get("a").copied(), Some(1));
        assert_eq!(map.get("b").copied(), Some(1));
        // "c" was resolved (not in post) → no entry.
        assert!(!map.contains_key("c"));
        // None of the new counters reached the threshold yet.
        assert!(crossed.is_empty());
    }

    #[test]
    fn staleness_prunes_resolved_items() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), 2);
        map.insert("b".to_string(), 1);
        // "a" was resolved this sweep → drops out. "b" survived → +1.
        let pre = texts(["a", "b"]);
        let post = texts(["b"]);
        update_sweep_staleness(&mut map, &pre, &post, 3);
        assert!(!map.contains_key("a"));
        assert_eq!(map.get("b").copied(), Some(2));
    }

    #[test]
    fn staleness_emits_threshold_crossing_only_on_transition() {
        let mut map = HashMap::new();
        map.insert("survivor".to_string(), 2);
        let pre = texts(["survivor"]);
        let post = texts(["survivor"]);
        // 2 → 3 crosses escalate_after = 3.
        let crossed = update_sweep_staleness(&mut map, &pre, &post, 3);
        assert_eq!(
            crossed,
            vec![("survivor".to_string(), 3)],
            "expected 2→3 crossing"
        );
        // Next sweep: 3 → 4, no fresh crossing.
        let crossed = update_sweep_staleness(&mut map, &pre, &post, 3);
        assert!(
            crossed.is_empty(),
            "items already at/above threshold must not re-emit; got {crossed:?}"
        );
        assert_eq!(map.get("survivor").copied(), Some(4));
    }

    #[test]
    fn staleness_text_rewrite_prunes_old_key_then_starts_fresh() {
        // The phase 02 sweep prompt forbids rewording, but a misbehaving agent
        // could still do it. The bookkeeping must treat the rewritten item as
        // brand-new work: old key drops out of the map, new key only starts
        // accumulating once a subsequent sweep sees it in the pre-texts.
        let mut map = HashMap::new();
        map.insert("old text".to_string(), 2);
        // Sweep 1: agent rewrote "old text" → "new text".
        let pre = texts(["old text"]);
        let post = texts(["new text"]);
        update_sweep_staleness(&mut map, &pre, &post, 3);
        assert!(
            !map.contains_key("old text"),
            "rewritten item's old key must be pruned"
        );
        assert!(
            !map.contains_key("new text"),
            "the rewrite is one sweep early — no entry yet"
        );
        // Sweep 2: "new text" is now in pre_texts and survives → starts at 1.
        let pre = texts(["new text"]);
        let post = texts(["new text"]);
        update_sweep_staleness(&mut map, &pre, &post, 3);
        assert_eq!(map.get("new text").copied(), Some(1));
    }

    #[test]
    fn staleness_zero_escalate_after_clamps_to_one() {
        // `SweepConfig::validate` rejects zero, but the helper itself must
        // never produce a threshold that can't be crossed.
        let mut map = HashMap::new();
        let pre = texts(["a"]);
        let post = texts(["a"]);
        let crossed = update_sweep_staleness(&mut map, &pre, &post, 0);
        assert_eq!(crossed, vec![("a".to_string(), 1)]);
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
