//! Integration tests for `pitboss status`, `pitboss rebuy`, and
//! `pitboss fold` (phase 17).
//!
//! These exercise the binary via `assert_cmd` against a temp workspace so the
//! full clap-dispatch path runs. Tests that require a halted run pre-populate
//! `.pitboss/state.json` directly rather than driving the runner — driving the
//! runner via the CLI requires a real `claude` binary, which CI doesn't have.
//!
//! The casino-themed verbs (`play` / `rebuy` / `fold`) are the canonical
//! names; their pre-rename aliases (`run` / `resume` / `abort`) are kept for
//! backwards compatibility and verified by [`legacy_aliases_still_work`].

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command as PCommand;

use assert_cmd::Command;
use chrono::{DateTime, Utc};
use predicates::str::contains;
use serde_json::json;
use tempfile::tempdir;

fn pitboss() -> Command {
    Command::cargo_bin("pitboss").expect("pitboss binary should be built")
}

fn init_workspace(dir: &Path) {
    pitboss().arg("init").current_dir(dir).assert().success();
}

fn init_git_repo(dir: &Path) {
    let status = PCommand::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(dir)
        .status()
        .unwrap();
    assert!(status.success());
    for (k, v) in [
        ("user.name", "pitboss-test"),
        ("user.email", "pitboss@test"),
    ] {
        PCommand::new("git")
            .args(["-C"])
            .arg(dir)
            .args(["config", k, v])
            .status()
            .unwrap();
    }
    let status = PCommand::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["commit", "--allow-empty", "-m", "seed", "-q"])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Write a `.pitboss/state.json` directly. Mirrors what `pitboss play` would
/// have persisted after a halt. The on-disk field is still called `aborted`
/// for backwards compatibility with state files written before the rename;
/// the user-facing verb is `fold`.
fn write_state(
    dir: &Path,
    branch: &str,
    original_branch: Option<&str>,
    completed: &[&str],
    aborted: bool,
) {
    let started_at: DateTime<Utc> = DateTime::parse_from_rfc3339("2026-04-29T14:30:22Z")
        .unwrap()
        .with_timezone(&Utc);
    let state = json!({
        "run_id": "20260429T143022Z",
        "branch": branch,
        "original_branch": original_branch,
        "started_at": started_at.to_rfc3339(),
        "started_phase": "01",
        "completed": completed,
        "attempts": {},
        "token_usage": {"input": 0, "output": 0, "by_role": {}},
        "aborted": aborted,
    });
    let path = dir.join(".pitboss/state.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_string_pretty(&state).unwrap() + "\n").unwrap();
}

#[test]
fn status_with_no_run_says_not_started() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());

    pitboss()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("run: not started"))
        // The seed init template starts at phase 01 of 1.
        .stdout(contains("plan: phase 01 of 1"));
}

#[test]
fn status_after_state_seeded_shows_run_metadata() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());
    write_state(
        dir.path(),
        "pitboss/run-20260429T143022Z",
        Some("main"),
        &[],
        false,
    );

    pitboss()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("run: 20260429T143022Z"))
        .stdout(contains("branch: pitboss/run-20260429T143022Z"))
        .stdout(contains("original branch: main"))
        .stdout(contains("completed: (none)"))
        .stdout(contains("deferred items: 0"))
        .stdout(contains("tokens: input=0 output=0"));
}

#[test]
fn status_marks_folded_run() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());
    write_state(
        dir.path(),
        "pitboss/run-20260429T143022Z",
        Some("main"),
        &[],
        true,
    );

    pitboss()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("folded"));
}

#[test]
fn rebuy_with_no_state_errors_clearly() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());

    pitboss()
        .arg("rebuy")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains("no run to rebuy"));
}

#[test]
fn rebuy_with_folded_state_refuses() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());
    init_git_repo(dir.path());
    write_state(
        dir.path(),
        "pitboss/run-20260429T143022Z",
        Some("main"),
        &[],
        true,
    );

    pitboss()
        .arg("rebuy")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains("folded"));
}

