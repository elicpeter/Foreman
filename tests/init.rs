//! Integration tests for `foreman init`.
//!
//! Drives the binary via `assert_cmd` against a temp directory so we exercise
//! the full clap-dispatch path (and prove `init` honors `current_dir()`).

use std::fs;

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::tempdir;

fn foreman() -> Command {
    Command::cargo_bin("foreman").expect("foreman binary should be built")
}

#[test]
fn fresh_init_creates_every_artifact() {
    let dir = tempdir().unwrap();

    foreman()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("created plan.md"))
        .stdout(contains("created deferred.md"))
        .stdout(contains("created foreman.toml"))
        .stdout(contains("created .foreman/"))
        .stdout(contains("created .foreman/snapshots/"))
        .stdout(contains("created .foreman/logs/"))
        .stdout(contains("created .foreman/state.json"));

    for rel in [
        "plan.md",
        "deferred.md",
        "foreman.toml",
        ".foreman",
        ".foreman/snapshots",
        ".foreman/logs",
        ".foreman/state.json",
        ".gitignore",
    ] {
        assert!(
            dir.path().join(rel).exists(),
            "expected {:?} after init",
            rel
        );
    }
}

#[test]
fn rerun_init_is_idempotent_and_prints_skipped() {
    let dir = tempdir().unwrap();

    foreman()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .success();

    let snapshot_paths = [
        "plan.md",
        "deferred.md",
        "foreman.toml",
        ".foreman/state.json",
        ".gitignore",
    ];
    let before: Vec<Vec<u8>> = snapshot_paths
        .iter()
        .map(|p| fs::read(dir.path().join(p)).unwrap())
        .collect();

    foreman()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("skipped plan.md (already exists)"))
        .stdout(contains("skipped deferred.md (already exists)"))
        .stdout(contains("skipped foreman.toml (already exists)"))
        .stdout(contains("skipped .foreman/state.json (already exists)"))
        .stdout(contains("skipped .gitignore (already exists)"));

    let after: Vec<Vec<u8>> = snapshot_paths
        .iter()
        .map(|p| fs::read(dir.path().join(p)).unwrap())
        .collect();
    assert_eq!(before, after, "rerun must not modify any artifact");
}

#[test]
fn preexisting_plan_md_survives_byte_for_byte_with_warning_on_stderr() {
    let dir = tempdir().unwrap();
    let custom = "---\ncurrent_phase: \"05\"\n---\n\n# Phase 05: Custom\n\nhand-written body.\n";
    fs::write(dir.path().join("plan.md"), custom).unwrap();

    foreman()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("skipped plan.md (already exists)"))
        .stderr(contains(
            "warning: plan.md already exists, leaving it alone",
        ));

    let after = fs::read_to_string(dir.path().join("plan.md")).unwrap();
    assert_eq!(after, custom, "init must not touch a pre-existing plan.md");
}

#[test]
fn gitignore_is_updated_idempotently() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join(".gitignore"), "/target\n").unwrap();

    for _ in 0..3 {
        foreman()
            .arg("init")
            .current_dir(dir.path())
            .assert()
            .success();
    }

    let gi = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gi.starts_with("/target\n"), "preserves existing entry");
    let occurrences = gi
        .lines()
        .filter(|l| l.trim().trim_start_matches('/').trim_end_matches('/') == ".foreman")
        .count();
    assert_eq!(occurrences, 1, ".foreman entry must appear exactly once");
}

#[test]
fn preexisting_gitignore_with_foreman_entry_is_left_alone() {
    let dir = tempdir().unwrap();
    let original = "/target\n.foreman/\n";
    fs::write(dir.path().join(".gitignore"), original).unwrap();

    foreman()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("skipped .gitignore (already exists)"));

    let gi = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert_eq!(gi, original);
}
