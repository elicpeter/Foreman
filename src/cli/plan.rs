//! `pitboss plan <goal>` — invoke the planner agent to scaffold a `plan.md`.
//!
//! Phase 15 wires the planner role end to end:
//!
//! 1. The CLI loads `pitboss.toml` to pick the planner model.
//! 2. A short repo overview is collected (top-level entries, package
//!    manifests, top-level READMEs) and threaded into [`prompts::planner`].
//! 3. The configured [`Agent`] is dispatched once. `Stdout` events are
//!    concatenated verbatim into the candidate `plan.md` body (the planner
//!    template instructs the model to emit only the file contents).
//! 4. The body is parsed with [`plan::parse`]. On success the runner writes
//!    it atomically. On failure the runner re-dispatches once with the parse
//!    diagnostic prepended to the prompt; a second failure is surfaced as a
//!    hard error.
//!
//! User-authored `plan.md` content is never overwritten silently — `--force`
//! is required to clobber an existing file. The one exception is the seed
//! `pitboss init` writes (see `crate::cli::init::PLAN_TEMPLATE`): when the
//! existing `plan.md` is byte-identical to that seed, the planner overwrites
//! it without `--force`, since the user demonstrably hasn't touched it. Any
//! deviation from the seed (even a single edited word) reverts to the
//! refuse-without-`--force` path.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{self, Agent, AgentEvent, AgentRequest, Role, StopReason};
use crate::config;
use crate::plan;
use crate::prompts;
use crate::style::{self, col};
use crate::util::write_atomic;

/// Wall-clock cap for a single planner dispatch. The planner runs once per
/// `pitboss plan` invocation (twice on the parse-failure retry path) so a
/// generous ceiling is fine; phase 18 makes timeouts configurable.
const PLANNER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Maximum number of agent dispatches allowed for a single `pitboss plan`
/// invocation. The first attempt uses the canonical prompt; if its output
/// fails to parse, the second attempt prepends the parser diagnostic. A
/// second parse failure is surfaced as an error.
const MAX_PLANNER_ATTEMPTS: u32 = 2;

/// What [`run_with_agent`] returns. Used by the CLI to print the summary line
/// and by tests to assert on the retry counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRunOutcome {
    /// Path that was written.
    pub plan_path: PathBuf,
    /// Number of planner dispatches it took to land a parseable plan
    /// (`1` on the happy path, `2` after a single retry).
    pub attempts: u32,
}

