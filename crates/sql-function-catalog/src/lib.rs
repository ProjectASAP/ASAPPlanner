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
//! Three tables, matching the three problems this replaces:
//!
//! - [`NATIVE_FUNCTIONS`] -- aggregate names DataFusion's own planner already
//!   resolves (`sum`, `avg`, `approx_percentile_cont`, ...). [`lookup_native`]
//!   maps one to the [`AggSemantic`] `lower_agg_intent` builds an `AggIntent`
//!   from. The DISTINCT-modifier rule ("`COUNT DISTINCT` alone maps, to
//!   `Cardinality`; reject DISTINCT elsewhere") and the "reducer argument
//!   must be a bare column" rule are call-site logic, not per-function data,
//!   and stay in `asap-frontend-sql`.
//! - [`CLICKHOUSE_BUILTINS`] -- ClickHouse-only *aggregate* names DataFusion
//!   doesn't know at all (`uniqExact`, `countIf`, `argMax`, `argMin`). Each
//!   entry additionally carries a [`RewriteKind`]: either the native
//!   DataFusion aggregate shape the call rewrites to before
//!   `lower_agg_intent` (or DataFusion's own physical planner) ever has to
//!   understand the ClickHouse name itself -- this is what generalizes
//!   `uniqExact`'s old bespoke `UniqExactRewrite` + `uniq_exact_udaf` pair
//!   (issue #221): a new builtin that rewrites to an already-handled shape is
//!   a new entry in this table, not a new `FunctionRewrite` impl and a new
//!   stub-`AggregateUDF` constructor -- or [`RewriteKind::PassThrough`] for a
//!   builtin with no native shape to rewrite to at all (`argMax`/`argMin`,
//!   issue #232): the call survives unchanged and `lower_agg_intent` handles
//!   the ClickHouse name itself directly.
//! - [`CLICKHOUSE_SCALAR_BUILTINS`] -- ClickHouse-only *scalar* names
//!   DataFusion doesn't know at all (`splitByChar`, `toDate`, `match`,
//!   the `toStartOf*` family, `startsWith`, `positionCaseInsensitive`). No
//!   [`RewriteKind`] here: unlike an aggregate call, a scalar call already
//!   lowers generically (`asap-frontend-sql::sql::expr`'s
//!   `Expr::ScalarFunction` arm), so a stub `ScalarUDF` registered for the
//!   name is the whole fix (issue #230).
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
    /// No native DataFusion aggregate shape to rewrite to at all -- the call
    /// survives unchanged (`ClickHouseBuiltinRewrite` is a no-op for it) and
    /// reaches `lower_agg_intent`, which builds an `AggIntent` directly from
    /// the ClickHouse name itself. `argMax`/`argMin` (issue #232): a
    /// two-column, row-selecting aggregate ("return `arg`'s value from the
    /// row where `val` is maximal") that fits no existing single-column
    /// `AggIntent` reducer shape and has no native DataFusion equivalent to
    /// borrow one from -- unlike `CountDistinct`/`CountIfToSum`, there is no
    /// "already-handled shape" for this rewrite to target.
    PassThrough,
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
    // argMax(arg, val) -- "arg's value from the row where val is maximal".
    // No native DataFusion aggregate shape to rewrite to (issue #232), so
    // `lower_agg_intent` builds `AggIntent::Extension` from these directly.
    ClickHouseBuiltin {
        name: "argmax",
        arity: Arity::Exact(2),
        rewrite: RewriteKind::PassThrough,
    },
    ClickHouseBuiltin {
        name: "argmin",
        arity: Arity::Exact(2),
        rewrite: RewriteKind::PassThrough,
    },
];

