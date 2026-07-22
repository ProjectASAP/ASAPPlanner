//! Serving-time execution model for the L4 IR.
//!
//! `asap-sketch` defines the L4 tree ([`L4Node`]/[`SummaryExpr`]);
//! `asap-plan` builds one from L3 (`asap_plan::bind`). Neither says what it
//! means to *run* one against already-materialized state at query time —
//! this module is that shared answer, so deployments don't each reinvent
//! the walk and merge rules.
//!
//! [`SummaryExecutor`] is the trait a deployment implements to plug in its
//! own storage/lookup/sketch-math/readout; [`execute`] is the generic
//! recursive walk over `L4Node` that calls into it at the right points.
//! `asap-sketch` owns the *structural* rules (which nestings are valid,
//! that a `SummaryMerge`'s children must agree on `(SummaryKind,
//! SummaryParams)`); the deployment owns everything that requires an
//! actual sketch-math implementation (this crate has none — see the crate
//! doc) or actual storage.
//!
//! ## Planning-time L4 vs. serving-time L4
//!
//! `L4Node` now serves two different operations, not one:
//!
//! - **Planning** (`asap_plan::bind`): `QueryExpr -> L4Node`, a *decision* —
//!   which sketch family/params should answer this intent, made once per
//!   query shape, symbolically, with no reference to what's actually
//!   stored anywhere.
//! - **Serving** (this module): `L4Node -> Value`, a *lookup* — given a
//!   tree of already-made decisions, resolve each leaf against whatever is
//!   actually materialized right now, which may be missing, may be
//!   multiple instances needing a merge, or may disagree on params.
//!   `ExecError::NoCandidates`/`MergeKindParamsMismatch` exist because
//!   reality can diverge from the plan in ways planning time never sees.
//!
//! ## Nested composition
//!
//! `L4Node`'s children are `Rc<L4Node>`, so the tree nests to arbitrary
//! depth by construction — [`execute`] is plain recursion and needs no
//! special-casing for "how many levels deep." Two things are worth being
//! explicit about because they aren't obvious from the type alone:
//!
//! - **What can be a `SummaryMerge` child.** Only nodes that produce a
//!   summary *state* — `SummaryAgg` or another `SummaryMerge` — are valid.
//!   A `SummaryEstimate` child (or a bare `Logical`) has already collapsed
//!   to a plain value; merging values isn't the same operation as merging
//!   sketch states, and isn't what this type models. [`execute`] rejects it
//!   ([`ExecError::MergeChildNotState`]) rather than silently doing
//!   something with it.
//! - **Merge-of-merges preserves the `(kind, params)` invariant
//!   transitively** — a `SummaryMerge` over two `SummaryMerge` children is
//!   valid exactly when both sides ultimately agree on the same
//!   `(SummaryKind, SummaryParams)`, the same rule as a flat merge. This
//!   falls out of `execute` propagating the agreed `(kind, params)` up
//!   through `ExecOutcome::State` at every level rather than re-deriving it
//!   from scratch per node.
//!
//! One open question this module deliberately does *not* resolve: whether a
//! `SummaryAgg` can ever validly sit *above* a `SummaryEstimate` (i.e.
//! "build a new summary from another summary's already-computed readout,
//! at query time"). Every materialized-summary store this design has been
//! checked against (see ASAPQuery-backend's `data_plane`) only serves
//! summaries built incrementally at *ingest* time from raw samples — there
//! is no precedent for building one from a query-time-derived scalar. A
//! `SummaryExecutor` that hits this shape today should treat it as
//! unsupported (return an error from `find_candidates`) rather than assume
//! either answer.
//!
//! ## What this module does *not* do
//!
//! `asap-sketch` has no sketch-math implementation — no KLL/CMS/HLL/DDSketch
//! algorithm lives in this crate or `asap-plan`. `SummaryExecutor::State`,
//! `merge_states`, and `readout` are entirely deployment-supplied for that
//! reason; this module only guarantees they're never called on
//! (`kind`, `params`) combinations that disagree with each other.

use asap_ir::intent_algebra::{ColumnId, ColumnRef, QueryExpr};

use crate::expr::{L4Node, SummaryExpr};
use crate::sketch::{SketchQuery, SummaryKind, SummaryParams};

