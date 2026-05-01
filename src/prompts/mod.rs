//! System + user prompt templates for each agent role.
//!
//! Templates live as `.txt` files under [`templates/`](self) and are embedded
//! at build time via [`include_str!`]. Each public function in this module
//! pairs one template with the typed inputs the runner has on hand and returns
//! a single rendered string ready to hand to an [`crate::agent::Agent`].
//!
//! # Templating
//!
//! The internal `render` helper is a deliberately small placeholder
//! substituter: `{name}` is replaced with the value matching `name` in the
//! supplied list, and the escapes `{{` and `}}` produce literal braces.
//! There is no logic, no loops,
//! no conditionals — anything that needs branching belongs in Rust, not in the
//! template. Unknown placeholder names panic at run time, which surfaces typos
//! in tests rather than letting them ship as garbled prompts.
//!
//! # Iteration
//!
//! The whole point of pulling prompts out of the runner is so they can be
//! tuned in isolation. The associated `insta` snapshot tests freeze the
//! rendered output for a representative input fixture; any prompt change is
//! one snapshot review away from being merged. Run
//! `cargo insta review --workspace` to accept new snapshots.

pub mod caveman;

use crate::deferred::{self, DeferredDoc};
use crate::plan::{Phase, PhaseId, Plan};

const IMPLEMENTER_TEMPLATE: &str = include_str!("templates/implementer.txt");
const AUDITOR_TEMPLATE: &str = include_str!("templates/auditor.txt");
const FIXER_TEMPLATE: &str = include_str!("templates/fixer.txt");
const SWEEP_TEMPLATE: &str = include_str!("templates/sweep.txt");
const SWEEP_FIXER_TEMPLATE: &str = include_str!("templates/sweep_fixer.txt");
const SWEEP_AUDITOR_TEMPLATE: &str = include_str!("templates/sweep_auditor.txt");
const PLANNER_TEMPLATE: &str = include_str!("templates/planner.txt");
const QUESTIONER_TEMPLATE: &str = include_str!("templates/questioner.txt");

/// Approximate ceiling on the static portion of any single template.
///
/// The rendered prompt is variable in size (it embeds the current phase body,
/// `deferred.md`, diffs, etc.) so this budget bounds what we ship in source,
/// not what the agent sees. Tightening it forces template authors to keep
/// instructions terse; loosening it should be deliberate.
pub const TEMPLATE_STATIC_BUDGET: usize = 8_000;

/// Render the implementer prompt. The output instructs the agent to sweep
/// `deferred.md`, then implement the current phase, then re-record any
/// unfinished work, while leaving `plan.md` and `.pitboss/` untouched.
pub fn implementer(_plan: &Plan, deferred: &DeferredDoc, current: &Phase) -> String {
    render(
        IMPLEMENTER_TEMPLATE,
        &[
            ("phase_id", current.id.as_str()),
            ("phase_title", current.title.as_str()),
            ("phase_body", current.body.as_str()),
            ("deferred", &serialize_deferred_for_prompt(deferred)),
        ],
    )
}

/// Render the auditor prompt. `diff` is the implementer's working-tree diff
/// against the run's base branch. `small_fix_line_limit` is the threshold from
/// [`crate::config::AuditConfig::small_fix_line_limit`]; the auditor inlines
/// changes at or below it and defers anything larger.
pub fn auditor(_plan: &Plan, current: &Phase, diff: &str, small_fix_line_limit: u32) -> String {
    let limit = small_fix_line_limit.to_string();
    render(
        AUDITOR_TEMPLATE,
        &[
            ("phase_id", current.id.as_str()),
            ("phase_title", current.title.as_str()),
            ("phase_body", current.body.as_str()),
            ("diff", diff),
            ("deferred", "(no deferred.md provided yet)"),
            ("small_fix_line_limit", &limit),
        ],
    )
}

/// Render the fixer prompt. `test_output` is the captured stdout/stderr from
/// the failing test run that triggered this dispatch.
pub fn fixer(_plan: &Plan, current: &Phase, test_output: &str) -> String {
    render(
        FIXER_TEMPLATE,
        &[
            ("phase_id", current.id.as_str()),
            ("phase_title", current.title.as_str()),
            ("phase_body", current.body.as_str()),
            ("test_output", test_output),
            ("deferred", "(no deferred.md provided yet)"),
        ],
    )
}

