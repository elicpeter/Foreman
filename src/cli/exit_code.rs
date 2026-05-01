//! Process exit codes for every `pitboss` subcommand.
//!
//! The numeric values are part of the supported CLI surface — scripts wrapping
//! pitboss read them. Variants were originally named for the `pitboss grind`
//! exit-code table because grind was the first subcommand to surface a non-zero
//! exit; later subcommands (`pitboss sweep`, future others) reuse the same
//! enum so that exit-code semantics stay coherent across the binary.
//!
//! Variant naming therefore mixes generic codes (`Success`, `Aborted`,
//! `FailedToStart`) with grind-flavored ones (`MixedFailures`,
//! `BudgetExhausted`, `ConsecutiveFailures`, `PrCreationFailed`). Non-grind
//! callers map their failure mode onto the closest semantic match — for
//! example `pitboss sweep` returns `MixedFailures` on a halt because that slot
//! is the canonical "exit 1 / something failed" code.

/// Documented process exit codes for the `pitboss` binary. Mapped to a
/// [`std::process::ExitCode`] via [`ExitCode::into_process`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// Every dispatched session resolved as `SessionStatus::Ok` (or the run
    /// produced zero sessions because the scheduler returned `None` from the
    /// start). `0`.
    Success = 0,
    /// At least one session resolved as `Error`, `Timeout`, or `Dirty` but the
    /// run otherwise completed. Also used by non-grind subcommands as the
    /// generic "exit 1 / operation failed" code (e.g., `pitboss sweep` on a
    /// halt). `1`.
    MixedFailures = 1,
    /// The user aborted the run (typically a second Ctrl-C). `2`.
    Aborted = 2,
    /// A budget tripped (max iterations, until, max tokens, or max cost).
    /// `3`.
    BudgetExhausted = 3,
    /// The runner could not start (missing config, dirty tree on start,
    /// branch creation failed, etc.). `4`.
    FailedToStart = 4,
    /// The consecutive-failure escape valve fired (see
    /// [`crate::config::GrindConfig::consecutive_failure_limit`]). `5`.
    ConsecutiveFailures = 5,
    /// `--require-pr` was set, the run otherwise succeeded, but the post-run
    /// `gh pr create` call failed. Lets a CI script tell "the work shipped but
    /// the PR open failed" apart from a fully clean run. `6`.
    PrCreationFailed = 6,
}

impl ExitCode {
    /// The raw numeric exit code.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Translate into a [`std::process::ExitCode`] for `main`. Implemented
    /// here rather than via `From` so the conversion site is grep-able.
    pub fn into_process(self) -> std::process::ExitCode {
        std::process::ExitCode::from(self.as_u8())
    }
}

impl std::fmt::Display for ExitCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExitCode::Success => f.write_str("success"),
            ExitCode::MixedFailures => f.write_str("mixed-failures"),
            ExitCode::Aborted => f.write_str("aborted"),
            ExitCode::BudgetExhausted => f.write_str("budget-exhausted"),
            ExitCode::FailedToStart => f.write_str("failed-to-start"),
            ExitCode::ConsecutiveFailures => f.write_str("consecutive-failures"),
            ExitCode::PrCreationFailed => f.write_str("pr-creation-failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_numeric_values_match_documented_table() {
        assert_eq!(ExitCode::Success.as_u8(), 0);
        assert_eq!(ExitCode::MixedFailures.as_u8(), 1);
        assert_eq!(ExitCode::Aborted.as_u8(), 2);
        assert_eq!(ExitCode::BudgetExhausted.as_u8(), 3);
        assert_eq!(ExitCode::FailedToStart.as_u8(), 4);
        assert_eq!(ExitCode::ConsecutiveFailures.as_u8(), 5);
        assert_eq!(ExitCode::PrCreationFailed.as_u8(), 6);
    }
}