/// Deployment-supplied backend for executing an [`L4Node`] tree against
/// materialized state. See the module docs for the split of
/// responsibilities between this trait and [`execute`].
pub trait SummaryExecutor {
    /// Opaque reference to one materialized summary instance (e.g. a data
    /// plane's sid). Cheap to clone — `execute` may hold several at once
    /// while resolving a `SummaryAgg`'s candidates.
    type Handle: Clone;
    /// Decoded, in-memory state for one instance (or an already-merged
    /// group of instances) — sketch bytes, an exact accumulator, whatever
    /// the deployment's storage actually holds.
    type State;
    /// A query answer — deployment-defined shape (one row, one scalar, a
    /// whole series set).
    type Value;
    /// Deployment error type — storage failures, decode failures, etc.
    type Error;

    /// Resolve a `SummaryAgg` leaf to the materialized-instance handles
    /// that answer it. `child` is the `SummaryAgg`'s own child subtree
    /// (typically `Logical(Window{Scan{..}})` — implementations recurse
    /// into it themselves to find the metric/source identity; this module
    /// does not interpret `QueryExpr` shapes on the deployment's behalf).
    ///
    /// Implementations MUST only return handles whose materialized
    /// `(SummaryKind, SummaryParams)` is exactly `(sketch, params)` — if
    /// more than one handle is returned, [`execute`] merges them directly
    /// via [`Self::merge_states`] without re-checking agreement (unlike
    /// `SummaryMerge`'s children, which *are* re-checked — see the module
    /// docs). Returning `Ok(vec![])` means "no materialized instance
    /// found"; `execute` reports that as [`ExecError::NoCandidates`].
    fn find_candidates(
        &self,
        sketch: &SummaryKind,
        params: &SummaryParams,
        col: &ColumnRef,
        by: &[ColumnId],
        child: &L4Node,
    ) -> Result<Vec<Self::Handle>, Self::Error>;

    /// Fetch/decode one handle's raw state.
    fn fetch_state(&self, handle: &Self::Handle) -> Result<Self::State, Self::Error>;

    /// Merge two or more states, all sharing the same `(SummaryKind,
    /// SummaryParams)` — `execute` never calls this with states it hasn't
    /// already verified agree (either by `find_candidates`'s own contract,
    /// for a single `SummaryAgg`'s candidates, or by `execute`'s explicit
    /// check across `SummaryMerge`'s children).
    fn merge_states(&self, states: Vec<Self::State>) -> Result<Self::State, Self::Error>;

    /// Read a query out of a built state.
    fn readout(&self, state: &Self::State, query: &SketchQuery) -> Result<Self::Value, Self::Error>;

    /// Handle a `SummaryExpr::Logical` node — nothing was committed for
    /// this subtree at L4. The deployment decides what that means (exact
    /// execution, an archive-tier miss, ...); `asap-sketch` has no opinion.
    fn logical(&self, expr: &QueryExpr) -> Result<Self::Value, Self::Error>;
}

/// Result of executing one [`L4Node`] — either a summary state (still
/// tagged with the `(kind, params)` it was built as, so a parent
/// `SummaryMerge`/`SummaryEstimate` can act on it without re-deriving that
/// information from `L4Schema`) or a final value.
pub enum ExecOutcome<E: SummaryExecutor + ?Sized> {
    State {
        state: E::State,
        kind: SummaryKind,
        params: SummaryParams,
    },
    Value(E::Value),
}

