//! Post-lowering canonicalization (stub).
//!
//! A single normalization pass both front ends run their L3 output through, so
//! semantically-equivalent SQL and PromQL produce **identical** L3 (e.g. the
//! heavy-hitter TopK recognition that is currently duplicated across the two
//! lowerers). Owning it here — above both front ends, over the shared IR — is
//! what lets a future language path inherit the normalization for free.
//!
//! TODO(#34): implement `canonicalize(expr: QueryExpr) -> QueryExpr` and route
//! both `lower_promql` / `lower_sql` outputs through it; add the cross-language
//! equivalence tests that pin the canonical form.
