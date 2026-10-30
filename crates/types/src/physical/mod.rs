//! #509 Stage 2 decision data: the materialization of each sub-DAG.
//!
//! - [`materialization`] — whether a sub-DAG's output is stored and when it
//!   is computed ([`Materialization`], [`Medium`]).
pub mod materialization;

pub use materialization::{Materialization, Medium};