/// Errors [`execute`] itself can raise, on top of the deployment's own
/// [`SummaryExecutor::Error`].
#[derive(Debug)]
pub enum ExecError<Inner> {
    /// A `SummaryAgg` leaf's [`SummaryExecutor::find_candidates`] returned
    /// no handles.
    NoCandidates,
    /// A `SummaryMerge` child evaluated to a [`ExecOutcome::Value`] instead
    /// of a state — only `SummaryAgg`/`SummaryMerge` children are valid
    /// (see the module docs).
    MergeChildNotState,
    /// Two `SummaryMerge` children disagreed on `(SummaryKind,
    /// SummaryParams)` — the child-level equivalent of the plan-time
    /// invariant `asap-plan`'s binder already enforces within one binding
    /// pass; re-checked here because a `SummaryMerge`'s children can, in
    /// principle, be resolved through entirely independent paths (e.g. two
    /// materialized instances built at different times under different
    /// catalog defaults) that a single build-time check can't see.
    MergeKindParamsMismatch,
    /// A `SummaryMerge` had zero children.
    EmptyMerge,
    /// `SummaryJoin`/`SummarySubtract`/`SummaryDelete` — no `Bind*` path in
    /// `asap-plan` produces these yet (see `physical_expr`-adjacent
    /// deployment docs); unreachable in practice today.
    NotYetSupported(&'static str),
    /// The deployment's own error.
    Executor(Inner),
}

impl<Inner> From<Inner> for ExecError<Inner> {
    fn from(e: Inner) -> Self {
        ExecError::Executor(e)
    }
}

/// Recursively execute `node` against `exec`. See the module docs for the
/// composition/merge rules this enforces generically, independent of any
/// deployment's storage or sketch-math implementation.
pub fn execute<E: SummaryExecutor>(
    node: &L4Node,
    exec: &E,
) -> Result<ExecOutcome<E>, ExecError<E::Error>> {
    match &node.expr {
        SummaryExpr::Logical(qe) => Ok(ExecOutcome::Value(exec.logical(qe)?)),

        SummaryExpr::SummaryAgg {
            child,
            sketch,
            params,
            col,
            by,
        } => {
            let handles = exec.find_candidates(sketch, params, col, by, child)?;
            if handles.is_empty() {
                return Err(ExecError::NoCandidates);
            }
            let states = handles
                .iter()
                .map(|h| exec.fetch_state(h))
                .collect::<Result<Vec<_>, _>>()?;
            let state = fold_states(states, exec)?;
            Ok(ExecOutcome::State {
                state,
                kind: sketch.clone(),
                params: params.clone(),
            })
        }

        SummaryExpr::SummaryEstimate { sketch_input, query } => {
            let (state, _kind, _params) = expect_state(execute(sketch_input, exec)?)?;
            Ok(ExecOutcome::Value(exec.readout(&state, query)?))
        }

        SummaryExpr::SummaryMerge { children } => {
            if children.is_empty() {
                return Err(ExecError::EmptyMerge);
            }
            let mut agreed: Option<(SummaryKind, SummaryParams)> = None;
            let mut states = Vec::with_capacity(children.len());
            for child in children {
                let (state, kind, params) = expect_state(execute(child, exec)?)?;
                match &agreed {
                    None => agreed = Some((kind, params)),
                    Some((k, p)) if *k == kind && *p == params => {}
                    Some(_) => return Err(ExecError::MergeKindParamsMismatch),
                }
                states.push(state);
            }
            let (kind, params) = agreed.expect("checked non-empty above");
            let merged = fold_states(states, exec)?;
            Ok(ExecOutcome::State {
                state: merged,
                kind,
                params,
            })
        }

        SummaryExpr::SummaryJoin { .. } => Err(ExecError::NotYetSupported("SummaryJoin")),
        SummaryExpr::SummarySubtract { .. } => Err(ExecError::NotYetSupported("SummarySubtract")),
        SummaryExpr::SummaryDelete { .. } => Err(ExecError::NotYetSupported("SummaryDelete")),
    }
}

/// Fold one-or-more same-`(kind, params)` states into one, skipping the
/// `merge_states` call entirely for the (overwhelmingly common) single-state
/// case rather than making every executor implement a no-op merge.
fn fold_states<E: SummaryExecutor>(
    mut states: Vec<E::State>,
    exec: &E,
) -> Result<E::State, ExecError<E::Error>> {
    if states.len() == 1 {
        Ok(states.pop().expect("len == 1"))
    } else {
        Ok(exec.merge_states(states)?)
    }
}

/// Unwrap an [`ExecOutcome::State`], erroring if it was actually a `Value`
/// (see [`ExecError::MergeChildNotState`]'s doc for why this can happen).
#[allow(clippy::type_complexity)]
fn expect_state<E: SummaryExecutor>(
    outcome: ExecOutcome<E>,
) -> Result<(E::State, SummaryKind, SummaryParams), ExecError<E::Error>> {
    match outcome {
        ExecOutcome::State { state, kind, params } => Ok((state, kind, params)),
        ExecOutcome::Value(_) => Err(ExecError::MergeChildNotState),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{L4DataType, L4Field, L4Schema};
    use asap_ir::intent_algebra::{Column, DataType, Schema, Source};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    fn scan() -> QueryExpr {
        QueryExpr::Scan {
            source: Source::TimeSeries {
                metric: "m".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }
    }

    fn lift(schema_fields: Vec<&str>) -> L4Schema {
        L4Schema {
            fields: schema_fields
                .into_iter()
                .map(|n| L4Field {
                    name: n.into(),
                    dtype: L4DataType::Primitive(DataType::Float64),
                    nullable: false,
                })
                .collect(),
            time_index: None,
        }
    }

    fn logical_node() -> Rc<L4Node> {
        Rc::new(L4Node {
            expr: SummaryExpr::Logical(Box::new(scan())),
            schema: lift(vec!["ts", "value"]),
        })
    }

    fn agg_node(sketch: SummaryKind, params: SummaryParams, child: Rc<L4Node>) -> Rc<L4Node> {
        Rc::new(L4Node {
            expr: SummaryExpr::SummaryAgg {
                child,
                sketch,
                params,
                col: ColumnRef::SampleValue,
                by: vec![],
            },
            schema: lift(vec!["value"]),
        })
    }

    fn estimate_node(sketch_input: Rc<L4Node>, query: SketchQuery) -> Rc<L4Node> {
        Rc::new(L4Node {
            expr: SummaryExpr::SummaryEstimate { sketch_input, query },
            schema: lift(vec!["value"]),
        })
    }

    fn merge_node(children: Vec<Rc<L4Node>>) -> Rc<L4Node> {
        Rc::new(L4Node {
            expr: SummaryExpr::SummaryMerge { children },
            schema: lift(vec!["value"]),
        })
    }

    /// Trivial in-memory executor for exercising `execute`'s tree-walking
    /// and merge-precondition logic — `State`/`Value` are just `f64`s, and
    /// `merge_states` is `sum`, so tests can assert on concrete numbers
    /// without any real sketch-math dependency.
    struct MockExecutor {
        /// handle -> raw value.
        values: RefCell<HashMap<u64, f64>>,
        next_handle: RefCell<u64>,
    }

    impl MockExecutor {
        fn new() -> Self {
            Self {
                values: RefCell::new(HashMap::new()),
                next_handle: RefCell::new(0),
            }
        }

        /// Register one materialized instance, returning its handle.
        fn register(&self, value: f64) -> u64 {
            let mut h = self.next_handle.borrow_mut();
            let handle = *h;
            *h += 1;
            self.values.borrow_mut().insert(handle, value);
            handle
        }
    }

    impl SummaryExecutor for MockExecutor {
        type Handle = u64;
        type State = f64;
        type Value = f64;
        type Error = String;

        fn find_candidates(
            &self,
            _sketch: &SummaryKind,
            _params: &SummaryParams,
            _col: &ColumnRef,
            _by: &[ColumnId],
            _child: &L4Node,
        ) -> Result<Vec<Self::Handle>, Self::Error> {
            Ok(self.values.borrow().keys().copied().collect())
        }

        fn fetch_state(&self, handle: &Self::Handle) -> Result<Self::State, Self::Error> {
            self.values
                .borrow()
                .get(handle)
                .copied()
                .ok_or_else(|| "unknown handle".to_string())
        }

        fn merge_states(&self, states: Vec<Self::State>) -> Result<Self::State, Self::Error> {
            Ok(states.into_iter().sum())
        }

        fn readout(&self, state: &Self::State, _query: &SketchQuery) -> Result<Self::Value, Self::Error> {
            Ok(*state)
        }

        fn logical(&self, _expr: &QueryExpr) -> Result<Self::Value, Self::Error> {
            Ok(-1.0)
        }
    }

    fn kll() -> (SummaryKind, SummaryParams) {
        (SummaryKind::Kll, SummaryParams::Kll { k: 200 })
    }

    fn sum() -> (SummaryKind, SummaryParams) {
        (SummaryKind::Sum, SummaryParams::Sum)
    }

    #[test]
    fn single_agg_reads_out_directly() {
        let exec = MockExecutor::new();
        exec.register(7.0);
        let (kind, params) = sum();
        let tree = estimate_node(
            agg_node(kind, params, logical_node()),
            SketchQuery::Cardinality,
        );
        let ExecOutcome::Value(v) = execute(&tree, &exec).unwrap() else {
            panic!("expected a value");
        };
        assert_eq!(v, 7.0);
    }

    #[test]
    fn multiple_candidates_for_one_agg_are_merged() {
        let exec = MockExecutor::new();
        exec.register(3.0);
        exec.register(4.0);
        let (kind, params) = sum();
        let tree = estimate_node(
            agg_node(kind, params, logical_node()),
            SketchQuery::Cardinality,
        );
        let ExecOutcome::Value(v) = execute(&tree, &exec).unwrap() else {
            panic!("expected a value");
        };
        assert_eq!(v, 7.0); // MockExecutor::merge_states sums
    }

    #[test]
    fn logical_leaf_defers_to_executor() {
        let exec = MockExecutor::new();
        let tree = logical_node();
        let ExecOutcome::Value(v) = execute(&tree, &exec).unwrap() else {
            panic!("expected a value");
        };
        assert_eq!(v, -1.0);
    }

    #[test]
    fn nested_summary_agg_recurses_through_both_levels() {
        // quantile(0.9, sum by (job) (m)) shape: Kll wraps Sum.
        let exec = MockExecutor::new();
        exec.register(10.0);
        let (sum_kind, sum_params) = sum();
        let (kll_kind, kll_params) = kll();
        let inner = agg_node(sum_kind, sum_params, logical_node());
        let outer = agg_node(kll_kind, kll_params, inner);
        let tree = estimate_node(outer, SketchQuery::Quantile { q: 0.9 });
        let ExecOutcome::Value(v) = execute(&tree, &exec).unwrap() else {
            panic!("expected a value");
        };
        // MockExecutor::find_candidates ignores the tree shape and always
        // returns every registered handle -- the point of this test is
        // that `execute` actually reaches both levels (no panic/short
        // circuit on the nested SummaryAgg), not the specific value.
        assert_eq!(v, 10.0);
    }

    #[test]
    fn merge_of_two_aggs_with_matching_kind_params_sums() {
        let exec = MockExecutor::new();
        let (kind, params) = sum();
        let a = agg_node(kind.clone(), params.clone(), logical_node());
        let b = agg_node(kind, params, logical_node());
        // Each SummaryAgg independently pulls every registered handle from
        // the shared MockExecutor (2 handles), so the merge sees two
        // already-summed inputs. Register after building the tree so each
        // `find_candidates` call sees the same two handles.
        exec.register(1.0);
        exec.register(2.0);
        let merged = merge_node(vec![a, b]);
        let tree = estimate_node(merged, SketchQuery::Cardinality);
        let ExecOutcome::Value(v) = execute(&tree, &exec).unwrap() else {
            panic!("expected a value");
        };
        // Each child sums to 3.0 (1.0 + 2.0); the outer merge sums the two
        // children's results: 3.0 + 3.0.
        assert_eq!(v, 6.0);
    }

    #[test]
    fn merge_of_merges_nests_arbitrarily_deep() {
        let exec = MockExecutor::new();
        exec.register(5.0);
        let (kind, params) = sum();
        let leaf_a = agg_node(kind.clone(), params.clone(), logical_node());
        let leaf_b = agg_node(kind.clone(), params.clone(), logical_node());
        let inner_merge = merge_node(vec![leaf_a, leaf_b]);
        let leaf_c = agg_node(kind, params, logical_node());
        let outer_merge = merge_node(vec![inner_merge, leaf_c]);
        let tree = estimate_node(outer_merge, SketchQuery::Cardinality);
        assert!(execute(&tree, &exec).is_ok());
    }

    #[test]
    fn merge_rejects_mismatched_kind_params() {
        let exec = MockExecutor::new();
        exec.register(1.0);
        let (sum_kind, sum_params) = sum();
        let (kll_kind, kll_params) = kll();
        let a = agg_node(sum_kind, sum_params, logical_node());
        let b = agg_node(kll_kind, kll_params, logical_node());
        let merged = merge_node(vec![a, b]);
        let tree = estimate_node(merged, SketchQuery::Cardinality);
        match execute(&tree, &exec) {
            Err(ExecError::MergeKindParamsMismatch) => {}
            other => panic!("expected MergeKindParamsMismatch, got a different outcome: {}", other.is_ok()),
        }
    }

    #[test]
    fn merge_rejects_a_value_producing_child() {
        let exec = MockExecutor::new();
        exec.register(1.0);
        let (kind, params) = sum();
        let state_child = agg_node(kind, params, logical_node());
        let value_child = estimate_node(logical_node(), SketchQuery::Cardinality);
        let merged = merge_node(vec![state_child, value_child]);
        let tree = estimate_node(merged, SketchQuery::Cardinality);
        match execute(&tree, &exec) {
            Err(ExecError::MergeChildNotState) => {}
            other => panic!("expected MergeChildNotState, got a different outcome: {}", other.is_ok()),
        }
    }

    #[test]
    fn no_candidates_is_a_distinct_error() {
        let exec = MockExecutor::new(); // nothing registered
        let (kind, params) = sum();
        let tree = estimate_node(
            agg_node(kind, params, logical_node()),
            SketchQuery::Cardinality,
        );
        match execute(&tree, &exec) {
            Err(ExecError::NoCandidates) => {}
            other => panic!("expected NoCandidates, got a different outcome: {}", other.is_ok()),
        }
    }
}