/// Render the planner prompt. `goal` is the user's free-form description of
/// what they want built; `repo_summary` is a short overview of the existing
/// repo layout (top-level files, package manifests, key READMEs) that
/// `pitboss plan` collects before dispatching the agent.
pub fn planner(goal: &str, repo_summary: &str) -> String {
    render(
        PLANNER_TEMPLATE,
        &[("goal", goal), ("repo_summary", repo_summary)],
    )
}

/// Render the questioner prompt used by `pitboss plan --interview`. Asks the
/// agent to produce a numbered list of design questions (at most
/// `max_questions`) tailored to the goal and repo context.
pub fn questioner(goal: &str, repo_summary: &str, max_questions: u32) -> String {
    let max = max_questions.to_string();
    render(
        QUESTIONER_TEMPLATE,
        &[
            ("goal", goal),
            ("repo_summary", repo_summary),
            ("max_questions", &max),
        ],
    )
}

/// Variant of [`auditor`] that accepts an explicit `deferred.md` rendering, so
/// the runner can present the same canonical text the agent will see on disk.
pub fn auditor_with_deferred(
    _plan: &Plan,
    current: &Phase,
    diff: &str,
    deferred: &DeferredDoc,
    small_fix_line_limit: u32,
) -> String {
    let limit = small_fix_line_limit.to_string();
    render(
        AUDITOR_TEMPLATE,
        &[
            ("phase_id", current.id.as_str()),
            ("phase_title", current.title.as_str()),
            ("phase_body", current.body.as_str()),
            ("diff", diff),
            ("deferred", &serialize_deferred_for_prompt(deferred)),
            ("small_fix_line_limit", &limit),
        ],
    )
}

/// Variant of [`fixer`] that accepts an explicit `deferred.md` rendering.
pub fn fixer_with_deferred(
    _plan: &Plan,
    current: &Phase,
    test_output: &str,
    deferred: &DeferredDoc,
) -> String {
    render(
        FIXER_TEMPLATE,
        &[
            ("phase_id", current.id.as_str()),
            ("phase_title", current.title.as_str()),
            ("phase_body", current.body.as_str()),
            ("test_output", test_output),
            ("deferred", &serialize_deferred_for_prompt(deferred)),
        ],
    )
}

/// Render the sweep auditor prompt. Mirrors [`auditor_with_deferred`] but is
/// scoped to a deferred-sweep dispatch instead of a plan phase. `resolved` is
/// the list of `## Deferred items` text the sweep implementer flipped from
/// `- [ ]` to `- [x]` in this dispatch; `remaining` is the unchecked-item text
/// still pending after the sweep. `diff` is the staged `git diff --cached`.
/// `stale_items` is the runner's per-item staleness snapshot — items at or
/// above [`crate::config::SweepConfig::escalate_after`] — so the auditor can
/// be more critical about a `- [x]` claim against an item that has resisted
/// previous sweeps. Empty slice renders a `(none)` marker.
/// The auditor's contract is "for each resolved item, does the diff actually
/// do that work? revert anything unrelated."
pub fn sweep_auditor(input: SweepAuditorPrompt<'_>) -> String {
    let SweepAuditorPrompt {
        plan: _,
        deferred,
        after,
        diff,
        resolved,
        remaining,
        stale_items,
        small_fix_line_limit,
    } = input;
    let limit = small_fix_line_limit.to_string();
    let resolved_block = render_audit_item_list(resolved);
    let remaining_block = render_audit_item_list(remaining);
    let stale_block = render_stale_items(stale_items);
    render(
        SWEEP_AUDITOR_TEMPLATE,
        &[
            ("after", after.as_str()),
            ("diff", diff),
            ("resolved", &resolved_block),
            ("remaining", &remaining_block),
            ("stale_items", &stale_block),
            ("deferred", &serialize_deferred_for_prompt(deferred)),
            ("small_fix_line_limit", &limit),
        ],
    )
}

/// Bundled inputs for [`sweep_auditor`]. Grouped into a struct so the renderer
/// reads at the call site as named fields rather than a long positional list,
/// and so the `small_fix_line_limit` field can match
/// [`crate::config::AuditConfig::small_fix_line_limit`]'s `u32` without a cast.
#[derive(Debug, Clone, Copy)]
pub struct SweepAuditorPrompt<'a> {
    /// Plan threaded through for parity with the phase auditor; today the
    /// renderer does not consume it directly, but a future template revision
    /// might want plan-level context (overall goal, prior-phase summaries).
    pub plan: &'a Plan,
    pub deferred: &'a DeferredDoc,
    pub after: &'a PhaseId,
    pub diff: &'a str,
    pub resolved: &'a [String],
    pub remaining: &'a [String],
    pub stale_items: &'a [StaleItem],
    pub small_fix_line_limit: u32,
}

