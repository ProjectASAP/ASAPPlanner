//! Sketch-vs-exact boundary (stub).
//!
//! The per-node accuracy decision — whether an approximate intent
//! (`Quantile`/`Cardinality`/`Count`/`TopK`) is realised by an exact operator
//! or a sketch, and with which parameters. This is an L4 concern: L3 carries
//! only the intent + accuracy target, never the realization.
//!
//! TODO(#34, cross-cutting): confirm the boundary decision fires per node over
//! nested trees once this layer is fleshed out.
