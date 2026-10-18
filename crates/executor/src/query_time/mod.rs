//! Code meant to run at **query execution time**, not planning time: pure,
//! sketch-object-agnostic posterior error-bound math (given a sketch's real
//! counter values, compute a tighter posterior error bound) that only makes
//! sense to invoke *after* a real sketch has ingested data and is being
//! evaluated.
//!
//! It lives in the executor, not in `asap-types`, because `asap-types`
//! holds no execution logic (issue #190): everything there is planning-time
//! IR, describing *what* summary will be built, not *computing over* one
//! that has run. Nothing in the planner or the executor calls into this
//! module today; it is a library waiting for a sketch evaluation path that
//! uses it (see [`error_estimation`]'s own docs for exactly what's blocked
//! and why). See issue #250 for the still-unimplemented idea of recording
//! this module's query-time observations over time and feeding them into a
//! future replan.

pub mod error_estimation;

// Glob, not a named list: this module exists only to hold
// `error_estimation` today, so re-exporting everything it makes public
// keeps this hop in sync automatically.
pub use error_estimation::*;