/// Render an item-list block for the sweep auditor prompt. Empty slice maps to
/// a visible `(none)` marker so the agent isn't tricked by a blank section.
fn render_audit_item_list(items: &[String]) -> String {
    if items.is_empty() {
        return "(none)\n".to_string();
    }
    let mut out = String::new();
    for text in items {
        out.push_str("- ");
        out.push_str(text);
        out.push('\n');
    }
    out
}

/// Render the fixer prompt for the deferred-sweep pipeline. Used when a sweep
/// dispatch fails its post-test step: there is no "current phase" to anchor on
/// because the implementer was working through `deferred.md` rather than a
/// plan phase, so the prompt frames the failure around the deferred items the
/// implementer touched and asks the fixer to keep the patch scoped to that
/// sweep.
pub fn fixer_for_sweep(_plan: &Plan, deferred: &DeferredDoc, test_output: &str) -> String {
    render(
        SWEEP_FIXER_TEMPLATE,
        &[
            ("test_output", test_output),
            ("deferred", &serialize_deferred_for_prompt(deferred)),
        ],
    )
}

/// One stale `## Deferred items` entry. The runner's staleness tracker (phase
/// 05) populates this with items that have survived multiple sweep dispatches
/// without flipping to `- [x]`; phase 02 ships the type so the prompt
/// renderer's signature is final today and the template can include a
/// "Stale items" section that renders cleanly with an empty slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleItem {
    /// The item text (the bytes that follow `- [ ]` on disk). Used to match
    /// the entry by exact text — sweep agents are forbidden from rewording
    /// items precisely so this stays a stable identifier.
    pub text: String,
    /// Number of consecutive sweep dispatches in which this item appeared
    /// without being checked off.
    pub attempts: u32,
}

/// Render the sweep prompt. The output instructs the agent to drain pending
/// `## Deferred items` entries, leaving `## Deferred phases`, `plan.md`, and
/// `.pitboss/` untouched.
///
/// `after_phase = Some(id)` flags this as the inter-phase sweep that fired
/// after `id` completed; `None` covers the standalone-sweep entry point
/// (`pitboss sweep` with no run in flight, phase 06). `stale_items` is the
/// staleness tracker's snapshot — empty until phase 05 wires it in, which the
/// template tolerates by rendering "(none)".
pub fn sweep(
    _plan: &Plan,
    deferred: &DeferredDoc,
    after_phase: Option<&PhaseId>,
    stale_items: &[StaleItem],
) -> String {
    let pending = render_pending_items(deferred);
    let stale = render_stale_items(stale_items);
    // Context and stale-items substitutions are inserted between H2 sections
    // separated by blank lines in the template, so they must not carry a
    // trailing newline of their own — that would expand `\n\n` into `\n\n\n`
    // and produce a double-blank visual seam.
    let context = match after_phase {
        Some(id) => format!(
            "- Most recently completed phase: {id}\n\
             - `plan.md` is on disk if you need to peek for context, but you must not modify it.",
        ),
        None => "- Standalone sweep — no preceding phase to anchor on. Treat the deferred list as the whole job.\n\
             - `plan.md` is on disk if you need to peek for context, but you must not modify it."
            .to_string(),
    };
    render(
        SWEEP_TEMPLATE,
        &[
            ("deferred_items", &pending),
            ("stale_items", &stale),
            ("context", &context),
        ],
    )
}

/// Build the `{deferred_items}` substitution: only `- [ ]` lines from
/// `## Deferred items`. Already-checked items are stripped so the agent's
/// view is exactly the work still pending.
fn render_pending_items(doc: &DeferredDoc) -> String {
    let mut out = String::new();
    for item in &doc.items {
        if item.done {
            continue;
        }
        out.push_str("- [ ] ");
        out.push_str(&item.text);
        out.push('\n');
    }
    if out.is_empty() {
        return "(no pending items)\n".to_string();
    }
    out
}

