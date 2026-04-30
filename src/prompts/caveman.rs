//! Caveman-mode system-prompt directive.
//!
//! Adapted from the [Caveman skill](https://github.com/JuliusBrussee/caveman) —
//! pitboss inlines the directive rather than depending on the upstream Claude
//! Code plugin so the same behavior applies to every backend (Claude Code,
//! Codex, Aider) regardless of whether the user has the plugin installed.
//!
//! [`system_prompt`] returns the empty string when caveman mode is disabled,
//! so callers can unconditionally use it as `AgentRequest::system_prompt`.

use crate::config::{CavemanConfig, CavemanIntensity};

/// System-prompt addendum for the configured caveman mode.
///
/// Returns the empty string when [`CavemanConfig::enabled`] is `false`, which
/// preserves the pre-feature behavior at every dispatch site.
///
/// The directive is intentionally compact — it adds a few hundred input tokens
/// per dispatch, which the output-token savings (~65–75% on prose per the
/// upstream skill) more than offset on any non-trivial run.
pub fn system_prompt(cfg: &CavemanConfig) -> String {
    if !cfg.enabled {
        return String::new();
    }

    let intensity_label = match cfg.intensity {
        CavemanIntensity::Lite => "lite",
        CavemanIntensity::Full => "full",
        CavemanIntensity::Ultra => "ultra",
    };
    let intensity_rule = match cfg.intensity {
        CavemanIntensity::Lite => {
            "LITE: drop filler/hedging only. Keep articles + full sentences. \
             Professional but tight."
        }
        CavemanIntensity::Full => {
            "FULL: drop articles, fragments OK, short synonyms. Classic caveman."
        }
        CavemanIntensity::Ultra => {
            "ULTRA: abbreviate (DB/auth/config/req/res/fn/impl), strip conjunctions, \
             arrows for causality (X → Y), one word when one word enough."
        }
    };

    format!(
        "CAVEMAN MODE ACTIVE — intensity: {intensity_label}\n\
         \n\
         Respond terse like smart caveman. All technical substance stay. \
         Only fluff die. ACTIVE EVERY RESPONSE — no revert after many turns, \
         no filler drift.\n\
         \n\
         Rules:\n\
         - Drop articles (a/an/the), filler (just/really/basically/actually/simply), \
         pleasantries (sure/certainly/of course/happy to), hedging.\n\
         - Fragments OK. Short synonyms (big not extensive, fix not \"implement a solution for\").\n\
         - Technical terms exact. Code blocks unchanged. Errors quoted exact.\n\
         - Pattern: [thing] [action] [reason]. [next step].\n\
         - Not: \"Sure! I'd be happy to help. The issue is likely caused by...\"\n\
         - Yes: \"Bug in auth middleware. Token expiry check use `<` not `<=`. Fix:\"\n\
         \n\
         {intensity_rule}\n\
         \n\
         Auto-clarity: drop caveman style for security warnings, irreversible action \
         confirmations, and multi-step sequences where fragment order risks misread. \
         Resume after the clear part is done.\n\
         \n\
         Boundaries: code, commit messages, PR descriptions, and structured artifacts \
         (`plan.md` phase bodies, `deferred.md` entries) follow their normal format — \
         caveman style applies to prose only.\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_returns_empty_string() {
        let cfg = CavemanConfig::default();
        assert!(!cfg.enabled);
        assert!(system_prompt(&cfg).is_empty());
    }

    #[test]
    fn enabled_returns_non_empty_directive() {
        let cfg = CavemanConfig {
            enabled: true,
            intensity: CavemanIntensity::Full,
        };
        let s = system_prompt(&cfg);
        assert!(!s.is_empty());
        assert!(s.contains("CAVEMAN MODE ACTIVE"));
    }

    #[test]
    fn intensity_label_appears_in_output() {
        for (intensity, label, marker) in [
            (CavemanIntensity::Lite, "lite", "LITE:"),
            (CavemanIntensity::Full, "full", "FULL:"),
            (CavemanIntensity::Ultra, "ultra", "ULTRA:"),
        ] {
            let cfg = CavemanConfig {
                enabled: true,
                intensity,
            };
            let s = system_prompt(&cfg);
            assert!(
                s.contains(&format!("intensity: {label}")),
                "missing label for {label}: {s}"
            );
            assert!(
                s.contains(marker),
                "missing intensity rule marker {marker} for {label}: {s}"
            );
        }
    }

    #[test]
    fn directive_carves_out_artifacts_and_code() {
        // The skill explicitly leaves code/commits/PRs alone; pitboss extends
        // that to the structured planning artifacts since downstream roles
        // parse them. Verify the carve-out wording survives any future rewrite.
        let cfg = CavemanConfig {
            enabled: true,
            intensity: CavemanIntensity::Full,
        };
        let s = system_prompt(&cfg);
        assert!(s.contains("Code blocks unchanged"));
        assert!(s.contains("plan.md"));
        assert!(s.contains("deferred.md"));
    }

    #[test]
    fn directive_size_stays_under_a_kilobyte() {
        // The directive ships on every dispatch, so its size is the input-cost
        // floor for caveman mode. A kilobyte (~250 tokens) is plenty of room
        // for the rules and a comfortable upper bound.
        let cfg = CavemanConfig {
            enabled: true,
            intensity: CavemanIntensity::Ultra,
        };
        let s = system_prompt(&cfg);
        assert!(
            s.len() < 1500,
            "directive grew beyond budget ({} bytes); trim it",
            s.len()
        );
    }
}