/// Top-level entry point for the `plan` subcommand. Builds the configured
/// backend via [`agent::build_agent`] and dispatches through
/// [`run_with_agent`].
///
/// When `interview` is `true`, the agent is first asked to generate targeted
/// design questions; the user answers them interactively. The Q&A spec is
/// appended to `goal` before the planner is dispatched, giving the model a
/// richer context for generating a precise `plan.md`.
pub async fn run(workspace: PathBuf, goal: String, force: bool, interview: bool) -> Result<()> {
    let cfg = config::load(&workspace)
        .with_context(|| format!("plan: loading config in {}", workspace.display()))?;
    let agent = agent::build_agent(&cfg)?;

    let effective_goal = if interview {
        let repo_summary = collect_repo_summary(&workspace)?;
        let spec = crate::cli::interview::conduct(
            &workspace,
            &goal,
            &repo_summary,
            &cfg,
            &agent,
            crate::cli::interview::DEFAULT_MAX_QUESTIONS,
        )
        .await?;
        format!("{goal}\n\n## Design Specification\n\n{spec}")
    } else {
        goal
    };

    let outcome = run_with_agent(&workspace, &effective_goal, force, &agent).await?;
    let c = style::use_color_stdout();
    println!(
        "{} {} ({} attempt{})",
        col(c, style::BOLD_GREEN, "wrote"),
        outcome.plan_path.display(),
        outcome.attempts,
        if outcome.attempts == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Test-friendly entry point that accepts any [`Agent`]. Tests pass a
/// `DryRunAgent` whose stdout script holds canned plan.md bodies; production
/// passes whatever backend [`agent::build_agent`] selected from
/// `pitboss.toml`.
pub async fn run_with_agent<A: Agent>(
    workspace: &Path,
    goal: &str,
    force: bool,
    agent: &A,
) -> Result<PlanRunOutcome> {
    let plan_path = workspace.join("plan.md");
    if plan_path.exists() && !force && !is_init_seed(&plan_path)? {
        bail!(
            "plan.md already exists at {}; pass --force to overwrite",
            plan_path.display()
        );
    }

    let cfg = config::load(workspace)
        .with_context(|| format!("plan: loading config in {}", workspace.display()))?;
    ensure_logs_dir(workspace)?;

    let repo_summary = collect_repo_summary(workspace)?;
    let base_prompt = prompts::planner(goal, &repo_summary);

    let c = style::use_color_stderr();
    let fm = col(c, style::BOLD_CYAN, "[pitboss]");

    let mut last_error: Option<String> = None;
    for attempt in 1..=MAX_PLANNER_ATTEMPTS {
        let user_prompt = match &last_error {
            None => base_prompt.clone(),
            Some(err) => prepend_retry_context(&base_prompt, err),
        };
        let log_path = planner_log_path(workspace, attempt);
        eprintln!(
            "{fm} {} {} (attempt {}/{})",
            col(c, style::MAGENTA, "dispatching planner"),
            col(c, style::CYAN, &cfg.models.planner),
            attempt,
            MAX_PLANNER_ATTEMPTS
        );
        eprintln!(
            "{fm} {}",
            col(c, style::DIM, &format!("live log: {}", log_path.display()))
        );
        let request = AgentRequest {
            role: Role::Planner,
            model: cfg.models.planner.clone(),
            system_prompt: prompts::caveman::system_prompt(&cfg.caveman),
            user_prompt,
            workdir: workspace.to_path_buf(),
            log_path,
            timeout: PLANNER_TIMEOUT,
        };

        let body = dispatch_planner(agent, request).await?;
        match plan::parse(&body) {
            Ok(_) => {
                write_atomic(&plan_path, body.as_bytes())
                    .with_context(|| format!("plan: writing {}", plan_path.display()))?;
                return Ok(PlanRunOutcome {
                    plan_path,
                    attempts: attempt,
                });
            }
            Err(e) => {
                let retrying = attempt < MAX_PLANNER_ATTEMPTS;
                eprintln!(
                    "{fm} {} {e}{}",
                    col(c, style::BOLD_YELLOW, "planner output failed to parse:"),
                    if retrying {
                        col(c, style::DIM, "; retrying")
                    } else {
                        String::new()
                    }
                );
                last_error = Some(format!("{e}"));
            }
        }
    }

    Err(anyhow!(
        "planner produced an unparsable plan {} times in a row; last error: {}",
        MAX_PLANNER_ATTEMPTS,
        last_error.unwrap_or_else(|| "(none captured)".into())
    ))
}

/// Cadence of the "still running…" heartbeat printed to stderr while the
/// planner agent is in flight. Long enough that a fast dry-run finishes
/// before the first tick (so unit tests stay quiet), short enough that a
/// real planner dispatch produces multiple pulses per minute.
const PLANNER_HEARTBEAT: Duration = Duration::from_secs(15);

/// Run the agent once and return the concatenated stdout body.
///
/// Stdout events are appended verbatim — the planner template instructs the
/// model to "Output ONLY the file contents", so consecutive text blocks are
/// pieces of the same plan.md rather than separate messages. Stderr from the
/// agent process is dropped here; it lives in the per-attempt log file for
/// post-mortem. `ToolUse` events are surfaced on stderr as they arrive so the
/// user sees the agent doing real work, and a periodic heartbeat reports
/// elapsed time so a quiet stretch (model thinking, no tool calls) doesn't
/// look like the command has hung.
async fn dispatch_planner<A: Agent>(agent: &A, request: AgentRequest) -> Result<String> {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let cancel = CancellationToken::new();

    // Color is resolved once per dispatch and captured by the spawned tasks.
    // `move` into each task is fine — `bool` is `Copy`.
    let c = style::use_color_stderr();

    let collector = tokio::spawn(async move {
        let mut buf = String::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Stdout(line) => buf.push_str(&line),
                AgentEvent::ToolUse(tool) => {
                    eprintln!(
                        "{}",
                        col(c, style::DARK_GRAY, &format!("[agent:tool] {tool}"))
                    );
                }
                AgentEvent::Stderr(_) | AgentEvent::TokenDelta(_) => {}
            }
        }
        buf
    });

    let heartbeat_stop = CancellationToken::new();
    let heartbeat_stop_inner = heartbeat_stop.clone();
    let heartbeat = tokio::spawn(async move {
        let fm = col(c, style::BOLD_CYAN, "[pitboss]");
        let start = std::time::Instant::now();
        let mut ticker = tokio::time::interval(PLANNER_HEARTBEAT);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Discard the immediate first tick so we don't print "0s elapsed"
        // before the agent has had a chance to do anything.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = heartbeat_stop_inner.cancelled() => break,
                _ = ticker.tick() => {
                    let elapsed = start.elapsed().as_secs();
                    eprintln!(
                        "{fm} {}",
                        col(
                            c,
                            style::DIM,
                            &format!("planner still running ({elapsed}s elapsed)")
                        )
                    );
                }
            }
        }
    });

    let agent_result = agent.run(request, tx, cancel).await;
    heartbeat_stop.cancel();
    let _ = heartbeat.await;
    let body = collector.await.unwrap_or_default();
    let outcome =
        agent_result.with_context(|| format!("plan: agent {:?} dispatch failed", agent.name()))?;

    match outcome.stop_reason {
        StopReason::Completed => {
            if outcome.exit_code != 0 {
                return Err(anyhow!(
                    "planner agent exited with code {}",
                    outcome.exit_code
                ));
            }
            Ok(body)
        }
        StopReason::Timeout => Err(anyhow!(
            "planner agent timed out after {:?}",
            PLANNER_TIMEOUT
        )),
        StopReason::Cancelled => Err(anyhow!("planner agent was cancelled")),
        StopReason::Error(msg) => Err(anyhow!("planner agent failed: {msg}")),
    }
}