/// Aggregate function names DataFusion's own planner resolves out of the box
/// that this catalog deliberately does *not* map to a canonical
/// [`AggSemantic`] -- either because `AggIntent` has no shape for them
/// (a multi-column correlation/regression aggregate, a bitwise/boolean
/// aggregate, a string concatenation aggregate, ...) or because this front
/// end already rejects them explicitly elsewhere (`array_agg`, `grouping`).
///
/// This list exists for one reason: `asap-frontend-sql`'s DataFusion-registry
/// drift test (issue #225, item 3 -- see
/// `asap_frontend_sql::sql::catalog_drift`) walks a real `SessionContext`'s
/// resolved aggregate names and requires every one to be either in
/// [`NATIVE_FUNCTIONS`], a [`CLICKHOUSE_BUILTINS`] name, or listed here. A
/// name landing here is a *recorded decision*, not a silenced test failure --
/// each group below says why. Do not add an entry just to make the test
/// pass; add a `NativeFunction` instead if the name should actually lower.
///
/// Two known, narrow gaps rather than a deliberate non-goal: `var_sample` and
/// `var_population` are DataFusion's own alias spellings of `var_samp` /
/// `var_pop` (see `variance.rs`'s `aliases()` in `datafusion-functions-
/// aggregate`) that [`NATIVE_FUNCTIONS`] doesn't also list under those
/// spellings. Surfaced here rather than silently added to
/// [`NATIVE_FUNCTIONS`], since accepting a new spelling is a maintainer's
/// call, not something this catalog should do on its own.
pub const KNOWN_UNMAPPED_NATIVE_FUNCTIONS: &[&str] = &[
    // Selector aggregates: this front end only supports these as window
    // functions (`WindowFuncKind::FirstValue`/`LastValue`/`NthValue`, via
    // `OVER (...)`), not as plain `GROUP BY` aggregates -- no `AggIntent`
    // variant models "the value from a particular row" as a reduction.
    "first_value",
    "last_value",
    "nth_value",
    // Bitwise / boolean aggregates -- no corresponding `AggIntent` variant.
    "bit_and",
    "bit_or",
    "bit_xor",
    "bool_and",
    "bool_or",
    // Two-column correlation / linear-regression aggregates -- every
    // `AggIntent` value reducer takes one input column (`reducer_col` in
    // `asap-frontend-sql`), so these have no home yet.
    "corr",
    "covar",
    "covar_pop",
    "covar_samp",
    "regr_avgx",
    "regr_avgy",
    "regr_count",
    "regr_intercept",
    "regr_r2",
    "regr_slope",
    "regr_sxx",
    "regr_sxy",
    "regr_syy",
    // String concatenation -- no `AggIntent` equivalent.
    "string_agg",
    // The weighted-percentile variant of `approx_percentile_cont` (an extra
    // weight-column argument); only the unweighted form is in
    // `NATIVE_FUNCTIONS`.
    "approx_percentile_cont_with_weight",
    // Explicitly rejected elsewhere, not merely unmapped:
    // `array_agg_is_deliberately_rejected` (asap-frontend-sql's
    // sql_lowering tests) covers `array_agg`; `lower_grouping_sets`'s own
    // doc comment covers `grouping` (`GROUPING(col)` -- observable only via
    // the `__grouping_id` discriminator this front end drops).
    "array_agg",
    "grouping",
    // Known narrow gaps (see doc comment above) -- alias spellings of
    // `var_samp` / `var_pop` this catalog doesn't accept yet.
    "var_sample",
    "var_population",
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

/// One [`CLICKHOUSE_SCALAR_BUILTINS`] entry -- just `{ name, arity }`, no
/// [`RewriteKind`]. Unlike an aggregate call, a scalar function call in the
/// canonical IR is already deliberately opaque
/// (`asap-frontend-sql::sql::expr::df_expr_to_unresolved`'s
/// `Expr::ScalarFunction` arm lowers *any* scalar call generically to
/// `Unresolved::FunctionCall { name, args }`), so once DataFusion's planner
/// accepts the name at all -- via a stub `ScalarUDF`, see
/// `asap-frontend-sql::sql::clickhouse_scalar_builtin_stub_udf` -- the
/// existing generic lowering already produces a structurally correct node.
/// No rewrite/semantic classification is needed (issue #230).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickHouseScalarBuiltin {
    /// Lowercase function name, matching the name a stub `ScalarUDF` is
    /// registered under (DataFusion resolves a SQL call to it
    /// case-insensitively, but reports it back lowercase).
    pub name: &'static str,
    pub arity: Arity,
}

/// ClickHouse-only *scalar* builtin names DataFusion's planner has no native
/// equivalent for at all -- each needs a stub `ScalarUDF` registered so the
/// planner accepts the call. Arities follow ClickHouse's documented
/// signatures for each function (optional trailing arguments -- a timezone,
/// a start position, a max-substrings cap -- become an `Arity::Range`).
///
/// No return-type modeling here: the stub's Arrow return type (a single,
/// per-entry plausible choice, not modeled in this arity-only table) only
/// needs to let DataFusion's planner keep building the surrounding
/// expression's type -- see `clickhouse_scalar_builtin_stub_udf`'s call
/// sites in `SqlLowerer::build_context`.
pub const CLICKHOUSE_SCALAR_BUILTINS: &[ClickHouseScalarBuiltin] = &[
    // splitByChar(separator, s[, max_substrings]) -> Array(String).
    ClickHouseScalarBuiltin {
        name: "splitbychar",
        arity: Arity::Range { min: 2, max: 3 },
    },
    // toDate(expr) -> Date.
    ClickHouseScalarBuiltin {
        name: "todate",
        arity: Arity::Exact(1),
    },
    // match(haystack, pattern) -> UInt8 (0/1), used as a boolean predicate.
    ClickHouseScalarBuiltin {
        name: "match",
        arity: Arity::Exact(2),
    },
    // toStartOfHour(datetime[, timezone]) -> DateTime.
    ClickHouseScalarBuiltin {
        name: "tostartofhour",
        arity: Arity::Range { min: 1, max: 2 },
    },
    // toStartOfWeek(datetime[, mode[, timezone]]) -> Date.
    ClickHouseScalarBuiltin {
        name: "tostartofweek",
        arity: Arity::Range { min: 1, max: 3 },
    },
    // toStartOfMinute(datetime[, timezone]) -> DateTime.
    ClickHouseScalarBuiltin {
        name: "tostartofminute",
        arity: Arity::Range { min: 1, max: 2 },
    },
    // toStartOfFiveMinutes(datetime[, timezone]) -> DateTime.
    ClickHouseScalarBuiltin {
        name: "tostartoffiveminutes",
        arity: Arity::Range { min: 1, max: 2 },
    },
    // toStartOfInterval(datetime, INTERVAL x unit[, timezone]) -> DateTime.
    // The `INTERVAL x unit` clause parses as a single expression argument.
    ClickHouseScalarBuiltin {
        name: "tostartofinterval",
        arity: Arity::Range { min: 2, max: 3 },
    },
    // startsWith(s, prefix) -> UInt8 (0/1), used as a boolean predicate.
    ClickHouseScalarBuiltin {
        name: "startswith",
        arity: Arity::Exact(2),
    },
    // positionCaseInsensitive(haystack, needle[, start_pos]) -> UInt64
    // (1-based position, 0 if not found).
    ClickHouseScalarBuiltin {
        name: "positioncaseinsensitive",
        arity: Arity::Range { min: 2, max: 3 },
    },
];