/// Build the `{stale_items}` substitution. Empty slice → `(none)` so the
/// section renders cleanly today (phase 05 supplies real values). The result
/// has no trailing newline; the template provides the blank line that
/// separates this section from the next.
fn render_stale_items(items: &[StaleItem]) -> String {
    if items.is_empty() {
        return "(none)".to_string();
    }
    let mut lines: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        // The text comes straight from `deferred.md`; wrapping it in backticks
        // keeps any markdown characters in the original from re-rendering.
        lines.push(format!(
            "- `{}` — {} sweep attempts without resolution",
            item.text, item.attempts
        ));
    }
    lines.join("\n")
}

fn serialize_deferred_for_prompt(doc: &DeferredDoc) -> String {
    let s = deferred::serialize(doc);
    if s.is_empty() {
        "(empty)\n".to_string()
    } else {
        s
    }
}

/// Substitute `{name}` placeholders in `template` with the matching values
/// from `vars`. `{{` and `}}` produce literal `{` and `}` in the output.
///
/// Unknown placeholder names, unmatched `{`, and unmatched `}` all panic — the
/// invariants belong to the developer authoring the template, not to runtime
/// data, so failing fast in tests is the right behavior.
fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len() + 256);
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'{' && bytes.get(i + 1) == Some(&b'{') {
            out.push('{');
            i += 2;
        } else if c == b'}' && bytes.get(i + 1) == Some(&b'}') {
            out.push('}');
            i += 2;
        } else if c == b'{' {
            let rel = template[i + 1..].find('}').unwrap_or_else(|| {
                panic!("unterminated placeholder in prompt template at byte {i}")
            });
            let name = &template[i + 1..i + 1 + rel];
            let val = vars
                .iter()
                .find(|(k, _)| *k == name)
                .unwrap_or_else(|| panic!("unknown placeholder {{{name}}} in prompt template"))
                .1;
            out.push_str(val);
            i = i + 1 + rel + 1;
        } else if c == b'}' {
            panic!("unmatched }} at byte {i} in prompt template");
        } else {
            // Fast-forward past plain text. `{` and `}` are ASCII so byte
            // indexing is char-safe; the search returns the next interesting
            // byte position relative to the slice.
            let next = template[i..]
                .find(['{', '}'])
                .map(|d| i + d)
                .unwrap_or(template.len());
            out.push_str(&template[i..next]);
            i = next;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deferred::{DeferredItem, DeferredPhase};
    use crate::plan::PhaseId;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    fn fixture_plan() -> Plan {
        Plan::new(
            pid("02"),
            vec![
                Phase {
                    id: pid("01"),
                    title: "Foundation".into(),
                    body: "\n**Scope.** stand it up.\n\n**Deliverables.**\n- crate\n\n**Acceptance.**\n- builds\n\n".into(),
                },
                Phase {
                    id: pid("02"),
                    title: "Domain types".into(),
                    body: "\n**Scope.** type vocabulary.\n\n**Deliverables.**\n- PhaseId\n\n**Acceptance.**\n- ordering tests\n".into(),
                },
            ],
        )
    }

    fn fixture_current() -> Phase {
        Phase {
            id: pid("02"),
            title: "Domain types".into(),
            body: "\n**Scope.** type vocabulary.\n\n**Deliverables.**\n- PhaseId\n\n**Acceptance.**\n- ordering tests\n".into(),
        }
    }

    fn fixture_deferred() -> DeferredDoc {
        DeferredDoc {
            items: vec![
                DeferredItem {
                    text: "polish error message".into(),
                    done: false,
                },
                DeferredItem {
                    text: "remove unused stub".into(),
                    done: true,
                },
            ],
            phases: vec![DeferredPhase {
                source_phase: pid("07"),
                title: "rework agent trait".into(),
                body: "\nbody line\n".into(),
            }],
        }
    }

    #[test]
    fn render_substitutes_simple_placeholders() {
        assert_eq!(
            render("hello {name}!", &[("name", "world")]),
            "hello world!"
        );
    }

    #[test]
    fn render_handles_repeated_and_adjacent_placeholders() {
        assert_eq!(render("{a}{b}{a}", &[("a", "X"), ("b", "Y")]), "XYX");
    }

    #[test]
    fn render_double_brace_escapes_literal_braces() {
        assert_eq!(
            render(
                "rust: Result<{T}, {E}> {{ ok }}",
                &[("T", "u32"), ("E", "Err")]
            ),
            "rust: Result<u32, Err> { ok }"
        );
    }

    #[test]
    fn render_preserves_unicode() {
        assert_eq!(render("ünîcødé {x} 漢字", &[("x", "✓")]), "ünîcødé ✓ 漢字");
    }

    #[test]
    #[should_panic(expected = "unknown placeholder")]
    fn render_panics_on_unknown_placeholder() {
        render("{nope}", &[("name", "v")]);
    }

    #[test]
    #[should_panic(expected = "unterminated placeholder")]
    fn render_panics_on_unterminated_placeholder() {
        render("oh no {forever", &[("forever", "x")]);
    }

    #[test]
    #[should_panic(expected = "unmatched")]
    fn render_panics_on_lone_close_brace() {
        render("oh no }", &[]);
    }

    #[test]
    fn implementer_includes_phase_and_deferred() {
        let plan = fixture_plan();
        let current = fixture_current();
        let deferred = fixture_deferred();
        let out = implementer(&plan, &deferred, &current);
        // Phase identity rendered verbatim.
        assert!(out.contains("# Phase 02: Domain types"));
        assert!(out.contains("PhaseId"));
        // Deferred file embedded.
        assert!(out.contains("- [ ] polish error message"));
        assert!(out.contains("### From phase 07: rework agent trait"));
        // Hard rules present.
        assert!(out.contains("Never edit `plan.md`"));
        assert!(out.contains(".pitboss/"));
        // No unsubstituted placeholders left.
        assert!(!out.contains("{phase_id}"));
        assert!(!out.contains("{deferred}"));
    }

    #[test]
    fn auditor_renders_threshold_and_diff() {
        let plan = fixture_plan();
        let current = fixture_current();
        let diff = "diff --git a/src/x.rs b/src/x.rs\n@@\n+println!(\"hi\");\n";
        let out = auditor(&plan, &current, diff, 30);
        assert!(out.contains("≤ 30 lines"));
        assert!(out.contains("Diff produced by the implementer"));
        assert!(out.contains("println!"));
        assert!(!out.contains("{small_fix_line_limit}"));
    }

    #[test]
    fn fixer_includes_test_output() {
        let plan = fixture_plan();
        let current = fixture_current();
        let test_output = "running 1 test\ntest foo ... FAILED\nassertion failed: 1 == 2\n";
        let out = fixer(&plan, &current, test_output);
        assert!(out.contains("Tests failed after the implementer ran"));
        assert!(out.contains("assertion failed"));
        assert!(!out.contains("{test_output}"));
    }

    #[test]
    fn planner_embeds_goal_and_repo_summary() {
        let out = planner(
            "Build a CLI todo app in Rust",
            "Cargo.toml\nsrc/main.rs\nREADME.md",
        );
        assert!(out.contains("Build a CLI todo app in Rust"));
        assert!(out.contains("Cargo.toml"));
        assert!(out.contains("YAML frontmatter"));
        assert!(out.contains("Output ONLY the file contents."));
        assert!(!out.contains("{goal}"));
    }

    #[test]
    fn empty_deferred_renders_as_visible_marker() {
        let plan = fixture_plan();
        let current = fixture_current();
        let out = implementer(&plan, &DeferredDoc::empty(), &current);
        assert!(
            out.contains("(empty)"),
            "expected an explicit empty marker so the agent isn't confused by a blank section"
        );
    }

    #[test]
    fn auditor_with_deferred_threads_real_doc() {
        let plan = fixture_plan();
        let current = fixture_current();
        let deferred = fixture_deferred();
        let out = auditor_with_deferred(&plan, &current, "no diff", &deferred, 25);
        assert!(out.contains("- [ ] polish error message"));
        assert!(out.contains("≤ 25 lines"));
    }

    #[test]
    fn fixer_with_deferred_threads_real_doc() {
        let plan = fixture_plan();
        let current = fixture_current();
        let deferred = fixture_deferred();
        let out = fixer_with_deferred(&plan, &current, "boom", &deferred);
        assert!(out.contains("- [ ] polish error message"));
        assert!(out.contains("boom"));
    }

    #[test]
    fn templates_fit_static_budget() {
        for (name, body) in [
            ("implementer", IMPLEMENTER_TEMPLATE),
            ("auditor", AUDITOR_TEMPLATE),
            ("fixer", FIXER_TEMPLATE),
            ("sweep", SWEEP_TEMPLATE),
            ("sweep_fixer", SWEEP_FIXER_TEMPLATE),
            ("sweep_auditor", SWEEP_AUDITOR_TEMPLATE),
            ("planner", PLANNER_TEMPLATE),
            ("questioner", QUESTIONER_TEMPLATE),
        ] {
            assert!(
                body.len() <= TEMPLATE_STATIC_BUDGET,
                "{name} template is {} bytes, exceeding TEMPLATE_STATIC_BUDGET ({} bytes)",
                body.len(),
                TEMPLATE_STATIC_BUDGET
            );
        }
    }

    #[test]
    fn snapshot_implementer() {
        let plan = fixture_plan();
        let current = fixture_current();
        let deferred = fixture_deferred();
        insta::assert_snapshot!(implementer(&plan, &deferred, &current));
    }

    #[test]
    fn snapshot_auditor() {
        let plan = fixture_plan();
        let current = fixture_current();
        let diff = "diff --git a/src/x.rs b/src/x.rs\n@@\n-old\n+new\n";
        insta::assert_snapshot!(auditor(&plan, &current, diff, 30));
    }

    #[test]
    fn snapshot_fixer() {
        let plan = fixture_plan();
        let current = fixture_current();
        let test_output = "running 2 tests\ntest a ... ok\ntest b ... FAILED\n";
        insta::assert_snapshot!(fixer(&plan, &current, test_output));
    }

    #[test]
    fn snapshot_planner() {
        let goal = "Build a CLI todo app in Rust with JSON persistence.";
        let repo_summary = "(empty repository)";
        insta::assert_snapshot!(planner(goal, repo_summary));
    }

    #[test]
    fn questioner_embeds_goal_and_max() {
        let out = questioner("Build a CLI todo app", "(empty repository)", 20);
        assert!(out.contains("Build a CLI todo app"));
        assert!(out.contains("20"));
        assert!(out.contains("numbered list"));
        assert!(!out.contains("{goal}"));
        assert!(!out.contains("{max_questions}"));
    }

    #[test]
    fn snapshot_questioner() {
        let goal = "Add a --interview flag to pitboss plan.";
        let repo_summary = "(empty repository)";
        insta::assert_snapshot!(questioner(goal, repo_summary, 25));
    }
}

