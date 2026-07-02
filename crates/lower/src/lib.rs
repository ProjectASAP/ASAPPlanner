//! `asap-lower` — thin facade over the two query front ends.
//!
//! Re-exports both language paths so a caller can depend on a single crate for
//! PromQL *and* SQL. Both front ends end at the canonical intent algebra via the
//! same shared L2→L3 converter in [`asap_ir`].
//!
//! ## Dependency isolation
//!
//! Depending on this facade pulls **both** parsers (`promql-parser` and
//! DataFusion). A caller that needs only one language should depend on the
//! matching front-end crate directly — [`asap_frontend_promql`] (PromQL parser
//! only) or [`asap_frontend_sql`] (DataFusion only) — so it never compiles the
//! other's parser.

pub use asap_frontend_promql::{lower_promql, lower_promql_batch, PromqlError, PromqlLowerer};
pub use asap_frontend_sql::{lower_sql, lower_sql_batch, SqlCatalog, SqlError, SqlLowerer};
