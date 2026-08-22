//! SQL aggregate-function name catalog: independent, hand-maintained data
//! mapping a resolved function name to (arity, canonical semantic) --
//! deliberately pulled out of `asap-frontend-sql::sql::lower_agg_intent`
//! (issue #225) so the mapping is its own small, independently-testable
//! artifact rather than buried in a hand-written `match`.
//!
//! Design lifted from [`tobilg/polyglot`](https://github.com/tobilg/polyglot)'s
//! `polyglot-sql-function-catalogs` crate: per-dialect function data
//! (existence, arity/overloads), deliberately *not* depending on whatever
//! consumes it (there, the SQL-parsing crate; here, `asap-frontend-sql` and
//! DataFusion) -- exposed to the consumer instead. Scope is equally
//! deliberately shallow, matching that project's own choice: no argument or
//! return *type* modeling, just existence + arity + which canonical
//! semantic a call maps to. `AggIntent` construction itself doesn't need
//! more than that either -- it already rejects a non-column argument
//! outright (`reducer_col` in `asap-frontend-sql`) rather than trying to
//! typecheck it.
//!
//! Two tables, matching the two problems this replaces:
//!
//! - [`NATIVE_FUNCTIONS`] -- names DataFusion's own planner already resolves
//!   (`sum`, `avg`, `approx_percentile_cont`, ...). [`lookup_native`] maps
//!   one to the [`AggSemantic`] `lower_agg_intent` builds an `AggIntent`
//!   from. The DISTINCT-modifier rule ("`COUNT DISTINCT` alone maps, to
//!   `Cardinality`; reject DISTINCT elsewhere") and the "reducer argument
//!   must be a bare column" rule are call-site logic, not per-function data,
//!   and stay in `asap-frontend-sql`.
//! - [`CLICKHOUSE_BUILTINS`] -- ClickHouse-only names DataFusion doesn't
//!   know at all (`uniqExact`, `countIf`). Each entry additionally carries a
//!   [`RewriteKind`]: the native DataFusion aggregate shape the call
//!   rewrites to before `lower_agg_intent` (or DataFusion's own physical
//!   planner) ever has to understand the ClickHouse name itself. This is
//!   what generalizes `uniqExact`'s old bespoke `UniqExactRewrite` +
//!   `uniq_exact_udaf` pair (issue #221): a new builtin that rewrites to an
//!   already-handled shape is a new entry in this table, not a new
//!   `FunctionRewrite` impl and a new stub-`AggregateUDF` constructor.
//!
//! Generating these tables from a live introspectable source -- ClickHouse's
//! `system.functions`, DataFusion's own in-process UDF/UDAF registry -- the
//! way `polyglot`'s `tools/*/extract_functions.py` do, is a deliberately
//! deferred follow-up (issue #225, item 3: it needs a decision on where such
//! extraction tooling would actually run, e.g. whether a live ClickHouse
//! instance in CI/dev is a given). Until then these are ordinary hand-edited
//! Rust consts.

/// How many arguments a catalog entry's function accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// Exactly `n` arguments.
    Exact(usize),
    /// Between `min` and `max` arguments, inclusive.
    Range { min: usize, max: usize },
}

/// The canonical semantic a native function name maps to -- the
/// classification [`lower_agg_intent`](../asap_frontend_sql/index.html)
/// switches on to build the real `AggIntent`.
///
/// Deliberately *not* `asap_types::pre_asap::agg_intent::AggIntent` itself:
/// most `AggIntent` variants carry call-site-only state that isn't a
/// function of the name alone -- the ambient `AccuracyTarget` (thread-local,
/// not catalog data), φ pulled from a call's literal 2nd argument, whether
/// `DISTINCT` was written. This enum is only the per-name discriminant those
/// call sites key off; building the actual `AggIntent` is
/// `asap-frontend-sql`'s job.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AggSemantic {
    /// `COUNT(*)` / `COUNT(x)` -- ignores its argument (always a row count).
    /// `COUNT(DISTINCT x)` is the one exception the call site special-cases
    /// into `Cardinality` instead; that combination is not its own catalog
    /// entry (it is the same name, `count`, with a modifier).
    Count,
    Sum,
    Min,
    Max,
    Avg,
    /// Sample stddev unless `population`.
    StdDev {
        population: bool,
    },
    /// Sample variance unless `population`.
    Variance {
        population: bool,
    },
    /// φ-quantile. `fixed_q = Some(0.5)` for a name that always means the
    /// median (`median`, `approx_median`); `None` for a name whose φ is a
    /// literal argument the call site must read (`approx_percentile_cont`,
    /// `percentile_cont`).
    Quantile {
        fixed_q: Option<f64>,
    },
    /// Approximate/exact distinct-value count (`approx_distinct`, and
    /// `COUNT DISTINCT` via the call-site modifier above).
    Cardinality,
}