/// Tests for the deferred-sweep prompt and its `sweep_fixer` companion. Kept
/// in their own module so they show up under `pitboss::prompts::sweep::…` and
/// can be filtered with `cargo test prompts::sweep`.
#[cfg(test)]
mod sweep {
    use super::*;
    use crate::deferred::{DeferredItem, DeferredPhase};
    use crate::plan::PhaseId;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    fn fixture_plan() -> Plan {
        Plan::new(
            pid("03"),
            vec![
                Phase {
                    id: pid("02"),
                    title: "Sweep prompt scaffolding".into(),
                    body: "\n**Scope.** wire the prompt.\n".into(),
                },
                Phase {
                    id: pid("03"),
                    title: "Inter-phase sweep".into(),
                    body: "\n**Scope.** dispatch the sweep agent.\n".into(),
                },
            ],
        )
    }

    fn fixture_deferred() -> DeferredDoc {
        DeferredDoc {
            items: vec![
                DeferredItem {
                    text: "polish error message in PhaseId::parse".into(),
                    done: false,
                },
                DeferredItem {
                    text: "drop unused stub in deferred::parse".into(),
                    done: false,
                },
                DeferredItem {
                    text: "rename `flag` to `enabled` in audit config".into(),
                    done: false,
                },
                DeferredItem {
                    text: "tighten test for empty deferred.md".into(),
                    done: false,
                },
                DeferredItem {
                    text: "document sweep section in README".into(),
                    done: false,
                },
                DeferredItem {
                    text: "remove already-shipped TODO in runner".into(),
                    done: true,
                },
            ],
            phases: vec![DeferredPhase {
                source_phase: pid("07"),
                title: "rework agent trait".into(),
                body: "\nbody line\n".into(),
            }],
        }
    }