#[test]
fn fold_with_no_state_errors() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());

    pitboss()
        .arg("fold")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains("no active run to fold"));
}

#[test]
fn fold_marks_state_aborted_and_persists_flag() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());
    init_git_repo(dir.path());
    // Pretend a run was previously started and halted.
    write_state(
        dir.path(),
        "pitboss/run-20260429T143022Z",
        Some("main"),
        &["01"],
        false,
    );
    // The branch must exist for `fold --checkout-original` (run by other
    // tests below) to work; the bare `fold` call doesn't need it but we set
    // it up here so we can compose this fixture in a later test if needed.
    PCommand::new("git")
        .args(["-C"])
        .arg(dir.path())
        .args(["branch", "pitboss/run-20260429T143022Z"])
        .status()
        .unwrap();

    pitboss()
        .arg("fold")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("folded run 20260429T143022Z"));

    // The state file's `aborted` field is still the on-disk source of truth.
    let state_text = fs::read_to_string(dir.path().join(".pitboss/state.json")).unwrap();
    assert!(
        state_text.contains("\"aborted\": true"),
        "state.json after fold: {state_text}"
    );

    // A subsequent `pitboss play` refuses on the folded state.
    pitboss()
        .arg("play")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains("folded"));
}

#[test]
fn fold_idempotent_second_call_is_a_noop_success() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());
    write_state(
        dir.path(),
        "pitboss/run-20260429T143022Z",
        Some("main"),
        &[],
        true,
    );

    pitboss()
        .arg("fold")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("was already folded"));
}

#[test]
fn fold_with_checkout_original_switches_branch() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());
    init_git_repo(dir.path());

    // Determine the initial branch (main on git >=2.28, master earlier).
    let original = String::from_utf8(
        PCommand::new("git")
            .args(["-C"])
            .arg(dir.path())
            .args(["branch", "--show-current"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert!(!original.is_empty(), "test setup: empty initial branch");

    // Create the per-run branch and switch onto it (mirrors what `pitboss play`
    // would have done before a halt).
    let run_branch = "pitboss/run-20260429T143022Z";
    PCommand::new("git")
        .args(["-C"])
        .arg(dir.path())
        .args(["branch", run_branch])
        .status()
        .unwrap();
    PCommand::new("git")
        .args(["-C"])
        .arg(dir.path())
        .args(["checkout", run_branch, "-q"])
        .status()
        .unwrap();
    write_state(dir.path(), run_branch, Some(&original), &[], false);

    pitboss()
        .arg("fold")
        .arg("--checkout-original")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("folded run"))
        .stdout(contains(format!("checked out {original}").as_str()));

    // Verify HEAD is back on the original branch.
    let after = String::from_utf8(
        PCommand::new("git")
            .args(["-C"])
            .arg(dir.path())
            .args(["branch", "--show-current"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(after, original);
}

#[test]
fn fold_with_checkout_original_errors_when_no_original_recorded() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());
    init_git_repo(dir.path());
    // No original_branch in the seeded state.
    write_state(dir.path(), "pitboss/run-x", None, &[], false);

    pitboss()
        .arg("fold")
        .arg("--checkout-original")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains("no original branch is recorded"));
}

/// The pre-rename verbs (`run`, `resume`, `abort`) keep working as clap
/// aliases for `play` / `rebuy` / `fold`. This test pins that promise so a
/// future refactor cannot silently drop the aliases.
#[test]
fn legacy_aliases_still_work() {
    let dir = tempdir().unwrap();
    init_workspace(dir.path());

    // `abort` aliases `fold`.
    pitboss()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains("no active run to fold"));

    // `resume` aliases `rebuy`.
    pitboss()
        .arg("resume")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains("no run to rebuy"));
}