/// One [`NATIVE_FUNCTIONS`] entry.
#[derive(Debug, Clone, Copy)]
pub struct NativeFunction {
    /// Lowercase function name, as DataFusion's `Expr::AggregateFunction`
    /// reports it (`agg_fn.func.name()`).
    pub name: &'static str,
    pub arity: Arity,
    pub semantic: AggSemantic,
}

/// Every aggregate function name `SqlDialect::DataFusionSQL` resolves out of
/// the box, mapped to its canonical semantic. A name appears more than once
/// when DataFusion (or this front end) accepts more than one spelling for
/// the same semantic (`avg`/`mean`, `stddev`/`stddev_samp`, ...).
pub const NATIVE_FUNCTIONS: &[NativeFunction] = &[
    NativeFunction {
        name: "count",
        arity: Arity::Range { min: 0, max: 1 },
        semantic: AggSemantic::Count,
    },
    NativeFunction {
        name: "sum",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Sum,
    },
    NativeFunction {
        name: "min",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Min,
    },
    NativeFunction {
        name: "max",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Max,
    },
    NativeFunction {
        name: "avg",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Avg,
    },
    NativeFunction {
        name: "mean",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Avg,
    },
    NativeFunction {
        name: "stddev",
        arity: Arity::Exact(1),
        semantic: AggSemantic::StdDev { population: false },
    },
    NativeFunction {
        name: "stddev_samp",
        arity: Arity::Exact(1),
        semantic: AggSemantic::StdDev { population: false },
    },
    NativeFunction {
        name: "stddev_pop",
        arity: Arity::Exact(1),
        semantic: AggSemantic::StdDev { population: true },
    },
    NativeFunction {
        name: "var",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Variance { population: false },
    },
    NativeFunction {
        name: "variance",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Variance { population: false },
    },
    NativeFunction {
        name: "var_samp",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Variance { population: false },
    },
    NativeFunction {
        name: "var_pop",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Variance { population: true },
    },
    NativeFunction {
        name: "approx_percentile_cont",
        arity: Arity::Exact(2),
        semantic: AggSemantic::Quantile { fixed_q: None },
    },
    NativeFunction {
        name: "percentile_cont",
        arity: Arity::Exact(2),
        semantic: AggSemantic::Quantile { fixed_q: None },
    },
    NativeFunction {
        name: "median",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Quantile { fixed_q: Some(0.5) },
    },
    NativeFunction {
        name: "approx_median",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Quantile { fixed_q: Some(0.5) },
    },
    NativeFunction {
        name: "approx_distinct",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Cardinality,
    },
];

/// The native DataFusion aggregate shape a [`ClickHouseBuiltin`] call
/// rewrites to, before `lower_agg_intent` (or DataFusion's own physical
/// planner) ever has to know the ClickHouse name existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteKind {
    /// `f(args...)` -> `count(args...) DISTINCT` -- ClickHouse's exact
    /// distinct-count family. `lower_agg_intent` already maps
    /// `count` + `DISTINCT` to `AggIntent::Cardinality`, so nothing further
    /// is needed once the call wears DataFusion's own name.
    CountDistinct,
    /// `f(cond)` -> `sum(CASE WHEN cond THEN 1 ELSE 0 END)` -- ClickHouse's
    /// conditional-count family. Not a plain `count(...) FILTER (WHERE
    /// cond)`: `AggIntent::Count` never consults its argument (it always
    /// means "row count"), so a per-call *filtered* count needs a shape
    /// whose value actually depends on `cond` to survive `lower_agg_intent`
    /// unchanged. Summing a 0/1 indicator does, and lands on the existing
    /// `Sum` path -- including the general non-column-argument
    /// materialization `asap-frontend-sql`'s `lower_aggregate` already does
    /// for any reducer over an expression (issue #110) -- so no new
    /// `AggIntent` variant or lowering path is needed either.
    CountIfToSum,
}