    #[test]
    fn sweep_strips_already_checked_items() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let after = pid("02");
        let out = sweep(&plan, &deferred, Some(&after), &[]);
        assert!(
            !out.contains("remove already-shipped TODO in runner"),
            "checked items must not appear in the agent's view"
        );
        // Five pending items: each one shows up as a `- [ ]` line.
        for text in [
            "polish error message in PhaseId::parse",
            "drop unused stub in deferred::parse",
            "rename `flag` to `enabled` in audit config",
            "tighten test for empty deferred.md",
            "document sweep section in README",
        ] {
            assert!(
                out.contains(&format!("- [ ] {text}")),
                "expected pending item {text:?} in output:\n{out}"
            );
        }
    }

    #[test]
    fn sweep_renders_after_phase_context() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let after = pid("02");
        let out = sweep(&plan, &deferred, Some(&after), &[]);
        assert!(
            out.contains("Most recently completed phase: 02"),
            "expected after_phase context line in output:\n{out}"
        );
        assert!(
            !out.contains("Standalone sweep"),
            "standalone wording must not appear when after_phase is Some:\n{out}"
        );
        // Hard rules mention the H3 / phases prohibition.
        assert!(out.contains("`### From phase X:`"));
        // No unsubstituted placeholders.
        assert!(!out.contains("{deferred_items}"));
        assert!(!out.contains("{stale_items}"));
        assert!(!out.contains("{context}"));
    }

    #[test]
    fn sweep_renders_standalone_when_after_phase_is_none() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let out = sweep(&plan, &deferred, None, &[]);
        assert!(
            out.contains("Standalone sweep"),
            "expected standalone wording when after_phase is None:\n{out}"
        );
        assert!(
            !out.contains("Most recently completed phase"),
            "phase-anchor wording must not appear in the standalone case:\n{out}"
        );
    }

    #[test]
    fn sweep_renders_empty_stale_section_with_marker() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let after = pid("02");
        let out = sweep(&plan, &deferred, Some(&after), &[]);
        // Phase 02 ships the section as a placeholder; an empty slice must
        // render visibly so the agent isn't tricked by a blank section.
        assert!(out.contains("# Stale items"));
        assert!(
            out.contains("(none)"),
            "empty stale slice should render the visible placeholder:\n{out}"
        );
    }

    #[test]
    fn sweep_renders_stale_items_when_provided() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let after = pid("02");
        let stale = vec![
            StaleItem {
                text: "polish error message in PhaseId::parse".into(),
                attempts: 3,
            },
            StaleItem {
                text: "tighten test for empty deferred.md".into(),
                attempts: 2,
            },
        ];
        let out = sweep(&plan, &deferred, Some(&after), &stale);
        assert!(out.contains("polish error message in PhaseId::parse"));
        assert!(
            out.contains("3 sweep attempts"),
            "expected stale-attempt count in output:\n{out}"
        );
        assert!(out.contains("2 sweep attempts"));
        assert!(
            !out.contains("(none)"),
            "stale-empty marker must not appear when stale items were supplied:\n{out}"
        );
    }

    #[test]
    fn sweep_with_no_pending_items_renders_marker() {
        let plan = fixture_plan();
        let deferred = DeferredDoc {
            items: vec![DeferredItem {
                text: "already done".into(),
                done: true,
            }],
            phases: Vec::new(),
        };
        let after = pid("02");
        let out = sweep(&plan, &deferred, Some(&after), &[]);
        assert!(
            out.contains("(no pending items)"),
            "expected pending-empty marker:\n{out}"
        );
    }

    #[test]
    fn sweep_template_static_size_under_budget_for_empty_inputs() {
        // The template is what we ship in source; this guards against a future
        // edit ballooning it past the per-template ceiling. Render with the
        // smallest possible inputs so we measure the static portion.
        let plan = fixture_plan();
        let out = sweep(&plan, &DeferredDoc::empty(), None, &[]);
        assert!(
            out.len() <= TEMPLATE_STATIC_BUDGET,
            "rendered sweep prompt is {} bytes for empty inputs (> budget {})",
            out.len(),
            TEMPLATE_STATIC_BUDGET
        );
    }

    #[test]
    fn snapshot_sweep_after_phase() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let after = pid("02");
        insta::assert_snapshot!(sweep(&plan, &deferred, Some(&after), &[]));
    }

    #[test]
    fn snapshot_sweep_after_phase_with_stale_items() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let after = pid("02");
        let stale = vec![
            StaleItem {
                text: "polish error message in PhaseId::parse".into(),
                attempts: 3,
            },
            StaleItem {
                text: "tighten test for empty deferred.md".into(),
                attempts: 2,
            },
        ];
        insta::assert_snapshot!(sweep(&plan, &deferred, Some(&after), &stale));
    }

    #[test]
    fn snapshot_sweep_standalone() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        insta::assert_snapshot!(sweep(&plan, &deferred, None, &[]));
    }

    #[test]
    fn sweep_auditor_renders_resolved_and_remaining() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let after = pid("02");
        let diff = "diff --git a/src/x.rs b/src/x.rs\n@@\n-old\n+new\n";
        let resolved = vec![
            "polish error message in PhaseId::parse".to_string(),
            "drop unused stub in deferred::parse".to_string(),
        ];
        let remaining = vec![
            "rename `flag` to `enabled` in audit config".to_string(),
            "tighten test for empty deferred.md".to_string(),
            "document sweep section in README".to_string(),
        ];
        let out = sweep_auditor(SweepAuditorPrompt {
            plan: &plan,
            deferred: &deferred,
            after: &after,
            diff,
            resolved: &resolved,
            remaining: &remaining,
            stale_items: &[],
            small_fix_line_limit: 25,
        });
        assert!(out.contains("polish error message in PhaseId::parse"));
        assert!(out.contains("rename `flag` to `enabled` in audit config"));
        assert!(out.contains("Most recently completed phase: 02"));
        assert!(out.contains("≤ 25 lines") || out.contains("25 lines"));
        assert!(out.contains("-old\n+new"));
        assert!(!out.contains("{resolved}"));
        assert!(!out.contains("{remaining}"));
        assert!(!out.contains("{after}"));
        assert!(!out.contains("{diff}"));
        assert!(!out.contains("{stale_items}"));
    }

    #[test]
    fn sweep_auditor_renders_stale_items_when_provided() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let after = pid("02");
        let stale = vec![StaleItem {
            text: "polish error message in PhaseId::parse".into(),
            attempts: 4,
        }];
        let out = sweep_auditor(SweepAuditorPrompt {
            plan: &plan,
            deferred: &deferred,
            after: &after,
            diff: "(empty diff)",
            resolved: &[],
            remaining: &[],
            stale_items: &stale,
            small_fix_line_limit: 25,
        });
        assert!(
            out.contains("Stale items"),
            "expected stale items section header in output:\n{out}"
        );
        assert!(
            out.contains("4 sweep attempts"),
            "expected stale-attempt count in sweep_auditor output:\n{out}"
        );
    }

    #[test]
    fn sweep_auditor_renders_empty_lists_with_marker() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let after = pid("02");
        let out = sweep_auditor(SweepAuditorPrompt {
            plan: &plan,
            deferred: &deferred,
            after: &after,
            diff: "(empty diff)",
            resolved: &[],
            remaining: &[],
            stale_items: &[],
            small_fix_line_limit: 30,
        });
        // Empty resolved/remaining/stale each render with a `(none)` marker.
        assert!(
            out.matches("(none)").count() >= 3,
            "expected (none) markers for resolved, remaining, and stale:\n{out}"
        );
    }

    #[test]
    fn snapshot_sweep_auditor() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let after = pid("02");
        let diff = "diff --git a/src/x.rs b/src/x.rs\n@@\n-old\n+new\n";
        let resolved = vec![
            "polish error message in PhaseId::parse".to_string(),
            "drop unused stub in deferred::parse".to_string(),
        ];
        let remaining = vec![
            "rename `flag` to `enabled` in audit config".to_string(),
            "tighten test for empty deferred.md".to_string(),
            "document sweep section in README".to_string(),
        ];
        insta::assert_snapshot!(sweep_auditor(SweepAuditorPrompt {
            plan: &plan,
            deferred: &deferred,
            after: &after,
            diff,
            resolved: &resolved,
            remaining: &remaining,
            stale_items: &[],
            small_fix_line_limit: 30,
        }));
    }

    #[test]
    fn snapshot_sweep_fixer() {
        let plan = fixture_plan();
        let deferred = fixture_deferred();
        let test_output = "running 4 tests\n\
            test sweep_strips_already_checked_items ... ok\n\
            test sweep_renders_after_phase_context ... ok\n\
            test sweep_renders_empty_stale_section_with_marker ... FAILED\n\
            test sweep_renders_standalone_when_after_phase_is_none ... ok\n\n\
            failures:\n\n\
            ---- sweep_renders_empty_stale_section_with_marker stdout ----\n\
            assertion failed: out.contains(\"(none)\")\n";
        insta::assert_snapshot!(fixer_for_sweep(&plan, &deferred, test_output));
    }
}