/// Look up a ClickHouse-only scalar builtin by name (case-sensitive, see
/// [`lookup_native`]).
pub fn lookup_clickhouse_scalar_builtin(name: &str) -> Option<&'static ClickHouseScalarBuiltin> {
    CLICKHOUSE_SCALAR_BUILTINS.iter().find(|b| b.name == name)
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

    /// `KNOWN_UNMAPPED_NATIVE_FUNCTIONS` documents names this catalog
    /// deliberately does *not* map -- it should never overlap with a table
    /// that *does* map the same name (that would be a contradiction: mapped
    /// and "known unmapped" at once), and shouldn't repeat itself either
    /// (each name is a one-time recorded decision).
    #[test]
    fn known_unmapped_list_is_disjoint_from_the_mapped_tables_and_has_no_duplicates() {
        for name in KNOWN_UNMAPPED_NATIVE_FUNCTIONS {
            assert!(
                lookup_native(name).is_none(),
                "{name} is both in NATIVE_FUNCTIONS and KNOWN_UNMAPPED_NATIVE_FUNCTIONS"
            );
            assert!(
                lookup_clickhouse_builtin(name).is_none(),
                "{name} is both in CLICKHOUSE_BUILTINS and KNOWN_UNMAPPED_NATIVE_FUNCTIONS"
            );
        }
        let mut sorted = KNOWN_UNMAPPED_NATIVE_FUNCTIONS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            KNOWN_UNMAPPED_NATIVE_FUNCTIONS.len(),
            "KNOWN_UNMAPPED_NATIVE_FUNCTIONS has a duplicate entry"
        );
    }

    #[test]
    fn known_unmapped_names_are_lowercase() {
        for name in KNOWN_UNMAPPED_NATIVE_FUNCTIONS {
            assert_eq!(*name, name.to_lowercase(), "not lowercase: {name}");
        }
    }

    #[test]
    fn clickhouse_scalar_builtin_lookup_finds_every_listed_name() {
        for b in CLICKHOUSE_SCALAR_BUILTINS {
            let found =
                lookup_clickhouse_scalar_builtin(b.name).expect("listed name must be found");
            assert_eq!(found.name, b.name);
            assert_eq!(found.arity, b.arity);
        }
        assert_eq!(
            lookup_clickhouse_scalar_builtin("not_a_real_function"),
            None
        );
    }

    #[test]
    fn clickhouse_scalar_builtin_names_are_lowercase() {
        for b in CLICKHOUSE_SCALAR_BUILTINS {
            assert_eq!(b.name, b.name.to_lowercase(), "not lowercase: {}", b.name);
        }
    }

    /// Scalar builtins live in their own namespace from the aggregate
    /// tables: a scalar and an aggregate function can share a bare SQL name
    /// in general, but none of this catalog's entries happen to collide, so
    /// this documents that rather than asserting a real invariant this crate
    /// enforces elsewhere.
    #[test]
    fn clickhouse_scalar_builtins_do_not_shadow_native_or_aggregate_names() {
        for b in CLICKHOUSE_SCALAR_BUILTINS {
            assert!(
                lookup_native(b.name).is_none(),
                "{} listed as both a native aggregate and a ClickHouse scalar builtin",
                b.name
            );
            assert!(
                lookup_clickhouse_builtin(b.name).is_none(),
                "{} listed as both a ClickHouse aggregate and scalar builtin",
                b.name
            );
        }
    }
}