/// One [`CLICKHOUSE_BUILTINS`] entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickHouseBuiltin {
    /// Lowercase function name, matching the name a stub `AggregateUDF` is
    /// registered under (DataFusion resolves a SQL call to it
    /// case-insensitively, but reports it back lowercase).
    pub name: &'static str,
    pub arity: Arity,
    pub rewrite: RewriteKind,
}

/// ClickHouse-only builtin aggregate names DataFusion's planner has no
/// native equivalent for at all -- each needs a stub `AggregateUDF`
/// registered (so the planner accepts the call) and a
/// [`RewriteKind`]-driven rewrite (so it becomes something DataFusion
/// natively understands before physical planning). See the module doc and
/// `asap-frontend-sql::sql::ClickHouseBuiltinRewrite`.
pub const CLICKHOUSE_BUILTINS: &[ClickHouseBuiltin] = &[
    ClickHouseBuiltin {
        name: "uniqexact",
        arity: Arity::Exact(1),
        rewrite: RewriteKind::CountDistinct,
    },
    ClickHouseBuiltin {
        name: "countif",
        arity: Arity::Exact(1),
        rewrite: RewriteKind::CountIfToSum,
    },
];

/// Look up a native function name's canonical semantic (case-sensitive --
/// callers normalize case first, as `asap-frontend-sql` already does via
/// `.to_lowercase()`).
pub fn lookup_native(name: &str) -> Option<AggSemantic> {
    NATIVE_FUNCTIONS
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.semantic)
}

/// Look up a ClickHouse-only builtin by name (case-sensitive, see
/// [`lookup_native`]).
pub fn lookup_clickhouse_builtin(name: &str) -> Option<&'static ClickHouseBuiltin> {
    CLICKHOUSE_BUILTINS.iter().find(|b| b.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_lookup_finds_every_listed_name() {
        for f in NATIVE_FUNCTIONS {
            assert_eq!(lookup_native(f.name), Some(f.semantic), "name: {}", f.name);
        }
        assert_eq!(lookup_native("not_a_real_function"), None);
    }

    #[test]
    fn clickhouse_builtin_lookup_finds_every_listed_name() {
        for b in CLICKHOUSE_BUILTINS {
            let found = lookup_clickhouse_builtin(b.name).expect("listed name must be found");
            assert_eq!(found.name, b.name);
            assert_eq!(found.rewrite, b.rewrite);
        }
        assert_eq!(lookup_clickhouse_builtin("not_a_real_function"), None);
    }

    #[test]
    fn native_and_clickhouse_names_are_lowercase() {
        for f in NATIVE_FUNCTIONS {
            assert_eq!(f.name, f.name.to_lowercase(), "not lowercase: {}", f.name);
        }
        for b in CLICKHOUSE_BUILTINS {
            assert_eq!(b.name, b.name.to_lowercase(), "not lowercase: {}", b.name);
        }
    }

    /// The two tables are disjoint -- a ClickHouse-only name has no native
    /// DataFusion equivalent by construction (that's the whole reason it
    /// needs a stub + rewrite), so it should never also appear as a name
    /// DataFusion already resolves.
    #[test]
    fn clickhouse_builtins_do_not_shadow_native_names() {
        for b in CLICKHOUSE_BUILTINS {
            assert!(
                lookup_native(b.name).is_none(),
                "{} listed as both native and ClickHouse-only",
                b.name
            );
        }
    }
}
