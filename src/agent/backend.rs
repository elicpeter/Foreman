//! Backend selection — which underlying agent CLI pitboss should drive.
//!
//! Phase 19 lays the groundwork for plugging in additional coding-agent CLIs
//! (Codex, Aider, Gemini) without expanding the [`super::Agent`] trait surface
//! or touching the runner. The enum is the in-memory form of the
//! `agent.backend` key in `pitboss.toml`; today only [`BackendKind::ClaudeCode`]
//! is wired through [`super::build_agent`], the others return a clear
//! "not yet implemented" error so the dispatch path is in place for the
//! follow-on phases that ship the real adapters.
//!
//! Parsing is case-insensitive and accepts the canonical underscored form as
//! well as a hyphen-and-no-separator variant of `claude_code`, since users
//! routinely type one and read the other.

use std::fmt;
use std::str::FromStr;

use anyhow::anyhow;

/// Which underlying agent backend pitboss should drive.
///
/// Round-trips through [`fmt::Display`] / [`FromStr`] as the canonical lowercase
/// underscored string used in `pitboss.toml` (`claude_code`, `codex`, `aider`,
/// `gemini`). [`Self::default`] is [`Self::ClaudeCode`], so a workspace with
/// no `[agent]` section keeps today's behavior.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// Anthropic's `claude` CLI (the only backend currently wired).
    #[default]
    ClaudeCode,
    /// OpenAI's `codex` CLI — not yet implemented.
    Codex,
    /// Aider — not yet implemented.
    Aider,
    /// Google's `gemini` CLI — not yet implemented.
    Gemini,
}

impl BackendKind {
    /// Canonical lowercase string form, matching `pitboss.toml`'s
    /// `agent.backend` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::Aider => "aider",
            Self::Gemini => "gemini",
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BackendKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "claude_code" | "claude-code" | "claudecode" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            "aider" => Ok(Self::Aider),
            "gemini" => Ok(Self::Gemini),
            _ => Err(anyhow!(
                "unknown agent backend {s:?}; expected one of \
                 'claude_code', 'codex', 'aider', 'gemini'"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_claude_code() {
        assert_eq!(BackendKind::default(), BackendKind::ClaudeCode);
    }

    #[test]
    fn display_uses_canonical_underscored_form() {
        assert_eq!(BackendKind::ClaudeCode.to_string(), "claude_code");
        assert_eq!(BackendKind::Codex.to_string(), "codex");
        assert_eq!(BackendKind::Aider.to_string(), "aider");
        assert_eq!(BackendKind::Gemini.to_string(), "gemini");
    }

    #[test]
    fn from_str_accepts_canonical_lowercase() {
        assert_eq!(
            "claude_code".parse::<BackendKind>().unwrap(),
            BackendKind::ClaudeCode
        );
        assert_eq!("codex".parse::<BackendKind>().unwrap(), BackendKind::Codex);
        assert_eq!("aider".parse::<BackendKind>().unwrap(), BackendKind::Aider);
        assert_eq!(
            "gemini".parse::<BackendKind>().unwrap(),
            BackendKind::Gemini
        );
    }

    #[test]
    fn from_str_is_case_insensitive() {
        assert_eq!(
            "Claude_Code".parse::<BackendKind>().unwrap(),
            BackendKind::ClaudeCode
        );
        assert_eq!("CODEX".parse::<BackendKind>().unwrap(), BackendKind::Codex);
        assert_eq!("Aider".parse::<BackendKind>().unwrap(), BackendKind::Aider);
        assert_eq!(
            "GEMINI".parse::<BackendKind>().unwrap(),
            BackendKind::Gemini
        );
    }

    #[test]
    fn from_str_accepts_hyphen_variant_for_claude_code() {
        // `claude-code` and `claudecode` are common typed-from-memory variants
        // of the canonical `claude_code` and parse identically.
        assert_eq!(
            "claude-code".parse::<BackendKind>().unwrap(),
            BackendKind::ClaudeCode
        );
        assert_eq!(
            "claudecode".parse::<BackendKind>().unwrap(),
            BackendKind::ClaudeCode
        );
    }

    #[test]
    fn from_str_trims_whitespace() {
        assert_eq!(
            "  codex  ".parse::<BackendKind>().unwrap(),
            BackendKind::Codex
        );
    }

    #[test]
    fn from_str_rejects_unknown_backend_with_helpful_error() {
        let err = "ollama".parse::<BackendKind>().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ollama"),
            "expected the bad input echoed back, got: {msg}"
        );
        assert!(
            msg.contains("claude_code") && msg.contains("codex"),
            "expected the valid set listed, got: {msg}"
        );
    }

    #[test]
    fn from_str_rejects_empty_string() {
        assert!("".parse::<BackendKind>().is_err());
        assert!("   ".parse::<BackendKind>().is_err());
    }

    #[test]
    fn display_round_trips_through_from_str() {
        for kind in [
            BackendKind::ClaudeCode,
            BackendKind::Codex,
            BackendKind::Aider,
            BackendKind::Gemini,
        ] {
            let parsed: BackendKind = kind.to_string().parse().unwrap();
            assert_eq!(parsed, kind);
        }
    }
}
