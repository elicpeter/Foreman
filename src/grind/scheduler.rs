//! Pure rotation scheduler.
//!
//! Given a [`GrindPlan`], a lookup of [`PromptDoc`]s, and a
//! [`SchedulerState`], decide which prompt the runner should dispatch next.
//! The scheduler does no IO — every call is a deterministic function of its
//! inputs, which lets the test suite exercise hundreds of rotations cheaply.
//!
//! The selection algorithm is weighted round-robin by deficit. Each rotation:
//!
//! 1. `state.rotation` is incremented before any picking happens.
//! 2. The candidate set is the plan's prompts that pass two gates: the every
//!    gate (`rotation % effective_every == 0`) and the max-runs gate
//!    (`runs[name] < effective_max_runs`).
//! 3. Among the candidates, the prompt with the highest score wins, where
//!    `score(i) = weight[i] * total_runs - runs[i] * total_weight` summed only
//!    over the candidate set. Ties break alphabetically by name. The score is
//!    proportional to "how far prompt `i` is below its weighted fair-share
//!    today", so over a long run, runs converge to the configured weight ratio.
//! 4. If the candidate set is empty (all gated out, or the plan is empty),
//!    [`Scheduler::next`] returns `None`.
//!
//! [`Scheduler::record_run`] is the way the runner reports a successful (or
//! attempted) dispatch back into the scheduler. It is intentionally separate
//! from [`Scheduler::next`] so the runner can decide not to count a session
//! that never made it past the pre-session hook.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::plan::{GrindPlan, PlanPromptRef};
use super::prompt::PromptDoc;

/// Mutable scheduler state. Persisted between rotations and serializable so
/// later phases can checkpoint a run to disk and resume it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerState {
    /// 1-based rotation counter. Incremented at the top of every
    /// [`Scheduler::next`] call (whether or not a prompt is picked), so
    /// `every`-gated prompts advance even on rotations that produce `None`.
    pub rotation: u64,
    /// How many times each prompt has been recorded as run. Keyed by prompt
    /// name. A missing entry means zero runs.
    pub runs_per_prompt: BTreeMap<String, u32>,
}

/// Pure rotation scheduler. See the module docs for the selection algorithm.
#[derive(Debug, Clone)]
pub struct Scheduler {
    plan: GrindPlan,
    prompts: BTreeMap<String, PromptDoc>,
    state: SchedulerState,
}

impl Scheduler {
    /// Build a scheduler at rotation 0 with no recorded runs.
    pub fn new(plan: GrindPlan, prompts: BTreeMap<String, PromptDoc>) -> Self {
        Self {
            plan,
            prompts,
            state: SchedulerState::default(),
        }
    }

    /// Build a scheduler from a previously-persisted state.
    pub fn with_state(
        plan: GrindPlan,
        prompts: BTreeMap<String, PromptDoc>,
        state: SchedulerState,
    ) -> Self {
        Self {
            plan,
            prompts,
            state,
        }
    }

    /// Read-only access to the underlying plan.
    pub fn plan(&self) -> &GrindPlan {
        &self.plan
    }

    /// Read-only access to the current scheduler state.
    pub fn state(&self) -> &SchedulerState {
        &self.state
    }

    /// Record that `name` was just dispatched. Bumps `runs_per_prompt[name]`.
    /// Saturates at `u32::MAX`; in practice rotation counts are far below that.
    pub fn record_run(&mut self, name: &str) {
        let counter = self
            .state
            .runs_per_prompt
            .entry(name.to_string())
            .or_insert(0);
        *counter = counter.saturating_add(1);
    }

    /// Advance the rotation and return the next prompt to dispatch, if any.
    ///
    /// Always increments `state.rotation`, even when no prompt is eligible —
    /// otherwise an `every`-gated prompt that the caller polls for could never
    /// reach its rotation.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<PromptDoc> {
        self.state.rotation = self.state.rotation.saturating_add(1);
        let rotation = self.state.rotation;

        let mut candidates: Vec<Candidate<'_>> = Vec::with_capacity(self.plan.prompts.len());
        for entry in &self.plan.prompts {
            let Some(doc) = self.prompts.get(&entry.name) else {
                // The plan referred to a prompt we don't have. Plan validation
                // (see `GrindPlan::validate_against`) is supposed to catch this
                // before the scheduler is built, so silently skipping is the
                // right move at this layer.
                continue;
            };
            let opts = effective(entry, doc);
            if !rotation.is_multiple_of(u64::from(opts.every)) {
                continue;
            }
            let runs = self
                .state
                .runs_per_prompt
                .get(entry.name.as_str())
                .copied()
                .unwrap_or(0);
            if let Some(cap) = opts.max_runs {
                if runs >= cap {
                    continue;
                }
            }
            candidates.push(Candidate {
                name: entry.name.as_str(),
                doc,
                weight: opts.weight,
                runs,
            });
        }

