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
use crate::plan::{Phase, Plan};

const IMPLEMENTER_TEMPLATE: &str = include_str!("templates/implementer.txt");
const AUDITOR_TEMPLATE: &str = include_str!("templates/auditor.txt");
const FIXER_TEMPLATE: &str = include_str!("templates/fixer.txt");
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