/// Build the retry prompt by prepending a short error preamble to the
/// canonical planner prompt. Re-rendering the canonical body each retry keeps
/// the goal + repo summary in front of the model alongside the diagnostic.
fn prepend_retry_context(base: &str, err: &str) -> String {
    format!(
        "Your previous attempt produced output that failed to parse as plan.md.\n\
         \n\
         Parser error:\n\
         {err}\n\
         \n\
         Re-emit the file from scratch — output ONLY the file contents, no \
         commentary, no surrounding fences.\n\
         \n\
         ---\n\
         \n\
         {base}"
    )
}

/// Whether `path` is byte-identical to the `pitboss init` seed template.
///
/// Lets the planner silently overwrite a freshly scaffolded `plan.md`. Any
/// user edit — even reformatting whitespace — flips this to `false` and the
/// caller falls back to refusing without `--force`. I/O errors propagate so a
/// permission-denied or unreadable file surfaces clearly rather than being
/// misclassified as "not the seed".
fn is_init_seed(path: &Path) -> Result<bool> {
    let bytes = fs::read(path)
        .with_context(|| format!("plan: reading {} for seed comparison", path.display()))?;
    Ok(bytes == crate::cli::init::PLAN_TEMPLATE.as_bytes())
}

fn planner_log_path(workspace: &Path, attempt: u32) -> PathBuf {
    workspace
        .join(".pitboss")
        .join("logs")
        .join(format!("planner-attempt-{attempt}.log"))
}

fn ensure_logs_dir(workspace: &Path) -> Result<()> {
    let logs = workspace.join(".pitboss").join("logs");
    fs::create_dir_all(&logs).with_context(|| format!("plan: creating {}", logs.display()))?;
    Ok(())
}

/// Cap on entries collected for the top-level listing. Big enough to cover
/// any reasonable repo root, small enough that the prompt stays compact.
const TOP_LEVEL_ENTRY_CAP: usize = 80;
/// Per-file character cap for manifests and READMEs. Cheap defense against a
/// runaway README or vendored manifest blowing the prompt out.
const PER_FILE_CHAR_CAP: usize = 4_000;
/// Directories never worth showing the planner — purely build / VCS / agent
/// state.
const SKIP_DIRS: &[&str] = &[
    ".pitboss",
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
];