        if candidates.is_empty() {
            return None;
        }

        let total_weight: i128 = candidates.iter().map(|c| i128::from(c.weight)).sum();
        let total_runs: i128 = candidates.iter().map(|c| i128::from(c.runs)).sum();

        let mut best: Option<&Candidate<'_>> = None;
        let mut best_score: i128 = 0;
        for cand in &candidates {
            let score = i128::from(cand.weight) * total_runs - i128::from(cand.runs) * total_weight;
            let take = match best {
                None => true,
                Some(prev) => score > best_score || (score == best_score && cand.name < prev.name),
            };
            if take {
                best = Some(cand);
                best_score = score;
            }
        }
        best.map(|c| c.doc.clone())
    }
}

struct Candidate<'a> {
    name: &'a str,
    doc: &'a PromptDoc,
    weight: u32,
    runs: u32,
}

struct EffectiveOpts {
    weight: u32,
    every: u32,
    max_runs: Option<u32>,
}

fn effective(entry: &PlanPromptRef, doc: &PromptDoc) -> EffectiveOpts {
    EffectiveOpts {
        weight: entry.weight_override.unwrap_or(doc.meta.weight),
        every: entry.every_override.unwrap_or(doc.meta.every),
        max_runs: entry.max_runs_override.or(doc.meta.max_runs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grind::plan::{default_plan_from_dir, GrindPlan, PlanPromptRef};
    use crate::grind::prompt::{PromptMeta, PromptSource};
    use std::path::PathBuf;

    fn fake_prompt(name: &str, weight: u32, every: u32, max_runs: Option<u32>) -> PromptDoc {
        PromptDoc {
            meta: PromptMeta {
                name: name.to_string(),
                description: format!("desc for {name}"),
                weight,
                every,
                max_runs,
                verify: false,
                parallel_safe: false,
                tags: Vec::new(),
                max_session_seconds: None,
                max_session_cost_usd: None,
            },
            body: String::new(),
            source_path: PathBuf::from(format!("/fixture/{name}.md")),
            source_kind: PromptSource::Project,
        }
    }

    fn lookup(prompts: &[PromptDoc]) -> BTreeMap<String, PromptDoc> {
        prompts
            .iter()
            .map(|p| (p.meta.name.clone(), p.clone()))
            .collect()
    }

    fn run_n(scheduler: &mut Scheduler, n: usize) -> Vec<Option<String>> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let pick = scheduler.next();
            let name = pick.as_ref().map(|d| d.meta.name.clone());
            if let Some(d) = pick {
                scheduler.record_run(&d.meta.name);
            }
            out.push(name);
        }
        out
    }

    #[test]
    fn empty_plan_returns_none() {
        let plan = default_plan_from_dir(&[]);
        let mut s = Scheduler::new(plan, BTreeMap::new());
        assert_eq!(s.next(), None);
        // Rotation still advances even when there's nothing to pick — keeps the
        // counter aligned with caller expectations if the plan grows mid-run.
        assert_eq!(s.state().rotation, 1);
    }

    #[test]
    fn weight_two_to_one_holds_over_one_hundred_rotations() {
        // With weight 2:1 and equal `every`, the long-run ratio of picks must
        // converge to 2:1 (within one tick).
        let prompts = vec![fake_prompt("a", 2, 1, None), fake_prompt("b", 1, 1, None)];
        let plan = default_plan_from_dir(&prompts);
        let mut s = Scheduler::new(plan, lookup(&prompts));
        let picks = run_n(&mut s, 100);
        let count_a = picks.iter().filter(|p| p.as_deref() == Some("a")).count();
        let count_b = picks.iter().filter(|p| p.as_deref() == Some("b")).count();
        assert_eq!(count_a + count_b, 100);
        // 100 * 2/3 ≈ 67. Allow ±1 for rounding in the deficit cycle.
        assert!(
            (66..=68).contains(&count_a),
            "expected ~67 a picks, got {count_a} (b={count_b})"
        );
        assert!(
            (32..=34).contains(&count_b),
            "expected ~33 b picks, got {count_b} (a={count_a})"
        );
    }

    #[test]
    fn weight_three_to_one_holds_over_one_hundred_rotations() {
        let prompts = vec![fake_prompt("a", 3, 1, None), fake_prompt("b", 1, 1, None)];
        let plan = default_plan_from_dir(&prompts);
        let mut s = Scheduler::new(plan, lookup(&prompts));
        let picks = run_n(&mut s, 100);
        let count_a = picks.iter().filter(|p| p.as_deref() == Some("a")).count();
        let count_b = picks.iter().filter(|p| p.as_deref() == Some("b")).count();
        assert_eq!(count_a + count_b, 100);
        // Expected ~75/25.
        assert!(
            (74..=76).contains(&count_a),
            "expected ~75 a picks, got {count_a}"
        );
    }

    #[test]
    fn every_three_only_fires_on_multiples_of_three() {
        let prompts = vec![fake_prompt("triage", 1, 3, None)];
        let plan = default_plan_from_dir(&prompts);
        let mut s = Scheduler::new(plan, lookup(&prompts));
        let picks = run_n(&mut s, 9);
        assert_eq!(
            picks,
            vec![
                None,
                None,
                Some("triage".to_string()),
                None,
                None,
                Some("triage".to_string()),
                None,
                None,
                Some("triage".to_string()),
            ]
        );
    }

    #[test]
    fn max_runs_retires_a_prompt() {
        let prompts = vec![fake_prompt("oneshot", 1, 1, Some(5))];
        let plan = default_plan_from_dir(&prompts);
        let mut s = Scheduler::new(plan, lookup(&prompts));
        let picks = run_n(&mut s, 10);
        let some_count = picks.iter().filter(|p| p.is_some()).count();
        let none_count = picks.iter().filter(|p| p.is_none()).count();
        assert_eq!(some_count, 5, "max_runs should cap dispatch at 5");
        assert_eq!(none_count, 5, "remaining rotations have no eligible prompt");
        // After the cap, every subsequent call returns None.
        assert!(picks[5..].iter().all(|p| p.is_none()));
    }

    #[test]
    fn max_runs_override_wins_over_frontmatter() {
        // Frontmatter says max_runs=10; plan override says 2.
        let prompt = fake_prompt("p", 1, 1, Some(10));
        let plan = GrindPlan {
            name: "test".to_string(),
            prompts: vec![PlanPromptRef {
                name: "p".to_string(),
                weight_override: None,
                every_override: None,
                max_runs_override: Some(2),
            }],
            max_parallel: 1,
            hooks: Default::default(),
            budgets: Default::default(),
        };
        let mut s = Scheduler::new(plan, lookup(&[prompt]));
        let picks = run_n(&mut s, 5);
        assert_eq!(picks.iter().filter(|p| p.is_some()).count(), 2);
    }

    #[test]
    fn weight_override_wins_over_frontmatter() {
        // Frontmatter weight 1:1, but plan overrides a's weight to 5.
        let prompts = vec![fake_prompt("a", 1, 1, None), fake_prompt("b", 1, 1, None)];
        let plan = GrindPlan {
            name: "test".to_string(),
            prompts: vec![
                PlanPromptRef {
                    name: "a".to_string(),
                    weight_override: Some(5),
                    every_override: None,
                    max_runs_override: None,
                },
                PlanPromptRef {
                    name: "b".to_string(),
                    weight_override: None,
                    every_override: None,
                    max_runs_override: None,
                },
            ],
            max_parallel: 1,
            hooks: Default::default(),
            budgets: Default::default(),
        };
        let mut s = Scheduler::new(plan, lookup(&prompts));
        let picks = run_n(&mut s, 60);
        let count_a = picks.iter().filter(|p| p.as_deref() == Some("a")).count();
        // 5:1 weight → 50/60 a, 10/60 b. Allow ±1 tick.
        assert!(
            (49..=51).contains(&count_a),
            "expected ~50 a picks, got {count_a}"
        );
    }

    #[test]
    fn every_override_wins_over_frontmatter() {
        let prompt = fake_prompt("p", 1, 1, None);
        let plan = GrindPlan {
            name: "test".to_string(),
            prompts: vec![PlanPromptRef {
                name: "p".to_string(),
                weight_override: None,
                every_override: Some(2),
                max_runs_override: None,
            }],
            max_parallel: 1,
            hooks: Default::default(),
            budgets: Default::default(),
        };
        let mut s = Scheduler::new(plan, lookup(&[prompt]));
        let picks = run_n(&mut s, 6);
        // every=2 → fires only on rotations 2, 4, 6.
        assert_eq!(picks[0], None);
        assert_eq!(picks[1].as_deref(), Some("p"));
        assert_eq!(picks[2], None);
        assert_eq!(picks[3].as_deref(), Some("p"));
        assert_eq!(picks[4], None);
        assert_eq!(picks[5].as_deref(), Some("p"));
    }

    #[test]
    fn ties_break_alphabetically() {
        // Identical weight/every with no runs → first rotation is a tie. The
        // alphabetically first name must win regardless of plan order.
        let prompts = vec![
            fake_prompt("zebra", 1, 1, None),
            fake_prompt("alpha", 1, 1, None),
            fake_prompt("mango", 1, 1, None),
        ];
        let plan = GrindPlan {
            name: "test".to_string(),
            prompts: vec![
                PlanPromptRef {
                    name: "zebra".to_string(),
                    weight_override: None,
                    every_override: None,
                    max_runs_override: None,
                },
                PlanPromptRef {
                    name: "alpha".to_string(),
                    weight_override: None,
                    every_override: None,
                    max_runs_override: None,
                },
                PlanPromptRef {
                    name: "mango".to_string(),
                    weight_override: None,
                    every_override: None,
                    max_runs_override: None,
                },
            ],
            max_parallel: 1,
            hooks: Default::default(),
            budgets: Default::default(),
        };
        let mut s = Scheduler::new(plan, lookup(&prompts));
        let pick = s.next().unwrap();
        assert_eq!(pick.meta.name, "alpha");
    }

    #[test]
    fn determinism_two_runs_yield_identical_sequences() {
        let prompts = vec![
            fake_prompt("a", 2, 1, None),
            fake_prompt("b", 1, 1, None),
            fake_prompt("c", 3, 1, Some(7)),
        ];
        let plan = default_plan_from_dir(&prompts);
        let mut s1 = Scheduler::new(plan.clone(), lookup(&prompts));
        let mut s2 = Scheduler::new(plan, lookup(&prompts));
        let seq1 = run_n(&mut s1, 50);
        let seq2 = run_n(&mut s2, 50);
        assert_eq!(seq1, seq2);
        assert_eq!(s1.state(), s2.state());
    }

    #[test]
    fn record_run_without_dispatch_advances_count() {
        // record_run is decoupled from next() — the runner can choose not to
        // bump the count for a session it never actually dispatched. Verify the
        // counter behaves the way the contract advertises.
        let prompts = vec![fake_prompt("a", 1, 1, Some(2))];
        let plan = default_plan_from_dir(&prompts);
        let mut s = Scheduler::new(plan, lookup(&prompts));
        // Pretend "a" already ran twice without next() being called.
        s.record_run("a");
        s.record_run("a");
        // Now next() should refuse further dispatch.
        assert_eq!(s.next(), None);
        assert_eq!(s.state().runs_per_prompt.get("a"), Some(&2));
    }

    #[test]
    fn state_round_trips_through_serde() {
        let mut s = Scheduler::new(
            default_plan_from_dir(&[fake_prompt("p", 1, 1, None)]),
            lookup(&[fake_prompt("p", 1, 1, None)]),
        );
        s.next();
        s.record_run("p");
        s.next();
        s.record_run("p");
        let json = serde_json::to_string(s.state()).expect("serialize state");
        let back: SchedulerState = serde_json::from_str(&json).expect("deserialize state");
        assert_eq!(&back, s.state());
    }

    #[test]
    fn unknown_plan_entry_is_skipped() {
        // A plan that pins a name we don't have in the prompt lookup should not
        // panic — the scheduler treats it as if no candidate is present at that
        // slot. Plan validation is supposed to catch this earlier.
        let prompts = vec![fake_prompt("real", 1, 1, None)];
        let plan = GrindPlan {
            name: "test".to_string(),
            prompts: vec![
                PlanPromptRef {
                    name: "ghost".to_string(),
                    weight_override: None,
                    every_override: None,
                    max_runs_override: None,
                },
                PlanPromptRef {
                    name: "real".to_string(),
                    weight_override: None,
                    every_override: None,
                    max_runs_override: None,
                },
            ],
            max_parallel: 1,
            hooks: Default::default(),
            budgets: Default::default(),
        };
        let mut s = Scheduler::new(plan, lookup(&prompts));
        let pick = s.next().unwrap();
        assert_eq!(pick.meta.name, "real");
    }
}
