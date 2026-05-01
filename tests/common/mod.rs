//! Shared helpers for the integration test suite.
//!
//! Integration tests cannot share a `tests/lib.rs`; the conventional dodge is
//! a `tests/common/mod.rs` module that each test file pulls in via
//! `mod common;`. Helpers here should be small and focused — anything bigger
//! belongs in `pitboss::tests` (already-shared in-crate harnesses).

#![allow(dead_code)]

use pitboss::config::Config;

/// Disable phase 08's trailing final-sweep drain on `c`.
///
/// Many existing tests assert exact between-phase sweep counts or
/// `deferred_item_attempts` values that predate phase 08's drain; without the
/// flag the drain dispatches an extra sweep after the last phase and trips
/// those assertions. Drain coverage lives in `tests/sweep_final_loop.rs`, so
/// the rest of the suite opts out via this helper.
pub fn disable_final_sweep(c: &mut Config) {
    c.sweep.final_sweep_enabled = false;
}
