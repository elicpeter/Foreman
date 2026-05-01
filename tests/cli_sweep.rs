//! Integration tests for the `pitboss sweep` subcommand (phase 06).
//!
//! Drives the binary via `assert_cmd` against a tempdir-seeded workspace.
//! Programmatic coverage of `Runner::run_standalone_sweep` lives in
//! `tests/cli_play_sweep.rs`; this file's job is to close the loop on the
//! CLI plumbing — argument parsing, exit codes, and the synthesized-state
//! cleanup contract that keeps a one-shot sweep from claiming a workspace.

use std::fs;
use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::tempdir;

const ONE_PHASE_PLAN: &str = "\
---
current_phase: \"01\"
---

# Pitboss Plan

# Phase 01: Single

**Scope.** Only phase.
";

fn pitboss() -> Command {
    Command::cargo_bin("pitboss").expect("pitboss binary should be built")
}

fn isolated(workspace: &Path, home: &Path) -> Command {
    let mut cmd = pitboss();
    cmd.current_dir(workspace)
        .env("HOME", home)
        .env_remove("NO_COLOR")
        .env("NO_COLOR", "1");
    cmd
}

fn seed_workspace(workspace: &Path, deferred: &str) {
    fs::create_dir_all(workspace.join(".pitboss/play/logs")).unwrap();
    fs::write(workspace.join(".pitboss/play/plan.md"), ONE_PHASE_PLAN).unwrap();
    fs::write(workspace.join(".pitboss/play/deferred.md"), deferred).unwrap();
}

fn deferred_with_items(items: &[(&str, bool)]) -> String {
    let mut s = String::from("## Deferred items\n\n");
    for (text, done) in items {
        let mark = if *done { 'x' } else { ' ' };
        s.push_str(&format!("- [{mark}] {text}\n"));
    }
    s.push_str("\n## Deferred phases\n");
    s
}

fn init_git_repo(dir: &Path) {
    let status = StdCommand::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(dir)
        .status()
        .expect("git init");
    assert!(status.success(), "git init failed");
    for (k, v) in [
        ("user.name", "pitboss-test"),
        ("user.email", "pitboss@test"),
    ] {
        StdCommand::new("git")
            .args(["-C"])
            .arg(dir)
            .args(["config", k, v])
            .status()
            .unwrap();
    }
    let status = StdCommand::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["commit", "--allow-empty", "-m", "seed", "-q"])
        .status()
        .expect("git seed commit");
    assert!(status.success());
}

#[test]
fn sweep_dry_run_succeeds_on_fresh_workspace() {
    let work = tempdir().unwrap();
    let home = tempdir().unwrap();
    seed_workspace(
        work.path(),
        &deferred_with_items(&[("first", false), ("second", false)]),
    );
    init_git_repo(work.path());

    isolated(work.path(), home.path())
        .args(["sweep", "--dry-run"])
        .assert()
        .success();

    // The synthesized fresh state must not pollute the workspace: no
    // state.json should remain after a one-shot sweep against a
    // never-played workspace.
    let state_path = work.path().join(".pitboss/play/state.json");
    assert!(
        !state_path.exists(),
        "fresh-workspace dry-run sweep must not leave state.json behind; found {state_path:?}",
    );

    // Deferred file is unchanged: dry-run agent makes no edits.
    let deferred = fs::read_to_string(work.path().join(".pitboss/play/deferred.md")).unwrap();
    assert!(deferred.contains("- [ ] first"));
    assert!(deferred.contains("- [ ] second"));
}

#[test]
fn sweep_help_documents_flags() {
    let assert = pitboss().args(["sweep", "--help"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for flag in [
        "--max-items",
        "--audit",
        "--no-audit",
        "--dry-run",
        "--after",
    ] {
        assert!(out.contains(flag), "missing {flag} in --help: {out}");
    }
}

#[test]
fn sweep_audit_and_no_audit_are_mutually_exclusive() {
    let work = tempdir().unwrap();
    let home = tempdir().unwrap();
    seed_workspace(work.path(), &deferred_with_items(&[("only", false)]));
    init_git_repo(work.path());

    let assert = isolated(work.path(), home.path())
        .args(["sweep", "--audit", "--no-audit"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("--audit") && stderr.contains("--no-audit"),
        "expected clap to mention both flags; stderr:\n{stderr}",
    );
}

#[test]
fn sweep_rejects_invalid_after_phase_id() {
    let work = tempdir().unwrap();
    let home = tempdir().unwrap();
    seed_workspace(work.path(), &deferred_with_items(&[("only", false)]));
    init_git_repo(work.path());

    isolated(work.path(), home.path())
        .args(["sweep", "--dry-run", "--after", "not a phase id"])
        .assert()
        .failure()
        .stderr(contains("invalid --after phase id"));
}
