//! Code meant to run at **query execution time**, not planning time —
//! folder-separated from `post_asap`'s other modules ([`super::expr`],
//! [`super::schema`], [`super::sketch`]) on purpose.
//!
//! `asap-types`' crate doc states the invariant this workspace otherwise
//! holds to without exception: "no execution logic lives in this
//! workspace (issue #190) — a downstream deployment crate is expected to
//! supply that." Everything else under [`super`] is *planning*-time IR —
//! types the planner (`asap-aware-mapping`) constructs and commits to
//! before a query ever runs, describing *what* summary will be built, not
//! *computing over* one that has run.
//!
//! [`error_estimation`] is the one deliberate exception, and this
//! submodule exists so that exception is visible in the directory listing
//! itself, not just in prose: it holds pure, sketch-object-agnostic *math*
//! (given a sketch's real counter values, compute a tighter posterior
//! error bound) that only makes sense to invoke *after* a real sketch has
//! ingested data and is being read out — a downstream runtime's job, not
//! this crate's. Nothing in `asap-types` or `asap-aware-mapping` calls
//! into this module today; it is a library waiting for a runtime that
//! doesn't exist yet in this workspace (see [`error_estimation`]'s own
//! docs for exactly what's blocked and why).
//!
//! Contrast with `asap-aware-mapping::boundary::posterior_aware_size_params`
//! (issue #239, PR #248): that function is real, wired *planning*-time
//! code — it lives in the planning crate, not here, and does not call
//! into this module. It borrows the same underlying intuition (a
//! non-adversarial workload can use a smaller sketch than the worst case)
//! but as an explicit, caller-supplied planning-time assumption, not a
//! query-time measurement — the two are independent by construction. See
//! issue #250 for the still-unimplemented idea of actually connecting
//! them: recording this module's query-time observations over time and
//! feeding them into a future replan.

pub mod error_estimation;

// Glob, not a named list: this submodule exists only to hold
// `error_estimation` today, so re-exporting everything it makes public
// keeps this hop in sync automatically — a new `pub fn` there needs no
// matching edit here, only in `post_asap::mod`'s own list below (the
// actual curated short-path public surface, kept explicit like `expr`'s
// and `sketch`'s re-exports).
pub use error_estimation::*;