/// Manifests we surface verbatim (truncated). Order is the order the planner
/// sees them.
const MANIFEST_FILES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "requirements.txt",
];

/// READMEs we surface verbatim (truncated). Lowercase variants are tried too.
const README_FILES: &[&str] = &["README.md", "README", "README.txt", "README.rst"];

fn collect_repo_summary(workspace: &Path) -> Result<String> {
    let mut sections: Vec<String> = Vec::new();
    sections.push(format!(
        "Top-level entries:\n{}",
        top_level_listing(workspace)?
    ));
    if let Some(s) = collect_files(workspace, MANIFEST_FILES, "Package manifests")? {
        sections.push(s);
    }
    if let Some(s) = collect_files(workspace, README_FILES, "Top-level READMEs")? {
        sections.push(s);
    }
    Ok(sections.join("\n\n"))
}

fn top_level_listing(workspace: &Path) -> Result<String> {
    let read = match fs::read_dir(workspace) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok("(workspace is empty)".to_string())
        }
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("plan: listing {}", workspace.display()))
            );
        }
    };

    let mut entries: Vec<(String, bool)> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && name != ".gitignore" {
            // Hidden files / dirs are noise unless they're meaningful for
            // packaging (.gitignore is the only one we surface here).
            continue;
        }
        if SKIP_DIRS.iter().any(|d| *d == name) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push((name, is_dir));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    if entries.is_empty() {
        return Ok("(empty)".to_string());
    }

    let truncated = entries.len() > TOP_LEVEL_ENTRY_CAP;
    let display = entries
        .iter()
        .take(TOP_LEVEL_ENTRY_CAP)
        .map(|(name, is_dir)| {
            if *is_dir {
                format!("- {name}/")
            } else {
                format!("- {name}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if truncated {
        Ok(format!(
            "{display}\n- … ({} more entries omitted)",
            entries.len() - TOP_LEVEL_ENTRY_CAP
        ))
    } else {
        Ok(display)
    }
}

fn collect_files(workspace: &Path, names: &[&str], header: &str) -> Result<Option<String>> {
    let mut chunks: Vec<String> = Vec::new();
    for name in names {
        let path = workspace.join(name);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // Binary file — skip silently.
                continue;
            }
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("plan: reading {}", path.display()))
                )
            }
        };
        let trimmed = truncate_for_summary(&text);
        chunks.push(format!("### {name}\n\n```\n{trimmed}\n```"));
    }
    if chunks.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!("{header}:\n\n{}", chunks.join("\n\n"))))
    }
}

