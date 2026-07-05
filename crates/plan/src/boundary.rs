//! Sketch-vs-exact boundary (stub).
//!
//! The per-node accuracy decision — whether an approximate intent
//! (`Quantile`/`Cardinality`/`Count`/`TopK`) is realised by an exact operator
//! or a sketch, and with which parameters. This is an L4 concern: L3 carries
//! only the intent + accuracy target, never the realization.
//!
//! TODO(#98): implement as part of the L3→L4 binding — the decision consumes
//! the `AccuracyTarget` threaded onto approximate intents, the
//! `agg_is_exact` / `agg_is_mergeable` helpers in `asap-ir`, and histogram
//! sketchability (#79: classic `le`-buckets are not re-sketchable; the generic
//! `Quantile` path is). Must fire per node over nested trees.
