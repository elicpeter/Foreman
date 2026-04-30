//! `foreman init` — scaffold a workspace.
//!
//! Idempotent and never destructive: every artifact is created only when
//! missing; pre-existing files are left byte-for-byte alone with a warning on
//! stderr. The summary printed to stdout reports `created` / `skipped` /
//! `updated` for each path so a re-run shows at a glance what changed.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

use crate::state;
use crate::util::write_atomic;

/// One-phase template seed for `plan.md`. Designed to round-trip through
/// [`crate::plan::parse`] so a freshly scaffolded plan parses cleanly without
/// edits.
const PLAN_TEMPLATE: &str = "\
---
current_phase: \"01\"
---

# Foreman Plan

Replace this preamble with a description of the work foreman will orchestrate.

# Phase 01: First phase

**Scope.** Describe what this phase delivers.

**Deliverables.**
- Item

**Acceptance.**
- Criterion
";

/// Empty scaffold for `deferred.md`. Both H2 sections are present so users see
/// the structure agents will write into; both are empty, which the parser
/// accepts.
const DEFERRED_TEMPLATE: &str = "\
## Deferred items

## Deferred phases
";

/// Default `foreman.toml`. The schema is finalized in phase 6; values here are
/// sensible placeholders so the file parses against any reasonable loader.
const FOREMAN_TOML_TEMPLATE: &str = "\
# foreman configuration

[models]
planner = \"claude-sonnet-4-6\"
implementer = \"claude-sonnet-4-6\"
auditor = \"claude-sonnet-4-6\"
fixer = \"claude-sonnet-4-6\"

[retries]
fixer_max_attempts = 2
max_phase_attempts = 3

[audit]
enabled = true
small_fix_line_limit = 30

[git]
branch_prefix = \"foreman/run-\"
create_pr = false
";

/// Marker line appended to `.gitignore`. Matched verbatim against trimmed
/// existing lines; only `".foreman"` and `".foreman/"` are recognized as
/// already-present.
const GITIGNORE_ENTRY: &str = ".foreman/";

/// What `init` did (or didn't do) to a single path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// File or directory was newly created.
    Created,
    /// Path already existed; left untouched.
    Skipped,
    /// File existed but was modified (currently only `.gitignore`).
    Updated,
}

/// One row of the per-file summary printed by [`run`].
#[derive(Debug, Clone)]
pub struct ReportEntry {
    /// Workspace-relative display path.
    pub path: String,
    /// Action taken.
    pub action: Action,
}

/// Scaffold a foreman workspace under `workspace`. Idempotent.
///
/// Stdout receives one line per artifact ("created plan.md",
/// "skipped plan.md (already exists)", etc.). Stderr receives a warning for
/// each pre-existing file we left alone, so users notice when init found a
/// populated workspace.
pub fn run(workspace: impl AsRef<Path>) -> Result<()> {
    let workspace = workspace.as_ref();
    let mut report: Vec<ReportEntry> = Vec::new();

    fs::create_dir_all(workspace)
        .with_context(|| format!("init: creating workspace {:?}", workspace))?;

    write_if_missing(workspace, "plan.md", PLAN_TEMPLATE.as_bytes(), &mut report)?;
    write_if_missing(
        workspace,
        "deferred.md",
        DEFERRED_TEMPLATE.as_bytes(),
        &mut report,
    )?;
    write_if_missing(
        workspace,
        "foreman.toml",
        FOREMAN_TOML_TEMPLATE.as_bytes(),
        &mut report,
    )?;

    ensure_dir(workspace, ".foreman", &mut report)?;
    ensure_dir(workspace, ".foreman/snapshots", &mut report)?;
    ensure_dir(workspace, ".foreman/logs", &mut report)?;

    init_state_file(workspace, &mut report)?;
    update_gitignore(workspace, &mut report)?;

    print_summary(&report);
    Ok(())
}

