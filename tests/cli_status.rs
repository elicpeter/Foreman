//! Snapshot-style tests for `pitboss status` (phase 06).
//!
//! Drives `cli::status::render_report` directly with carefully constructed
//! state / deferred / config inputs so the asserted lines match the
//! "Sweep:" block layout the spec mandates without touching git or the
//! filesystem. The function is pure over its inputs, so each test reads
//! like a tabular fixture.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use tempfile::tempdir;

use pitboss::cli::status::render_report;
use pitboss::config::Config;
use pitboss::deferred::{DeferredDoc, DeferredItem};
use pitboss::plan::{Phase, PhaseId, Plan};
use pitboss::state::{RunState, TokenUsage};

fn pid(s: &str) -> PhaseId {
    PhaseId::parse(s).expect("valid phase id")
}

fn small_plan() -> Plan {
    Plan::new(
        pid("02"),
        vec![
            Phase {
                id: pid("01"),
                title: "First".into(),
                body: String::new(),
            },
            Phase {
                id: pid("02"),
                title: "Second".into(),
                body: String::new(),
            },
        ],
    )
}

fn fresh_state() -> RunState {
    RunState {
        run_id: "20260501T120000Z".into(),
        branch: "pitboss/play/20260501T120000Z".into(),
        original_branch: Some("main".into()),
        started_at: DateTime::parse_from_rfc3339("2026-05-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        started_phase: pid("01"),
        completed: Vec::new(),
        attempts: HashMap::new(),
        token_usage: TokenUsage::default(),
        aborted: false,
        pending_sweep: false,
        consecutive_sweeps: 0,
        deferred_item_attempts: HashMap::new(),
        post_final_phase: false,
    }
}

fn pending_items(n: usize) -> DeferredDoc {
    let items = (0..n)
        .map(|i| DeferredItem {
            text: format!("pending item {i}"),
            done: false,
        })
        .collect();
    DeferredDoc {
        items,
        phases: Vec::new(),
    }
}

fn render(
    workspace: &Path,
    plan: &Plan,
    deferred: &DeferredDoc,
    state: Option<&RunState>,
) -> String {
    let cfg = Config::default();
    render_report(workspace, plan, deferred, state, &cfg, false)
}

#[test]
fn sweep_block_no_pending_no_items() {
    let dir = tempdir().unwrap();
    let plan = small_plan();
    let deferred = DeferredDoc::empty();
    let state = fresh_state();
    let report = render(dir.path(), &plan, &deferred, Some(&state));

    assert!(report.contains("Sweep:"), "report:\n{report}");
    assert!(report.contains("pending: false"), "report:\n{report}");
    assert!(report.contains("consecutive: 0"), "report:\n{report}");
    assert!(
        report.contains("deferred items: 0 unchecked / 0 total"),
        "report:\n{report}"
    );
    assert!(
        !report.contains("stale items"),
        "stale items section must be absent when no items are stale; report:\n{report}"
    );
}

#[test]
fn sweep_block_pending_with_six_items() {
    let dir = tempdir().unwrap();
    let plan = small_plan();
    let deferred = pending_items(6);
    let mut state = fresh_state();
    state.pending_sweep = true;
    let report = render(dir.path(), &plan, &deferred, Some(&state));

    assert!(report.contains("pending: true"), "report:\n{report}");
    assert!(
        report.contains("deferred items: 6 unchecked / 6 total"),
        "report:\n{report}"
    );
    assert!(report.contains("consecutive: 0"), "report:\n{report}");
}

#[test]
fn sweep_block_just_finished_consecutive_one() {
    let dir = tempdir().unwrap();
    let plan = small_plan();
    let deferred = pending_items(2);
    let mut state = fresh_state();
    state.pending_sweep = false;
    state.consecutive_sweeps = 1;
    state.completed.push(pid("01"));
    let report = render(dir.path(), &plan, &deferred, Some(&state));

    assert!(report.contains("pending: false"), "report:\n{report}");
    assert!(report.contains("consecutive: 1"), "report:\n{report}");
    assert!(
        report.contains("deferred items: 2 unchecked / 2 total"),
        "report:\n{report}"
    );
}

#[test]
fn sweep_block_renders_stale_items_capped_and_sorted() {
    let dir = tempdir().unwrap();
    let plan = small_plan();
    let deferred = pending_items(3);
    let mut state = fresh_state();
    // Default `escalate_after = 3`. Three items past the threshold at
    // varying attempt counts; the block must list them in descending
    // order with the spec's "(tried <n> times)" suffix.
    state
        .deferred_item_attempts
        .insert("polish error".to_string(), 5);
    state
        .deferred_item_attempts
        .insert("rename flag".to_string(), 3);
    state
        .deferred_item_attempts
        .insert("audit defaults".to_string(), 7);
    let report = render(dir.path(), &plan, &deferred, Some(&state));

    assert!(report.contains("stale items: 3"), "report:\n{report}");
    assert!(
        report.contains("(need attention)"),
        "spec line missing; report:\n{report}"
    );
    let p_audit = report
        .find("\"audit defaults\"")
        .expect("audit defaults entry; report:\n{report}");
    let p_polish = report.find("\"polish error\"").expect("polish error entry");
    let p_rename = report.find("\"rename flag\"").expect("rename flag entry");
    assert!(
        p_audit < p_polish && p_polish < p_rename,
        "expected sorted by descending attempts; got positions audit={p_audit} polish={p_polish} rename={p_rename}; report:\n{report}"
    );
    assert!(
        report.contains("(tried 7 times)"),
        "audit defaults attempt count; report:\n{report}"
    );
    assert!(
        report.contains("(tried 5 times)"),
        "polish error attempt count; report:\n{report}"
    );
    assert!(
        report.contains("(tried 3 times)"),
        "rename flag attempt count; report:\n{report}"
    );
    assert!(
        report.contains("Promote a stale item"),
        "footer with remediation suggestion; report:\n{report}"
    );
}

#[test]
fn sweep_block_large_backlog_no_stale() {
    let dir = tempdir().unwrap();
    let plan = small_plan();
    let deferred = pending_items(20);
    let state = fresh_state();
    let report = render(dir.path(), &plan, &deferred, Some(&state));

    assert!(
        report.contains("deferred items: 20 unchecked / 20 total"),
        "report:\n{report}"
    );
    assert!(
        !report.contains("stale items"),
        "no item should be stale; report:\n{report}"
    );
}