fn truncate_for_summary(text: &str) -> String {
    if text.len() <= PER_FILE_CHAR_CAP {
        return text.trim_end_matches('\n').to_string();
    }
    let mut out = text[..PER_FILE_CHAR_CAP].to_string();
    // Don't slice mid-codepoint — back up to a char boundary.
    while !out.is_char_boundary(out.len()) {
        out.pop();
    }
    out.push_str("\n… (truncated)");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::dry_run::{DryRunAgent, DryRunFinal};
    use crate::state::TokenUsage;
    use tempfile::tempdir;

    /// Minimal valid plan.md the dry-run agent can emit.
    const CANNED_PLAN: &str = "\
---
current_phase: \"01\"
---

# Pitboss Plan

# Phase 01: First

**Scope.** Stand it up.

**Deliverables.**
- crate

**Acceptance.**
- builds
";

    fn dry_agent_emitting(body: &str) -> DryRunAgent {
        DryRunAgent::new("planner-test")
            .emit(AgentEvent::Stdout(body.to_string()))
            .finish(DryRunFinal::Success {
                exit_code: 0,
                tokens: TokenUsage::default(),
            })
    }

    #[tokio::test]
    async fn happy_path_writes_plan_md_on_first_attempt() {
        let dir = tempdir().unwrap();
        let agent = dry_agent_emitting(CANNED_PLAN);

        let outcome = run_with_agent(dir.path(), "build a thing", false, &agent)
            .await
            .unwrap();

        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.plan_path, dir.path().join("plan.md"));
        let written = fs::read_to_string(dir.path().join("plan.md")).unwrap();
        assert_eq!(written, CANNED_PLAN);
    }

    #[tokio::test]
    async fn refuses_to_overwrite_existing_plan_without_force() {
        let dir = tempdir().unwrap();
        let preexisting = "preexisting plan body\n";
        fs::write(dir.path().join("plan.md"), preexisting).unwrap();

        let agent = dry_agent_emitting(CANNED_PLAN);
        let err = run_with_agent(dir.path(), "build", false, &agent)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("--force"), "err: {err}");
        let after = fs::read_to_string(dir.path().join("plan.md")).unwrap();
        assert_eq!(
            after, preexisting,
            "plan.md must be untouched without --force"
        );
    }

    #[tokio::test]
    async fn unmodified_init_seed_is_overwritten_without_force() {
        // `pitboss init` followed by `pitboss plan` is the canonical
        // first-run flow. The init-seeded `plan.md` must be replaced
        // silently — requiring `--force` here was a UX bug.
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("plan.md"),
            crate::cli::init::PLAN_TEMPLATE.as_bytes(),
        )
        .unwrap();
        let agent = dry_agent_emitting(CANNED_PLAN);

        let outcome = run_with_agent(dir.path(), "build", false, &agent)
            .await
            .unwrap();

        assert_eq!(outcome.attempts, 1);
        let written = fs::read_to_string(dir.path().join("plan.md")).unwrap();
        assert_eq!(written, CANNED_PLAN);
    }

    #[tokio::test]
    async fn edited_init_seed_still_requires_force() {
        // A single byte of user edits over the seed flips it back to the
        // refuse-without-force path. Anything other than a verbatim seed
        // is treated as user-authored content.
        let dir = tempdir().unwrap();
        let mut edited = crate::cli::init::PLAN_TEMPLATE.to_string();
        edited.push_str("\nuser added a note here\n");
        fs::write(dir.path().join("plan.md"), edited.as_bytes()).unwrap();
        let agent = dry_agent_emitting(CANNED_PLAN);

        let err = run_with_agent(dir.path(), "build", false, &agent)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--force"), "err: {err}");
        let after = fs::read_to_string(dir.path().join("plan.md")).unwrap();
        assert_eq!(after, edited, "edited plan.md must be untouched");
    }

    #[tokio::test]
    async fn force_flag_overwrites_existing_plan() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("plan.md"), b"old content\n").unwrap();
        let agent = dry_agent_emitting(CANNED_PLAN);

        let outcome = run_with_agent(dir.path(), "build", true, &agent)
            .await
            .unwrap();

        assert_eq!(outcome.attempts, 1);
        let written = fs::read_to_string(dir.path().join("plan.md")).unwrap();
        assert_eq!(written, CANNED_PLAN);
    }

    #[tokio::test]
    async fn validation_retry_loop_recovers_on_attempt_2() {
        // First attempt emits garbage (no frontmatter); second emits a valid
        // plan. The retry path must succeed and report attempts == 2.
        let dir = tempdir().unwrap();
        let agent = QueuedPlannerAgent::new(vec![
            "garbage without frontmatter\n".to_string(),
            CANNED_PLAN.to_string(),
        ]);
        let outcome = run_with_agent(dir.path(), "g", false, &agent)
            .await
            .unwrap();
        assert_eq!(outcome.attempts, 2);
        let written = fs::read_to_string(dir.path().join("plan.md")).unwrap();
        assert_eq!(written, CANNED_PLAN);
    }

    #[tokio::test]
    async fn validation_retry_loop_fails_after_two_bad_outputs() {
        let dir = tempdir().unwrap();
        let agent = QueuedPlannerAgent::new(vec![
            "still not a plan\n".to_string(),
            "still garbage\n".to_string(),
        ]);
        let err = run_with_agent(dir.path(), "g", false, &agent)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unparsable"),
            "expected a parse-failure summary, got: {msg}"
        );
        assert!(
            !dir.path().join("plan.md").exists(),
            "plan.md must not be written on consecutive parse failures"
        );
    }

    #[tokio::test]
    async fn agent_failure_surfaces_as_error_with_no_retry() {
        // An agent-side failure is NOT a parse failure — the loop must not
        // retry and plan.md must not be written.
        let dir = tempdir().unwrap();
        let agent = DryRunAgent::new("planner-fail").finish(DryRunFinal::Error("boom".into()));
        let err = run_with_agent(dir.path(), "g", false, &agent)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("boom"));
        assert!(!dir.path().join("plan.md").exists());
    }

    #[tokio::test]
    async fn writes_per_attempt_log_paths_under_dot_pitboss() {
        // The runner pre-creates `.pitboss/logs/`. After a single attempt the
        // dir exists even if the agent didn't write the log itself (DryRun).
        let dir = tempdir().unwrap();
        let agent = dry_agent_emitting(CANNED_PLAN);
        let _ = run_with_agent(dir.path(), "g", false, &agent)
            .await
            .unwrap();
        assert!(dir.path().join(".pitboss/logs").is_dir());
    }

    #[test]
    fn top_level_listing_filters_skip_dirs_and_hidden() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".pitboss")).unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "target\n").unwrap();

        let listing = top_level_listing(dir.path()).unwrap();
        assert!(listing.contains("Cargo.toml"));
        assert!(listing.contains("src/"));
        assert!(listing.contains(".gitignore"));
        assert!(!listing.contains("target"));
        assert!(!listing.contains(".pitboss"));
        assert!(!listing.contains(".env"));
    }

    #[test]
    fn collect_files_truncates_long_inputs() {
        let dir = tempdir().unwrap();
        let huge = "x".repeat(PER_FILE_CHAR_CAP * 2);
        fs::write(dir.path().join("README.md"), &huge).unwrap();
        let section = collect_files(dir.path(), README_FILES, "Top-level READMEs")
            .unwrap()
            .expect("README section");
        assert!(section.contains("README.md"));
        assert!(section.contains("(truncated)"));
        assert!(section.len() < huge.len());
    }

    #[test]
    fn collect_files_returns_none_when_nothing_matches() {
        let dir = tempdir().unwrap();
        let section = collect_files(dir.path(), MANIFEST_FILES, "x").unwrap();
        assert!(section.is_none());
    }

    #[test]
    fn collect_repo_summary_includes_manifest_and_listing() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("README.md"), "# demo\n").unwrap();
        let summary = collect_repo_summary(dir.path()).unwrap();
        assert!(summary.contains("Top-level entries"));
        assert!(summary.contains("Cargo.toml"));
        assert!(summary.contains("Package manifests"));
        assert!(summary.contains("Top-level READMEs"));
        assert!(summary.contains("[package]"));
        assert!(summary.contains("# demo"));
    }

    #[test]
    fn retry_prompt_carries_error_and_canonical_body() {
        let base = "(canonical prompt body)";
        let out = prepend_retry_context(base, "missing frontmatter");
        assert!(out.contains("missing frontmatter"));
        assert!(out.contains("output ONLY"));
        assert!(out.ends_with(base));
    }

    /// Minimal multi-dispatch agent: pops the next stdout body off a shared
    /// queue per call so the retry loop can be exercised end to end.
    struct QueuedPlannerAgent {
        bodies: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl QueuedPlannerAgent {
        fn new(bodies: Vec<String>) -> Self {
            Self {
                bodies: std::sync::Mutex::new(bodies.into()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Agent for QueuedPlannerAgent {
        fn name(&self) -> &str {
            "queued-planner"
        }
        async fn run(
            &self,
            req: AgentRequest,
            events: tokio::sync::mpsc::Sender<AgentEvent>,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<crate::agent::AgentOutcome> {
            let body = self.bodies.lock().unwrap().pop_front().unwrap_or_default();
            let _ = events.send(AgentEvent::Stdout(body)).await;
            Ok(crate::agent::AgentOutcome {
                exit_code: 0,
                stop_reason: StopReason::Completed,
                tokens: TokenUsage::default(),
                log_path: req.log_path,
            })
        }
    }
}