fn write_if_missing(
    workspace: &Path,
    rel: &str,
    contents: &[u8],
    report: &mut Vec<ReportEntry>,
) -> Result<()> {
    let path = workspace.join(rel);
    if path.exists() {
        warn_skipped(rel);
        report.push(ReportEntry {
            path: rel.to_string(),
            action: Action::Skipped,
        });
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("init: creating parent of {:?}", path))?;
    }
    write_atomic(&path, contents)?;
    report.push(ReportEntry {
        path: rel.to_string(),
        action: Action::Created,
    });
    Ok(())
}

fn ensure_dir(workspace: &Path, rel: &str, report: &mut Vec<ReportEntry>) -> Result<()> {
    let path = workspace.join(rel);
    let display = format!("{}/", rel);
    if path.is_dir() {
        report.push(ReportEntry {
            path: display,
            action: Action::Skipped,
        });
        return Ok(());
    }
    if path.exists() {
        // Path exists but isn't a directory — refuse rather than clobber.
        anyhow::bail!(
            "init: {:?} exists but is not a directory; refusing to overwrite",
            path
        );
    }
    fs::create_dir_all(&path).with_context(|| format!("init: creating {:?}", path))?;
    report.push(ReportEntry {
        path: display,
        action: Action::Created,
    });
    Ok(())
}

fn init_state_file(workspace: &Path, report: &mut Vec<ReportEntry>) -> Result<()> {
    let path = state::state_path(workspace);
    let rel = ".foreman/state.json".to_string();
    if path.exists() {
        warn_skipped(&rel);
        report.push(ReportEntry {
            path: rel,
            action: Action::Skipped,
        });
        return Ok(());
    }
    state::save(workspace, None)?;
    report.push(ReportEntry {
        path: rel,
        action: Action::Created,
    });
    Ok(())
}

fn update_gitignore(workspace: &Path, report: &mut Vec<ReportEntry>) -> Result<()> {
    let path = workspace.join(".gitignore");
    let rel = ".gitignore".to_string();

    let existing = match fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("init: reading {:?}", path))
            )
        }
    };

    if let Some(ref text) = existing {
        if has_foreman_entry(text) {
            report.push(ReportEntry {
                path: rel,
                action: Action::Skipped,
            });
            return Ok(());
        }
    }

    let new_contents = append_entry(existing.as_deref());
    write_atomic(&path, new_contents.as_bytes())?;
    report.push(ReportEntry {
        path: rel,
        action: if existing.is_some() {
            Action::Updated
        } else {
            Action::Created
        },
    });
    Ok(())
}

fn has_foreman_entry(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return false;
        }
        // Strip an optional leading slash so `/.foreman/` and `.foreman/` both
        // count as the same entry. Trailing slash is also optional.
        let canonical = trimmed
            .trim_start_matches('/')
            .trim_end_matches('/');
        canonical == ".foreman"
    })
}

fn append_entry(existing: Option<&str>) -> String {
    let mut out = match existing {
        Some(s) => s.to_string(),
        None => String::new(),
    };
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(GITIGNORE_ENTRY);
    out.push('\n');
    out
}

fn warn_skipped(rel: &str) {
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    // Best-effort: warning output is informational and we don't want a write
    // error to fail the whole init.
    let _ = writeln!(handle, "warning: {} already exists, leaving it alone", rel);
}

fn print_summary(report: &[ReportEntry]) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    for entry in report {
        let line = match entry.action {
            Action::Created => format!("created {}", entry.path),
            Action::Skipped => format!("skipped {} (already exists)", entry.path),
            Action::Updated => format!("updated {}", entry.path),
        };
        let _ = writeln!(handle, "{}", line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn paths_with(report: &[ReportEntry], action: Action) -> Vec<&str> {
        report
            .iter()
            .filter(|e| e.action == action)
            .map(|e| e.path.as_str())
            .collect()
    }

    #[test]
    fn fresh_workspace_creates_every_artifact() {
        let dir = tempdir().unwrap();
        run(dir.path()).unwrap();

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
                "expected {:?} to be created",
                rel
            );
        }

        // plan.md template parses cleanly.
        let plan_text = fs::read_to_string(dir.path().join("plan.md")).unwrap();
        let plan = crate::plan::parse(&plan_text).expect("seed plan.md must parse");
        assert_eq!(plan.current_phase.as_str(), "01");

        // deferred.md template parses cleanly.
        let deferred_text = fs::read_to_string(dir.path().join("deferred.md")).unwrap();
        crate::deferred::parse(&deferred_text).expect("seed deferred.md must parse");

        // state.json is JSON null (no run started).
        assert!(state::load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn rerun_is_idempotent_and_skips_everything() {
        let dir = tempdir().unwrap();
        run(dir.path()).unwrap();

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

        run(dir.path()).unwrap();

        let after: Vec<Vec<u8>> = snapshot_paths
            .iter()
            .map(|p| fs::read(dir.path().join(p)).unwrap())
            .collect();
        assert_eq!(before, after, "rerun must not modify any artifact");
    }

    #[test]
    fn preexisting_plan_md_survives_byte_for_byte() {
        let dir = tempdir().unwrap();
        let custom = "---\ncurrent_phase: \"05\"\n---\n\n# Phase 05: Custom\n\nbody.\n";
        fs::write(dir.path().join("plan.md"), custom).unwrap();

        run(dir.path()).unwrap();

        let after = fs::read_to_string(dir.path().join("plan.md")).unwrap();
        assert_eq!(after, custom);
    }

    #[test]
    fn gitignore_is_created_with_foreman_entry() {
        let dir = tempdir().unwrap();
        run(dir.path()).unwrap();
        let gi = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gi.contains(".foreman/"));
    }

    #[test]
    fn gitignore_is_appended_when_entry_missing() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "/target\n").unwrap();
        run(dir.path()).unwrap();
        let gi = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gi.starts_with("/target\n"));
        assert!(gi.contains(".foreman/"));
    }

    #[test]
    fn gitignore_entry_recognized_in_several_forms() {
        for line in [".foreman", ".foreman/", "/.foreman", "/.foreman/"] {
            let dir = tempdir().unwrap();
            fs::write(
                dir.path().join(".gitignore"),
                format!("/target\n{}\n", line),
            )
            .unwrap();
            run(dir.path()).unwrap();
            let gi = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
            // No duplicate appended.
            let occurrences = gi
                .lines()
                .filter(|l| {
                    let t = l.trim().trim_start_matches('/').trim_end_matches('/');
                    t == ".foreman"
                })
                .count();
            assert_eq!(occurrences, 1, "input form {:?}, full file: {:?}", line, gi);
        }
    }

    #[test]
    fn gitignore_idempotent_across_many_runs() {
        let dir = tempdir().unwrap();
        for _ in 0..3 {
            run(dir.path()).unwrap();
        }
        let gi = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        let occurrences = gi
            .lines()
            .filter(|l| l.trim().trim_start_matches('/').trim_end_matches('/') == ".foreman")
            .count();
        assert_eq!(occurrences, 1);
    }

    #[test]
    fn rejects_non_directory_at_dot_foreman() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".foreman"), b"oops").unwrap();
        let err = run(dir.path()).unwrap_err();
        assert!(err.to_string().contains("is not a directory"));
    }

    #[test]
    fn report_describes_skipped_files() {
        // We can't observe `run`'s report directly (it's printed), but we can
        // exercise the lower-level helpers to ensure the Action variants are
        // produced as expected.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("plan.md"), "preexisting\n").unwrap();

        let mut report = Vec::new();
        write_if_missing(dir.path(), "plan.md", PLAN_TEMPLATE.as_bytes(), &mut report).unwrap();
        write_if_missing(
            dir.path(),
            "deferred.md",
            DEFERRED_TEMPLATE.as_bytes(),
            &mut report,
        )
        .unwrap();

        assert_eq!(paths_with(&report, Action::Skipped), vec!["plan.md"]);
        assert_eq!(paths_with(&report, Action::Created), vec!["deferred.md"]);
    }

}
