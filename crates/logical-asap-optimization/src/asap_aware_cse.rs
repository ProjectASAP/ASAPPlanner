//! Stage 1 (logical ASAP-aware optimization), Pass 2: ASAP-aware
//! common-subexpression elimination, as defined in
//! `docs/design_docs/proposals/planner-layering.md` (Algorithm 2).
//!
//! Its three rules are implemented: the identical-expression rule, the
//! summary-capability rule and, for tumbling panes, the
//! window-composition rule; so are two reuse rules (#680), rollup and exact
//! top-k reuse.
//!
//! # Identical-expression rule
//!
//! A *sharing candidate* ([`SharingCandidate`]) of a whole-workload
//! candidate is a set of two or more of its nodes with identical input and
//! computation semantics that are legal to share: the `ir::cse` interning
//! merges only ASAP nodes, scans, and nodes with a unique key.
//! [`identical_expression_pass2_candidates`] returns one Pass 2 candidate
//! per non-empty subset of the sharing candidates (Algorithm 2, line 31):
//! each sharing candidate in the subset becomes one node serving every
//! consumer, and every other stays separate. The caller keeps the
//! independent candidate beside them, so plan selection compares all of
//! them; nothing is capped.
//!
//! Sharing a node shares the sub-DAG below it. A subset that leaves out a
//! sharing candidate whose nodes all lie below the nodes of one it shares
//! gives the same Pass 2 candidate as the subset with it; such a Pass 2
//! candidate is returned once, under the subset it actually shares.
//!
//! Scans of one metric that project different labels read the same rows.
//! [`with_union_scan_labels`] widens them to the union of their labels, so
//! interning finds them identical; the caller also lists the Pass 2
//! candidates of the widened roots that share a scan.
//!
//! # Summary-capability rule
//!
//! Composition sizes each consumer's summary build alone, for its own
//! accuracy target. The *members* of a [`CapabilitySharingCandidate`] are summary
//! builds of one candidate that chose the same family (the same
//! [`SketchAlgorithm`](asap_types::ir::schema::SketchAlgorithm) at any
//! size, or the same per-series exact accumulator) over the same input
//! sub-DAG (window, `WHERE` and time range included), item and weight,
//! grouping and filter, and whose every reader is an estimate the family
//! answers ([`FamilyCapability::answers`]) or, for an exact accumulator,
//! its finalization. They differ in their sizes, or, for per-series builds
//! over one metric's scans without label matchers, in the labels those
//! scans project: the shared build is over the scan with every member's
//! labels, and each other member's readers keep only their own columns.
//! [`summary_capability_pass2_candidates`] returns one Pass 2 candidate per
//! choice, for each capability sharing candidate, of no sharing or one
//! subset of two or more of its members
//! whose sizes differ (Q1: partial sharing too): the subset becomes one
//! build serving all its readers, sized with each parameter's largest value
//! among them ([`size_for_all_readers`]: each consumer sized alone, the shared
//! build the per-parameter maximum, a KLL's failure probability the
//! minimum and its `k` the largest after each is resized to that minimum),
//! and every guarantee above it is composed again, so plan
//! selection checks each consumer against the shared state. Members of
//! equal size are the identical-expression rule's; builds that are not
//! shared are untouched, and a consumer never changes family.
//!
//! # Window-composition rule
//!
//! A query repeating every `E` over a window `W` can build its summary as
//! back-to-back tumbling panes of width `w = gcd(W, E)` (Algorithm 2's
//! `Tumbling(L)` for one query), merged before each read. A *pane
//! candidate* ([`pane_candidates`]) is a `SummaryAgg` over a range
//! `TimeRange(W)` whose
//! family merges in the family-capability table ([`FamilyCapability`]):
//! exact accumulators included, and also families whose merged readouts
//! are only approximate (heap top-k), whose readers then carry no
//! guarantee and are left to plan selection's evidence. A family without a
//! merge (IRate) has no pane candidate.
//! [`window_composition_pass2_candidates`] returns one Pass 2 candidate per
//! non-empty subset of the pane candidates: the build of each in the subset
//! becomes a `SummaryMerge` over one [`ASAPOp::PaneChain`] of `W / w` panes,
//! a compact node whatever the pane count, with the build's family and
//! size (a mergeable family's merged state has one build's guarantee), and
//! every guarantee above is composed again. Nothing is materialized here:
//! the panes are rebuilt at each evaluation until Stage 2 keeps them.
//!
//! Nested windows share one chain: when a subset holds builds of one family
//! (at any size) over the same input below their ranges, read the same way,
//! with the same output column names, whose panes differ in width or count,
//! the subset also comes as a Pass 2 candidate in which each such group
//! reads one chain ([`SharedPaneChain`]) over the longest window, sized for
//! every member ([`size_for_all_readers`]). Two grids are offered: the
//! finest member's own panes, for the members whose widths it divides
//! ([`nested_window_groups`]), and, when that differs, the gcd of all the
//! members' widths, even finer than every member's own panes
//! ([`gcd_window_groups`]); Stage 3's pricing weighs the extra states and
//! merges of finer panes. Queries repeating at different intervals read the
//! same panes, each at its own cadence. Every window ends at the evaluation
//! time, so each shorter one is the chain's newest panes: its
//! `SummaryMerge` merges only those (`newest_panes`), and has the guarantee
//! of one build over its own window. A build grouped by fewer keys also
//! reads the chain of a finer grouping over a window at least its own
//! ([`rollup_window_groups`]): it merges its newest panes, then rolls the
//! groups up into its own, as the rollup rule does. Stage 2 never keeps a
//! chain read at different intervals across evaluations from query time
//! (each reader would need one new pane per evaluation). Long-window segments, sliding
//! windows and Exponential Histograms are not implemented.
//!
//! # Rollup rule
//!
//! A coarse grouped computation can be answered from a finer one the
//! candidate already computes over the same rows: `sum by (job)` from
//! `sum by (job, instance)`. The rule never introduces a finer grouping
//! nobody asked for. A *rollup candidate* ([`rollup_candidates`]) is a
//! coarse build and a finer build of the same family over the same rows (a
//! PromQL scan may project other labels), whose group keys include the
//! coarse ones:
//! - An exact accumulator whose family merges across disjoint groups with
//!   an exact merged value ([`rollup_kind`]: sums, counts, minima and
//!   maxima; counter states merge only in time). The coarse build becomes
//!   an exact accumulator over its coarsest finer build's finalized values,
//!   grouped by the coarse keys (a count sums the finer counts), so the
//!   value stays exact.
//! - A per-group sketch (KMV, HLL, KLL, DDSketch, Count-Min, Count Sketch,
//!   UnivMon, at any size) read only by estimates whose merged readouts
//!   keep a guarantee (not a heap's top-k or UnivMon's entropy). The coarse build becomes a rollup (a regrouping
//!   `SummaryMerge`) of its finest finer build's states by the coarse
//!   keys. The groups of one build partition its rows, so the rolled-up
//!   state keeps the family's merged guarantee: the direct coarse build's
//!   for KMV, HLL, DDSketch and the linear sketches, the same rank bound
//!   for KLL.
//!
//! [`rollup_pass2_candidates`] returns one Pass 2 candidate per non-empty
//! subset of the rollup *choices* ([`rollup_choices`]): each exact rollup
//! alone, and every sketch rollup of one finest build together, since they
//! share its size. A chosen sketch choice sizes the finest build for every
//! reader, its own and each coarse one's (the strictest consumer, as the
//! summary-capability rule sizes a shared build), and composes every
//! guarantee above it again. So 15 groupings of one window are one choice,
//! not 2^14 subsets.
//!
//! # Exact top-k reuse rule
//!
//! Exact top-k computations of one input and partition with different `k`
//! read one ranking: the rows of the largest top-k, `Limit(k_max)` over
//! `Sort`, which hold every smaller top-k ([`topk_reuse_candidates`],
//! [`topk_reuse_pass2_candidates`]). Only exact computations over exact input are
//! reused; a sketch's heap is not, as its membership has no guarantee yet.

use std::collections::HashMap;
use std::collections::HashSet;
use std::rc::Rc;

use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::{
    ExactKind, FamilyCapability, FieldDataType, GroupingStrategy, MergeInputRule, MergedError,
    Schema, SketchKind, PROMQL_SERIES_IDENTITY,
};
use asap_types::ir::ScalarExpr;
use asap_types::ir::SortKey;
use asap_types::ir::{
    ASAPOp, NonASAPOp, Operator, OperatorNode, OperatorResultKind, QueryRoot, TimeRangeKind,
};

use crate::accuracy::estimators::size_for_all_readers;
use crate::per_computation_candidates::{recompose_guarantees, PerComputationCandidateError};

/// A Pass 2 candidate of the identical-expression rule: the candidate's
/// roots with each of `sharing_candidates` made one node.
#[derive(Debug, Clone)]
pub struct IdenticalExpressionPass2Candidate {
    /// The sharing candidates made one node, in [`sharing_candidates`]
    /// order.
    pub sharing_candidates: Vec<SharingCandidate>,
    /// The candidate's roots with those nodes shared.
    pub roots: Vec<QueryRoot>,
}

/// Identical nodes the identical-expression rule can make one node, in the
/// order a children-first walk of the roots meets them.
#[derive(Debug, Clone)]
pub struct SharingCandidate {
    /// Two or more nodes with identical input and computation.
    pub nodes: Vec<Rc<OperatorNode>>,
}

/// The sharing candidates of the candidate with query roots `roots`, children
/// before parents.
pub fn sharing_candidates(roots: &[QueryRoot]) -> Vec<SharingCandidate> {
    let operators: Vec<Rc<OperatorNode>> = roots.iter().flat_map(root_operators).cloned().collect();
    let interned = share_common_sub_dags(operators.iter().cloned().enumerate().collect());
    // Each node's interned representative: interning rebuilds every node
    // over its children in order, so the two DAGs are walked side by side.
    let mut representative: HashMap<*const OperatorNode, *const OperatorNode> = HashMap::new();
    let mut order = Vec::new();
    for (original, (_, merged)) in operators.iter().zip(&interned) {
        pair(original, merged, &mut representative, &mut order);
    }
    let mut members: HashMap<*const OperatorNode, Vec<Rc<OperatorNode>>> = HashMap::new();
    let mut first_seen = Vec::new();
    for node in order {
        let key = representative[&Rc::as_ptr(&node)];
        let entry = members.entry(key).or_default();
        if entry.is_empty() {
            first_seen.push(key);
        }
        entry.push(node);
    }
    first_seen
        .into_iter()
        .filter_map(|key| {
            let nodes = members.remove(&key)?;
            (nodes.len() >= 2).then_some(SharingCandidate { nodes })
        })
        .collect()
}

/// Record `original`'s representative `merged` and recurse into the
/// children, then list `original` (children first, each node once).
fn pair(
    original: &Rc<OperatorNode>,
    merged: &Rc<OperatorNode>,
    representative: &mut HashMap<*const OperatorNode, *const OperatorNode>,
    order: &mut Vec<Rc<OperatorNode>>,
) {
    if representative.contains_key(&Rc::as_ptr(original)) {
        return;
    }
    representative.insert(Rc::as_ptr(original), Rc::as_ptr(merged));
    for (child, merged_child) in original.children().into_iter().zip(merged.children()) {
        pair(child, merged_child, representative, order);
    }
    order.push(Rc::clone(original));
}

/// `roots` with the time-series scans of one metric made identical when
/// they project different labels, so that [`sharing_candidates`] finds
/// them: the PromQL frontend projects only the labels each query uses
/// (`ts, value, job, …` for `quantile by (job) (0.5, data)`), and
/// structural CSE shares only nodes with equal schemas. Every scan of a
/// metric without predicates projects the union of the labels its scans
/// project, in the order the walk meets them (before the series identity,
/// with the unique keys moved along).
/// The nodes that pass the labels on ([`passes_labels_on`]) keep the wider
/// rows above a widened scan; any other consumer reads them through a
/// `Project` that keeps the columns it read, by name, as
/// `relabeled_estimate` does. `None` when every metric's scans already
/// agree, when two of them give one label different types or one is closed
/// and the other not, when a node would change arity, or when a consumer
/// would have to narrow a summary state or a grouped or filtered pane chain
/// would read a `Project`.
pub fn with_union_scan_labels(
    roots: &[QueryRoot],
) -> Result<Option<Vec<QueryRoot>>, PerComputationCandidateError> {
    let mut nodes = Vec::new();
    let mut seen = HashSet::new();
    for node in roots.iter().flat_map(root_operators) {
        collect_children_first(node, &mut seen, &mut nodes);
    }
    // Each metric's union schema, `None` once two of its scans conflict.
    let mut unions: Vec<(&Source, Option<Schema>)> = Vec::new();
    for node in &nodes {
        let Some((source, schema)) = unfiltered_series_scan(node) else {
            continue;
        };
        let Some(i) = unions.iter().position(|(s, _)| *s == source) else {
            unions.push((source, Some(schema.clone())));
            continue;
        };
        let Some(union) = unions[i].1.as_mut() else {
            continue;
        };
        // A scan known to hold every label and one that is not cannot share.
        if union.closed != schema.closed {
            unions[i].1 = None;
            continue;
        }
        for field in &schema.fields {
            match union.fields.iter().find(|f| f.name == field.name) {
                Some(known) if known == field => {}
                Some(_) => {
                    unions[i].1 = None;
                    break;
                }
                None => {
                    let at = union
                        .fields
                        .iter()
                        .position(|f| f.name == PROMQL_SERIES_IDENTITY)
                        .unwrap_or(union.fields.len());
                    union.fields.insert(at, field.clone());
                    // Unique keys (the series identity and ts) name columns
                    // by position.
                    for column in union.unique_keys.iter_mut().flatten() {
                        if *column >= at {
                            *column += 1;
                        }
                    }
                }
            }
        }
    }
    let union_of = |node: &OperatorNode| {
        let (source, schema) = unfiltered_series_scan(node)?;
        let (_, union) = unions.iter().find(|(s, _)| *s == source)?;
        union.as_ref().filter(|union| *union != schema)
    };
    if !nodes.iter().any(|node| union_of(node).is_some()) {
        return Ok(None);
    }
    let mut widened: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    for node in &nodes {
        let out = match (union_of(node), node.non_asap()) {
            (Some(union), Some(NonASAPOp::Scan { source, .. })) => {
                let mut schema = union.clone();
                schema.time_index = schema.fields.iter().position(|f| f.name == "ts");
                OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
                    source: source.clone(),
                    predicates: Vec::new(),
                    schema,
                }))?
            }
            _ if node
                .children()
                .iter()
                .all(|c| Rc::ptr_eq(c, &widened[&Rc::as_ptr(c)])) =>
            {
                Rc::clone(node)
            }
            // A node whose renamed outputs would change arity is not widened.
            _ if passes_labels_on(node) => {
                match node.with_new_children(|c| Rc::clone(&widened[&Rc::as_ptr(c)])) {
                    Ok(rebuilt) => Rc::new(rebuilt),
                    Err(_) => return Ok(None),
                }
            }
            // A grouped or filtered pane chain over a `Project` would hide
            // its window from plan selection.
            _ if matches!(node.asap(), Some(ASAPOp::PaneChain { .. })) => {
                return Ok(None);
            }
            _ => {
                let mut children = HashMap::new();
                for child in node.children() {
                    let wide = &widened[&Rc::as_ptr(child)];
                    let Some(narrow) = narrowed(wide, &child.schema)? else {
                        return Ok(None);
                    };
                    children.insert(Rc::as_ptr(child), narrow);
                }
                Rc::new(node.with_new_children(|c| Rc::clone(&children[&Rc::as_ptr(c)]))?)
            }
        };
        widened.insert(Rc::as_ptr(node), out);
    }
    let mut out = Vec::with_capacity(roots.len());
    for query_root in roots {
        let mut mapped = HashMap::new();
        for node in root_operators(query_root) {
            let Some(narrow) = narrowed(&widened[&Rc::as_ptr(node)], &node.schema)? else {
                return Ok(None);
            };
            mapped.insert(Rc::as_ptr(node), narrow);
        }
        out.push(match query_root {
            QueryRoot::Operator(node) => QueryRoot::Operator(Rc::clone(&mapped[&Rc::as_ptr(node)])),
            QueryRoot::Scalar(expr) => QueryRoot::Scalar(
                expr.map_operator_refs(&mut |node| Rc::clone(&mapped[&Rc::as_ptr(node)])),
            ),
        });
    }
    recompose_guarantees(&out).map(Some)
}

/// The source and schema of a time-series `Scan` without predicates.
fn unfiltered_series_scan(node: &OperatorNode) -> Option<(&Source, &Schema)> {
    match node.non_asap() {
        Some(NonASAPOp::Scan {
            source: source @ Source::TimeSeries { .. },
            predicates,
            schema,
        }) if predicates.is_empty() => Some((source, schema)),
        _ => None,
    }
}

/// Whether `node`, over a child with more labels, keeps them in its own
/// output: a time range or shift of the rows, and a per-series build or
/// pane chain without a filter (keyed by the series identity, which
/// determines every label) with its merges, estimates and finalizations.
/// The executor reads a per-series build's raw rows from its `TimeRange`
/// directly, and plan selection reads a pane chain's window from the rows
/// below it, so nothing may sit between them.
fn passes_labels_on(node: &OperatorNode) -> bool {
    matches!(
        node.non_asap(),
        Some(NonASAPOp::TimeRange { .. } | NonASAPOp::TimeShift { .. })
    ) || matches!(
        node.asap(),
        Some(
            ASAPOp::SummaryAgg {
                reduction: Reduction::PerEntity,
                filter: None,
                ..
            } | ASAPOp::PaneChain {
                reduction: Reduction::PerEntity,
                filter: None,
                ..
            } | ASAPOp::SummaryMerge { .. }
                | ASAPOp::SummaryEstimate { .. }
                | ASAPOp::FinalizeExactAccumulator { .. }
        )
    )
}

/// `wide`, which holds every column of `schema` and perhaps more, with
/// only `schema`'s columns, by name: `wide` itself when it has exactly
/// them; `None` when `wide` is a summary state, which has no columns to
/// project.
fn narrowed(
    wide: &Rc<OperatorNode>,
    schema: &Schema,
) -> Result<Option<Rc<OperatorNode>>, PerComputationCandidateError> {
    use asap_types::ir::ProjectItem;
    if wide.schema.fields == schema.fields {
        return Ok(Some(Rc::clone(wide)));
    }
    if wide.asap().is_some_and(|op| op.produced_state().is_some()) {
        return Ok(None);
    }
    let cols = schema
        .fields
        .iter()
        .map(|field| {
            let column = wide
                .schema
                .fields
                .iter()
                .position(|f| f.name == field.name && f.table == field.table)
                .expect("a widened node keeps every column it had");
            ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(column),
            }
        })
        .collect();
    Ok(Some(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: Rc::clone(wide),
        },
    ))?))
}

/// Every Pass 2 candidate of the identical-expression rule for the
/// candidate with query roots `roots` and sharing candidates `candidates`
/// ([`sharing_candidates`]): one per non-empty subset of them that gives a
/// distinct Pass 2 candidate, in binary-counter order of the subsets (the
/// first sharing candidate the lowest bit). Empty when nothing is
/// identical.
///
/// Sharing a set of sharing candidates also shares every one whose
/// remaining nodes it leaves as one; that closure maps each subset to the
/// Pass 2 candidate it gives, and the distinct ones are the closed sets.
/// They are enumerated directly (Ganter's Next Closure, one closure per
/// step and sharing candidate) rather than by trying all `2^g` subsets,
/// which repeat when sharing candidates nest.
pub fn identical_expression_pass2_candidates(
    roots: &[QueryRoot],
    candidates: &[SharingCandidate],
) -> Vec<IdenticalExpressionPass2Candidate> {
    let n = candidates.len();
    let mut closed_sets = Vec::new();
    // The closed sets in lectic order, from the closure of the empty set
    // (which shares nothing).
    let mut closed = vec![false; n];
    'next: loop {
        for i in (0..n).rev() {
            if closed[i] {
                continue;
            }
            let mut candidate: Vec<bool> = (0..n).map(|g| g < i && closed[g]).collect();
            candidate[i] = true;
            let (shared_roots, merged) = apply(roots, candidates, &candidate);
            // Lectic successor: adds nothing below `i` that `closed` lacks.
            if (0..i).all(|g| !merged[g] || closed[g]) {
                // `merged` is closed: applying it gives the same candidate.
                closed_sets.push((merged.clone(), shared_roots));
                closed = merged;
                continue 'next;
            }
        }
        break;
    }
    // Binary-counter order of the subsets: compare from the highest candidate.
    closed_sets.sort_by(|(a, _), (b, _)| a.iter().rev().cmp(b.iter().rev()));
    closed_sets
        .into_iter()
        .map(|(subset, roots)| IdenticalExpressionPass2Candidate {
            sharing_candidates: (0..n)
                .filter(|&g| subset[g])
                .map(|g| candidates[g].clone())
                .collect(),
            roots,
        })
        .collect()
}

/// `roots` with the sharing candidates in `subset` shared, and the ones
/// that leaves shared: those whose every node still read became one node (a
/// node below a shared node other than the one kept is no longer read).
fn apply(
    roots: &[QueryRoot],
    candidates: &[SharingCandidate],
    subset: &[bool],
) -> (Vec<QueryRoot>, Vec<bool>) {
    let candidate_of: HashMap<*const OperatorNode, usize> = candidates
        .iter()
        .enumerate()
        .flat_map(|(index, candidate)| candidate.nodes.iter().map(move |n| (Rc::as_ptr(n), index)))
        .collect();
    let mut sharing = Sharing {
        subset,
        candidate_of: &candidate_of,
        rebuilt: HashMap::new(),
        node_of_shared_candidate: HashMap::new(),
    };
    let shared_roots: Vec<QueryRoot> = roots.iter().map(|r| sharing.root(r)).collect();
    let merged = candidates
        .iter()
        .map(|candidate| {
            let mut nodes = candidate.nodes.iter().filter_map(|node| {
                let (_, rebuilt) = sharing.rebuilt.get(&Rc::as_ptr(node))?;
                Some(Rc::as_ptr(rebuilt))
            });
            let first = nodes.next();
            nodes.all(|node| Some(node) == first)
        })
        .collect();
    (shared_roots, merged)
}

/// Binary increment of `subset`, the first entry the lowest bit; `false`
/// once every subset has been visited.
#[cfg(test)]
fn next_subset(subset: &mut [bool]) -> bool {
    for bit in subset.iter_mut() {
        *bit = !*bit;
        if *bit {
            return true;
        }
    }
    false
}

/// One application of the rule: the sharing candidates in `subset` shared.
struct Sharing<'a> {
    subset: &'a [bool],
    /// Node → the index of the sharing candidate it belongs to.
    candidate_of: &'a HashMap<*const OperatorNode, usize>,
    /// Original node → (original, rebuilt). The original is kept alive so
    /// its address cannot be reused while it is a key.
    rebuilt: HashMap<*const OperatorNode, (Rc<OperatorNode>, Rc<OperatorNode>)>,
    /// Shared candidate's index → the one node all its nodes become.
    node_of_shared_candidate: HashMap<usize, Rc<OperatorNode>>,
}

impl Sharing<'_> {
    fn root(&mut self, root: &QueryRoot) -> QueryRoot {
        match root {
            QueryRoot::Operator(node) => QueryRoot::Operator(self.node(node)),
            QueryRoot::Scalar(expr) => {
                QueryRoot::Scalar(expr.map_operator_refs(&mut |node| self.node(node)))
            }
        }
    }

    fn node(&mut self, node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        if let Some((_, rebuilt)) = self.rebuilt.get(&Rc::as_ptr(node)) {
            return Rc::clone(rebuilt);
        }
        let candidate = self
            .candidate_of
            .get(&Rc::as_ptr(node))
            .copied()
            .filter(|&c| self.subset[c]);
        let rebuilt = match candidate.and_then(|c| self.node_of_shared_candidate.get(&c)) {
            Some(shared) => Rc::clone(shared),
            None => {
                let rebuilt = self.over_rebuilt_children(node);
                if let Some(c) = candidate {
                    self.node_of_shared_candidate.insert(c, Rc::clone(&rebuilt));
                }
                rebuilt
            }
        };
        self.rebuilt
            .insert(Rc::as_ptr(node), (Rc::clone(node), Rc::clone(&rebuilt)));
        rebuilt
    }

    /// `node` over its rebuilt children; the same `Rc` when none changed.
    /// Every child is replaced by an identical node, so `node`'s schema,
    /// guarantee, timing and materialization stay valid.
    fn over_rebuilt_children(&mut self, node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        let mut changed = false;
        let operator = node.operator.map_children(|child| {
            let rebuilt = self.node(child);
            changed |= !Rc::ptr_eq(&rebuilt, child);
            rebuilt
        });
        if !changed {
            return Rc::clone(node);
        }
        Rc::new(
            OperatorNode::with_schema(operator, node.schema.clone())
                .with_guarantee(node.guarantee.clone())
                .with_timing(node.timing)
                .with_materialization(node.materialization),
        )
    }
}

/// Summary builds the summary-capability rule can make one build: builds
/// of one family over the same input data that differ only in size, or in
/// the labels their per-series scans project, in the order a children-first
/// walk of the roots meets them.
#[derive(Debug, Clone)]
pub struct CapabilitySharingCandidate {
    /// The family the first member builds: every member builds this sketch
    /// algorithm at some size, or this exact accumulator.
    pub family: FieldDataType,
    /// The summary builds (`SummaryAgg`), at least two, not all equal (in
    /// size or in the labels their scans project).
    pub members: Vec<Rc<OperatorNode>>,
}

/// A Pass 2 candidate of the summary-capability rule: the candidate's
/// roots with each set of builds in `shared_builds` made one build, sized
/// for all their readers, and every guarantee composed again; the error
/// when that composition fails, which leaves the other Pass 2 candidates
/// unaffected.
#[derive(Debug)]
pub struct SummaryCapabilityPass2Candidate {
    /// Each set of summary builds made one build: two or more members of
    /// one [`CapabilitySharingCandidate`], at most one set per candidate.
    pub shared_builds: Vec<Vec<Rc<OperatorNode>>>,
    /// The candidate's roots with those builds shared.
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
}

/// The capability sharing candidates of the candidate with query roots
/// `roots`: at least two builds each, whose sizes are not all equal.
pub fn capability_sharing_candidates(roots: &[QueryRoot]) -> Vec<CapabilitySharingCandidate> {
    let mut nodes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for node in roots.iter().flat_map(root_operators) {
        collect_children_first(node, &mut seen, &mut nodes);
    }
    // Every node that reads each build, by the build's address.
    let mut readers: HashMap<*const OperatorNode, Vec<&Rc<OperatorNode>>> = HashMap::new();
    for node in &nodes {
        for child in node.children() {
            readers.entry(Rc::as_ptr(child)).or_default().push(node);
        }
    }
    let mut candidates: Vec<CapabilitySharingCandidate> = Vec::new();
    for node in &nodes {
        let Some(family) = shareable_family(node, readers.get(&Rc::as_ptr(node))) else {
            continue;
        };
        match candidates
            .iter_mut()
            .find(|c| same_family(&c.family, &family) && same_input_data(&c.members[0], node))
        {
            Some(candidate) => candidate.members.push(Rc::clone(node)),
            None => candidates.push(CapabilitySharingCandidate {
                family,
                members: vec![Rc::clone(node)],
            }),
        }
    }
    candidates.retain(|c| !all_equal(c.members.iter()));
    candidates
}

fn collect_children_first(
    node: &Rc<OperatorNode>,
    seen: &mut std::collections::HashSet<*const OperatorNode>,
    out: &mut Vec<Rc<OperatorNode>>,
) {
    if !seen.insert(Rc::as_ptr(node)) {
        return;
    }
    for child in node.children() {
        collect_children_first(child, seen, out);
    }
    out.push(Rc::clone(node));
}

/// The family of a per-subpopulation sketch build whose every reader is an
/// estimate the family answers, or of a per-series exact accumulator whose
/// every reader finalizes it; `None` for any other node.
fn shareable_family(
    node: &OperatorNode,
    readers: Option<&Vec<&Rc<OperatorNode>>>,
) -> Option<FieldDataType> {
    let readers = readers?;
    match node.asap() {
        Some(ASAPOp::SummaryAgg {
            family: family @ FieldDataType::Sketch(_, GroupingStrategy::PerSubpopulationInstance),
            ..
        }) => {
            let capability = FamilyCapability::of(family);
            readers
                .iter()
                .all(|reader| {
                    matches!(
                        reader.asap(),
                        Some(ASAPOp::SummaryEstimate { query, .. }) if capability.answers(query)
                    )
                })
                .then(|| family.clone())
        }
        Some(ASAPOp::SummaryAgg {
            family: family @ FieldDataType::ExactAggregate(..),
            reduction: Reduction::PerEntity,
            ..
        }) => readers
            .iter()
            .all(|reader| matches!(reader.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. })))
            .then(|| family.clone()),
        _ => None,
    }
}

/// Whether builds of families `a` and `b` can be one build: one sketch
/// algorithm and grouping at any sizes, or one exact accumulator.
fn same_family(a: &FieldDataType, b: &FieldDataType) -> bool {
    match (a, b) {
        (FieldDataType::Sketch(a, grouping), FieldDataType::Sketch(b, grouping_b)) => {
            a.algorithm() == b.algorithm() && grouping == grouping_b
        }
        _ => a == b,
    }
}

/// Whether two builds of one family read the same data the same way: the
/// same input sub-DAG, item and weight, grouping and filter. Two
/// per-series builds over one metric's samples also do when their scans
/// project different labels ([`same_rows`]): a per-series state is keyed by
/// the series identity, which determines every label, so a build over the
/// scan with the most labels holds every member's states and its readers
/// keep the labels each needs ([`share_summaries`]).
fn same_input_data(a: &OperatorNode, b: &OperatorNode) -> bool {
    match (a.asap(), b.asap()) {
        (
            Some(ASAPOp::SummaryAgg {
                child,
                input,
                reduction,
                grouping,
                filter,
                ..
            }),
            Some(ASAPOp::SummaryAgg {
                child: child_b,
                input: input_b,
                reduction: reduction_b,
                grouping: grouping_b,
                filter: filter_b,
                ..
            }),
        ) => {
            let same_child = Rc::ptr_eq(child, child_b)
                || child == child_b
                || (*reduction == Reduction::PerEntity
                    && filter.is_none()
                    && child.schema.has_promql_series_identity()
                    && child_b.schema.has_promql_series_identity()
                    && same_rows(child, child_b));
            same_child
                && input == input_b
                && reduction == reduction_b
                && grouping == grouping_b
                && filter == filter_b
        }
        _ => false,
    }
}

/// The names of the columns of a build's input: for a per-series build,
/// the labels its scan projects, among others.
fn input_column_names(build: &OperatorNode) -> Vec<&str> {
    match build.asap() {
        Some(ASAPOp::SummaryAgg { child, .. }) => child
            .schema
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect(),
        _ => Vec::new(),
    }
}

fn all_equal<'a>(mut nodes: impl Iterator<Item = &'a Rc<OperatorNode>>) -> bool {
    let Some(first) = nodes.next() else {
        return true;
    };
    nodes.all(|node| Rc::ptr_eq(node, first) || **node == **first)
}

/// The most summary builds one capability sharing candidate may have:
/// every subset of two or more is a Pass 2 candidate, about a million at 20.
pub const MAX_CAPABILITY_SHARING_MEMBERS: usize = 20;

/// Every Pass 2 candidate of the summary-capability rule for the candidate
/// with query roots `roots` and capability sharing candidates `candidates`
/// ([`capability_sharing_candidates`]): one per choice, for each, of no
/// sharing or one subset of two or more members whose sizes are not all
/// equal, other than sharing nothing. An error only for one of more than
/// [`MAX_CAPABILITY_SHARING_MEMBERS`] members; a Pass 2 candidate whose
/// composition fails carries its own error. The first capability sharing
/// candidate varies slowest; within one, subsets come in binary-counter
/// order (the first member the lowest bit).
pub fn summary_capability_pass2_candidates(
    roots: &[QueryRoot],
    candidates: &[CapabilitySharingCandidate],
) -> Result<Vec<SummaryCapabilityPass2Candidate>, PerComputationCandidateError> {
    // Every subset of a capability sharing candidate is a Pass 2
    // candidate: 2^n of them. Past MAX_CAPABILITY_SHARING_MEMBERS that is
    // no longer a list plan selection can price.
    let largest = candidates
        .iter()
        .map(|candidate| candidate.members.len())
        .max()
        .unwrap_or(0);
    if largest > MAX_CAPABILITY_SHARING_MEMBERS {
        return Err(PerComputationCandidateError::TooManyCandidates {
            counted: "summary builds in one capability sharing candidate",
            found: largest,
            max: MAX_CAPABILITY_SHARING_MEMBERS,
        });
    }
    // Each capability sharing candidate's options: `None` (no sharing),
    // then its subsets.
    let options: Vec<Vec<Option<Vec<usize>>>> = candidates
        .iter()
        .map(|candidate| {
            let n = candidate.members.len();
            let mut options = vec![None];
            for mask in 1u64..(1u64 << n) {
                let members: Vec<usize> = (0..n).filter(|&m| mask >> m & 1 == 1).collect();
                if members.len() >= 2 && !all_equal(members.iter().map(|&m| &candidate.members[m]))
                {
                    options.push(Some(members));
                }
            }
            options
        })
        .collect();
    let mut choice = vec![0usize; candidates.len()];
    let mut pass2_candidates = Vec::new();
    loop {
        // Mixed-radix increment, the last candidate varying fastest.
        let mut carried = true;
        for (digit, candidate_options) in choice.iter_mut().zip(&options).rev() {
            *digit += 1;
            if *digit < candidate_options.len() {
                carried = false;
                break;
            }
            *digit = 0;
        }
        if carried {
            break;
        }
        let shared_builds: Vec<Vec<Rc<OperatorNode>>> = choice
            .iter()
            .zip(candidates.iter().zip(&options))
            .filter_map(|(&option, (candidate, candidate_options))| {
                let members = candidate_options[option].as_ref()?;
                Some(
                    members
                        .iter()
                        .map(|&m| Rc::clone(&candidate.members[m]))
                        .collect(),
                )
            })
            .collect();
        match share_summaries(roots, &shared_builds) {
            Ok(None) => {}
            Ok(Some(roots)) => pass2_candidates.push(SummaryCapabilityPass2Candidate {
                shared_builds,
                roots: Ok(roots),
            }),
            Err(error) => pass2_candidates.push(SummaryCapabilityPass2Candidate {
                shared_builds,
                roots: Err(error),
            }),
        }
    }
    Ok(pass2_candidates)
}

/// `roots` with each set of `shared_builds` made one build sized for all
/// its members, and every guarantee composed again; `None` when the
/// members' sizes have no shared size.
fn share_summaries(
    roots: &[QueryRoot],
    shared_builds: &[Vec<Rc<OperatorNode>>],
) -> Result<Option<Vec<QueryRoot>>, PerComputationCandidateError> {
    let mut replaced: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    // Every node that reads each build, by the build's address.
    let mut nodes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for node in roots.iter().flat_map(root_operators) {
        collect_children_first(node, &mut seen, &mut nodes);
    }
    let mut readers: HashMap<*const OperatorNode, Vec<&Rc<OperatorNode>>> = HashMap::new();
    for node in &nodes {
        for child in node.children() {
            readers.entry(Rc::as_ptr(child)).or_default().push(node);
        }
    }
    for members in shared_builds {
        let mut members: Vec<&Rc<OperatorNode>> = members.iter().collect();
        // The build is over the input with every member's columns (the
        // scan with the most labels); none such, no shared build.
        let Some(host) = members.iter().position(|candidate| {
            let columns = input_column_names(candidate);
            members.iter().all(|member| {
                input_column_names(member)
                    .iter()
                    .all(|c| columns.contains(c))
            })
        }) else {
            return Ok(None);
        };
        members.swap(0, host);
        let Some(build) = sized_for_all(&members)? else {
            return Ok(None);
        };
        for member in members {
            if member.schema == build.schema {
                replaced.insert(Rc::as_ptr(member), Rc::clone(&build));
                continue;
            }
            // A member over fewer labels: each of its estimates reads the
            // shared build and keeps the member's columns, by name.
            for reader in readers.get(&Rc::as_ptr(member)).into_iter().flatten() {
                let Some(estimate) = relabeled_estimate(reader, &build)? else {
                    return Ok(None);
                };
                replaced.insert(Rc::as_ptr(reader), estimate);
            }
        }
    }
    let mut rebuilt: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    let mut rebuild = |node: &Rc<OperatorNode>| rebuild(node, &replaced, &mut rebuilt);
    let mut out = Vec::new();
    for root in roots {
        out.push(match root {
            QueryRoot::Operator(node) => QueryRoot::Operator(rebuild(node)?),
            QueryRoot::Scalar(expr) => {
                let mut nodes = HashMap::new();
                for node in expr.operator_refs() {
                    nodes.insert(Rc::as_ptr(node), rebuild(node)?);
                }
                QueryRoot::Scalar(
                    expr.map_operator_refs(&mut |node| nodes[&Rc::as_ptr(node)].clone()),
                )
            }
        });
    }
    Ok(Some(recompose_guarantees(&out)?))
}

/// `reader`, an estimate or finalization of a per-series build over fewer
/// labels, as the same read of `build` with only `reader`'s columns, by
/// name; `None` for another reader or a missing column.
fn relabeled_estimate(
    reader: &OperatorNode,
    build: &Rc<OperatorNode>,
) -> Result<Option<Rc<OperatorNode>>, PerComputationCandidateError> {
    use asap_types::ir::ProjectItem;
    let operator = match reader.asap() {
        Some(ASAPOp::SummaryEstimate { query, .. }) => ASAPOp::SummaryEstimate {
            summary_input: Rc::clone(build),
            query: query.clone(),
        },
        Some(ASAPOp::FinalizeExactAccumulator { .. }) => ASAPOp::FinalizeExactAccumulator {
            child: Rc::clone(build),
        },
        _ => return Ok(None),
    };
    let estimate = Rc::new(OperatorNode::new(Operator::ASAP(operator))?);
    let mut cols = Vec::with_capacity(reader.schema.fields.len());
    for field in &reader.schema.fields {
        let Some(column) = estimate
            .schema
            .fields
            .iter()
            .position(|f| f.name == field.name && f.table == field.table)
        else {
            return Ok(None);
        };
        cols.push(ProjectItem {
            alias: Some(field.name.clone()),
            expr: ScalarExpr::Column(column),
        });
    }
    Ok(Some(Rc::new(OperatorNode::new(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: estimate,
        },
    ))?)))
}

/// One build serving every member: the first member with the members'
/// shared size ([`size_for_all_readers`]).
fn sized_for_all(
    members: &[&Rc<OperatorNode>],
) -> Result<Option<Rc<OperatorNode>>, PerComputationCandidateError> {
    // An exact accumulator has one size: the first member serves them all.
    if matches!(
        members[0].asap(),
        Some(ASAPOp::SummaryAgg {
            family: FieldDataType::ExactAggregate(..),
            ..
        })
    ) {
        return Ok(Some(Rc::clone(members[0])));
    }
    let kinds: Vec<&SketchKind> = members
        .iter()
        .filter_map(|member| match member.asap() {
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }) => Some(kind),
            _ => None,
        })
        .collect();
    let params: Vec<_> = kinds.iter().map(|kind| kind.params()).collect();
    let Some(params) = size_for_all_readers(&params) else {
        return Ok(None);
    };
    let first = members[0];
    let Some(ASAPOp::SummaryAgg {
        child,
        family: FieldDataType::Sketch(kind, grouping_strategy),
        input,
        reduction,
        grouping,
        filter,
    }) = first.asap()
    else {
        return Ok(None);
    };
    let operator = Operator::ASAP(ASAPOp::SummaryAgg {
        child: Rc::clone(child),
        family: FieldDataType::Sketch(
            SketchKind::new(kind.algorithm().clone(), params),
            grouping_strategy.clone(),
        ),
        input: input.clone(),
        reduction: reduction.clone(),
        grouping: grouping.clone(),
        filter: filter.clone(),
    });
    let mut build = OperatorNode::new(operator)?;
    // Keep the member's explicit output names.
    for (field, retained) in build.schema.fields.iter_mut().zip(&first.schema.fields) {
        field.name = retained.name.clone();
        field.table = retained.table.clone();
    }
    Ok(Some(Rc::new(build)))
}

/// One summary build the window-composition rule can build as tumbling
/// panes: a `SummaryAgg` over a range `TimeRange(W)` whose family merges
/// (the family-capability table: exact accumulators included, and families
/// whose merged readouts are only approximate), read only by queries that
/// repeat at a fixed interval. Its panes are `pane_count = W / w` back-to-back
/// windows of `w = gcd(W, E)` over the queries' intervals `E` (and, for a
/// query with a range evaluation, its step), at least two.
#[derive(Debug, Clone)]
pub struct PaneCandidate {
    /// The `SummaryAgg` over `TimeRange(W)` built as panes.
    pub build: Rc<OperatorNode>,
    /// The width `w` of one pane.
    pub pane_width_ms: u64,
    /// The number of panes, `W / w`, at least two.
    pub pane_count: u32,
}

impl PaneCandidate {
    /// The window `W` its panes cover, `pane_width_ms · pane_count`.
    pub fn window_ms(&self) -> u64 {
        self.pane_width_ms * u64::from(self.pane_count)
    }
}

/// A Pass 2 candidate of the window-composition rule: the candidate's roots
/// with the build of each of `pane_candidates` built as its tumbling panes,
/// merged before every read, and every guarantee composed again; the error
/// when that fails, which leaves the other Pass 2 candidates unaffected.
#[derive(Debug)]
pub struct WindowCompositionPass2Candidate {
    /// The builds built as tumbling panes, in [`pane_candidates`] order.
    pub pane_candidates: Vec<PaneCandidate>,
    /// The chains each shared by several of `pane_candidates`
    /// ([`nested_window_groups`] or [`gcd_window_groups`]); empty when each
    /// build has its own chain.
    pub shared_chains: Vec<SharedPaneChain>,
    /// The candidate's roots with those builds in panes.
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
}

/// One pane chain several pane candidates read (window-composition rule):
/// panes of `pane_width_ms`, a width that divides every member's own pane
/// width, and `pane_count` of them, covering the longest member's window.
/// Each member merges the newest `W / pane_width_ms` panes of its window `W`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedPaneChain {
    /// Positions in the Pass 2 candidate's `pane_candidates`, two or more.
    pub members: Vec<usize>,
    /// The width of one pane of the shared chain.
    pub pane_width_ms: u64,
    /// The number of panes of the shared chain.
    pub pane_count: u32,
}

/// The most pane candidates one candidate may have: every non-empty subset
/// is a Pass 2 candidate, and with shared grids up to three, about three
/// million at 20.
pub const MAX_PANE_CANDIDATES: usize = 20;

/// The pane candidates of the candidate with query roots `roots`, children
/// first. `evaluation_grid_ms[q]` is the grid query `q`'s evaluations fall
/// on: how often it repeats, or with a range evaluation the greatest common
/// divisor of that and its step, and of its phase if declared
/// (`QueryWorkloadEntry::evaluation_grid_ms`); `None` when it does not
/// repeat at a fixed interval. Each pane width divides every reader's grid,
/// so with panes anchored at the epoch every evaluation, every range step
/// included, ends on a pane boundary.
pub fn pane_candidates(
    roots: &[QueryRoot],
    evaluation_grid_ms: &[Option<u64>],
) -> Vec<PaneCandidate> {
    let mut nodes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for node in roots.iter().flat_map(root_operators) {
        collect_children_first(node, &mut seen, &mut nodes);
    }
    let reach: Vec<std::collections::HashSet<*const OperatorNode>> = roots
        .iter()
        .map(|root| {
            root_operators(root)
                .into_iter()
                .flat_map(OperatorNode::reachable)
                .map(|node| Rc::as_ptr(&node))
                .collect()
        })
        .collect();
    nodes
        .into_iter()
        .filter_map(|build| {
            let window_ms = pane_window_ms(&build)?;
            let readers: Vec<usize> = (0..roots.len())
                .filter(|&q| reach[q].contains(&Rc::as_ptr(&build)))
                .collect();
            let mut width = window_ms;
            for &q in &readers {
                width = asap_types::workload::greatest_common_divisor(
                    width,
                    evaluation_grid_ms.get(q).copied().flatten()?,
                );
            }
            let pane_count = u32::try_from(window_ms / width).ok()?;
            (!readers.is_empty() && pane_count >= 2).then_some(PaneCandidate {
                build,
                pane_width_ms: width,
                pane_count,
            })
        })
        .collect()
}

/// The window `W` of a build the rule can split: a `SummaryAgg` whose
/// family merges, over a range `TimeRange(W)` over an instant vector or a
/// SQL table's rows, read at the query's time or a fixed offset from it: no `@` anchor on any of
/// the time shifts stacked below the range.
fn pane_window_ms(build: &OperatorNode) -> Option<u64> {
    let Some(ASAPOp::SummaryAgg { child, family, .. }) = build.asap() else {
        return None;
    };
    FamilyCapability::of(family).merge_inputs?;
    let Some(NonASAPOp::TimeRange {
        range,
        kind: TimeRangeKind::Range,
        child: input,
    }) = child.non_asap()
    else {
        return None;
    };
    // An instant vector (PromQL `m[w]`) or a SQL table's rows (a trailing
    // window); not a range vector, whose rows are already windows.
    if !matches!(
        input.result_kind,
        OperatorResultKind::InstantVector | OperatorResultKind::Relation
    ) {
        return None;
    }
    // Refuse an `@` anchor on any of the shifts stacked below the range.
    let mut below = input;
    while let Some(NonASAPOp::TimeShift { shift, child }) = below.non_asap() {
        if shift.at.is_some() {
            return None;
        }
        below = child;
    }
    u64::try_from(range.as_millis()).ok().filter(|&ms| ms > 0)
}

/// Every Pass 2 candidate of the window-composition rule for the candidate
/// with query roots `roots` and pane candidates `candidates`
/// ([`pane_candidates`]): one per non-empty subset of them, in
/// binary-counter order (the first candidate the lowest bit). Each build in
/// the subset becomes a `SummaryMerge` over one [`ASAPOp::PaneChain`] of
/// its panes; every other build is untouched. A subset whose builds can
/// share chains is followed by one Pass 2 candidate per shared grid, in
/// which each group reads one shared chain, each member merging its newest
/// panes: the finest member's panes ([`nested_window_groups`]), then the
/// gcd of the members' widths when that groups them differently
/// ([`gcd_window_groups`]); and by one error candidate per group whose
/// sizing fails. Each grid groups every build of a subset at once: a
/// shared gcd grid over only some of the builds that could share it is not
/// listed. An error only for more than
/// [`MAX_PANE_CANDIDATES`] pane candidates; a Pass 2 candidate whose
/// composition fails carries its own error.
pub fn window_composition_pass2_candidates(
    roots: &[QueryRoot],
    candidates: &[PaneCandidate],
) -> Result<Vec<WindowCompositionPass2Candidate>, PerComputationCandidateError> {
    if candidates.len() > MAX_PANE_CANDIDATES {
        return Err(PerComputationCandidateError::TooManyCandidates {
            counted: "summary builds that could be built in tumbling panes",
            found: candidates.len(),
            max: MAX_PANE_CANDIDATES,
        });
    }
    let rollable = rollable_pane_candidates(roots, candidates);
    let mut pass2_candidates = Vec::new();
    for mask in 1u64..(1u64 << candidates.len()) {
        let chosen: Vec<PaneCandidate> = (0..candidates.len())
            .filter(|&i| mask >> i & 1 == 1)
            .map(|i| candidates[i].clone())
            .collect();
        pass2_candidates.push(WindowCompositionPass2Candidate {
            roots: in_panes(roots, &chosen, &[]),
            pane_candidates: chosen.clone(),
            shared_chains: Vec::new(),
        });
        // Shared chains on the members' own finest panes, then, when it
        // groups them differently, on the gcd of their widths.
        let on_member_panes = nested_window_groups(&chosen);
        let on_gcd_panes = gcd_window_groups(&chosen);
        push_shared_chains(roots, &chosen, &on_member_panes, &mut pass2_candidates);
        if on_gcd_panes != on_member_panes {
            push_shared_chains(roots, &chosen, &on_gcd_panes, &mut pass2_candidates);
        }
        // Coarser groupings read a finer grouping's chain, rolled up.
        let chosen_rollable: Vec<bool> = (0..candidates.len())
            .filter(|&i| mask >> i & 1 == 1)
            .map(|i| rollable[i])
            .collect();
        push_shared_chains(
            roots,
            &chosen,
            &rollup_window_groups(&chosen, &chosen_rollable),
            &mut pass2_candidates,
        );
    }
    Ok(pass2_candidates)
}

/// Push the Pass 2 candidate in which each of `groups` reads its one shared
/// chain: the groups whose members have a shared size, and the error of
/// each group whose sizing fails. Nothing when no group has a shared size.
fn push_shared_chains(
    roots: &[QueryRoot],
    chosen: &[PaneCandidate],
    groups: &[SharedPaneChain],
    pass2_candidates: &mut Vec<WindowCompositionPass2Candidate>,
) {
    // Each group with a size that serves every member, and that build.
    let mut shared = Vec::new();
    for group in groups {
        match shared_chain_build(chosen, &group.members) {
            Ok(Some(build)) => shared.push((group.clone(), build)),
            Ok(None) => {}
            Err(error) => pass2_candidates.push(WindowCompositionPass2Candidate {
                roots: Err(error),
                pane_candidates: chosen.to_vec(),
                shared_chains: vec![group.clone()],
            }),
        }
    }
    if !shared.is_empty() {
        pass2_candidates.push(WindowCompositionPass2Candidate {
            roots: in_panes(roots, chosen, &shared),
            pane_candidates: chosen.to_vec(),
            shared_chains: shared.into_iter().map(|(group, _)| group).collect(),
        });
    }
}

/// The pane candidates (positions in `candidates`) that can read one
/// shared chain at the gcd of their pane widths, each group with its
/// chain: every build over one input ([`same_build_over_one_input`]) in one
/// group, whatever its pane width, on panes of the gcd of the members'
/// widths over the longest window. Finer panes than any member builds mean
/// more states and more merges; Stage 3's pricing decides whether sharing
/// pays. Groups of members with the same panes and window are left to the
/// other rules.
fn gcd_window_groups(candidates: &[PaneCandidate]) -> Vec<SharedPaneChain> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, candidate) in candidates.iter().enumerate() {
        match groups
            .iter_mut()
            .find(|group| same_build_over_one_input(&candidates[group[0]], candidate))
        {
            Some(group) => group.push(i),
            None => groups.push(vec![i]),
        }
    }
    groups
        .into_iter()
        .filter_map(|members| {
            let panes = |i: usize| (candidates[i].pane_width_ms, candidates[i].pane_count);
            if members.iter().all(|&i| panes(i) == panes(members[0])) {
                return None;
            }
            let pane_width_ms = members.iter().fold(0, |width, &i| {
                asap_types::workload::greatest_common_divisor(width, candidates[i].pane_width_ms)
            });
            let longest = longest(candidates, &members);
            let pane_count = u32::try_from(candidates[longest].window_ms() / pane_width_ms).ok()?;
            Some(SharedPaneChain {
                members,
                pane_width_ms,
                pane_count,
            })
        })
        .collect()
}

/// The pane candidates (positions in `candidates`) that can read one
/// shared chain, each group with its chain: builds of one family
/// ([`same_family`], at any size) that read the same input below their
/// ranges the same way (item and weight, grouping and filter), with the
/// same output column names (each reader reads the shared chain's columns
/// by position). Their windows end at the same time, so each is the newest
/// panes of the longest. The shared panes are the group's finest member's
/// own panes: a build joins a group only when that width divides its own
/// pane width (the gcd of the widths, never finer than a chain some member
/// builds anyway), and otherwise starts its own group. Members at different
/// repeat intervals read the same panes, each at its own cadence. One chain
/// over the longest window, sized for every reader
/// ([`size_for_all_readers`]), serves them all. Groups of members with the
/// same panes and window are left to the other rules.
fn nested_window_groups(candidates: &[PaneCandidate]) -> Vec<SharedPaneChain> {
    // Finest panes first: each build joins the first group over its input
    // whose (finest) pane width divides its own, so a build whose panes no
    // group's divide starts its own group instead of breaking another.
    let mut by_width: Vec<usize> = (0..candidates.len()).collect();
    by_width.sort_by_key(|&i| candidates[i].pane_width_ms);
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for i in by_width {
        let candidate = &candidates[i];
        match groups.iter_mut().find(|group| {
            let finest = &candidates[group[0]];
            candidate.pane_width_ms.is_multiple_of(finest.pane_width_ms)
                && same_build_over_one_input(finest, candidate)
        }) {
            Some(group) => group.push(i),
            None => groups.push(vec![i]),
        }
    }
    let mut shared = groups
        .into_iter()
        .filter_map(|mut members| {
            let panes = |i: usize| (candidates[i].pane_width_ms, candidates[i].pane_count);
            if members.iter().all(|&i| panes(i) == panes(members[0])) {
                return None;
            }
            let pane_width_ms = candidates[members[0]].pane_width_ms;
            let longest = longest(candidates, &members);
            let pane_count = u32::try_from(candidates[longest].window_ms() / pane_width_ms).ok()?;
            members.sort_unstable();
            Some(SharedPaneChain {
                members,
                pane_width_ms,
                pane_count,
            })
        })
        .collect::<Vec<_>>();
    // In the order of their first members, as [`gcd_window_groups`] lists
    // its groups, so equal groupings compare equal.
    shared.sort_by_key(|group| group.members[0]);
    shared
}

/// Whether each of `candidates` may be read rolled up from a finer
/// grouping's chain: a sketch build over a SQL relation's rows (a PromQL
/// rollup over newest panes is not offered yet) whose every reader in
/// `roots` is an estimate whose merged readout keeps a guarantee, as for
/// the rollup rule.
fn rollable_pane_candidates(roots: &[QueryRoot], candidates: &[PaneCandidate]) -> Vec<bool> {
    let mut nodes = Vec::new();
    let mut seen = HashSet::new();
    for node in roots.iter().flat_map(root_operators) {
        collect_children_first(node, &mut seen, &mut nodes);
    }
    let mut readers: HashMap<*const OperatorNode, Vec<&Rc<OperatorNode>>> = HashMap::new();
    for node in &nodes {
        for child in node.children() {
            readers.entry(Rc::as_ptr(child)).or_default().push(node);
        }
    }
    candidates
        .iter()
        .map(|candidate| match candidate.build.asap() {
            Some(ASAPOp::SummaryAgg {
                child: range,
                family: family @ FieldDataType::Sketch(..),
                ..
            }) => {
                range.children()[0].result_kind == OperatorResultKind::Relation
                    && readers
                        .get(&Rc::as_ptr(&candidate.build))
                        .is_some_and(|readers| read_with_merged_guarantee(family, readers))
            }
            _ => false,
        })
        .collect()
}

/// The pane candidates (positions in `candidates`) that can read one
/// chain of a finer grouping's panes, each group with its chain: a host,
/// and every candidate whose group keys are the host's or fewer
/// ([`rolls_up_into`]), whose window is at most the host's and whose pane
/// width the host's divides. Each member merges the newest panes of its
/// window, and a coarser member then rolls the host's groups up into its
/// own (a regrouping `SummaryMerge`): `p90 per city over 20 minutes` in
/// 10 s panes serves `p90 over 5 minutes` as the newest 30 panes merged
/// across cities. A coarser member is one `rollable[i]` allows
/// ([`rollable_pane_candidates`]). Hosts are tried by most group keys,
/// then longest window, and each candidate joins one group. Only groups
/// with a coarser member are listed; the others are
/// [`nested_window_groups`]'s.
fn rollup_window_groups(candidates: &[PaneCandidate], rollable: &[bool]) -> Vec<SharedPaneChain> {
    let mut hosts: Vec<usize> = (0..candidates.len()).collect();
    hosts.sort_by_key(|&i| {
        (
            std::cmp::Reverse(group_key_count(&candidates[i].build)),
            std::cmp::Reverse(candidates[i].window_ms()),
            i,
        )
    });
    let mut grouped = vec![false; candidates.len()];
    let mut groups = Vec::new();
    for host in hosts {
        if grouped[host] {
            continue;
        }
        let coarser = |i: usize| {
            group_key_count(&candidates[i].build) < group_key_count(&candidates[host].build)
        };
        let reads_host = |i: usize| {
            let (member, chain) = (&candidates[i], &candidates[host]);
            i == host
                || (!grouped[i]
                    && member.window_ms() <= chain.window_ms()
                    && member.pane_width_ms.is_multiple_of(chain.pane_width_ms)
                    && rolls_up_into(member, chain)
                    && (!coarser(i) || rollable[i]))
        };
        let members: Vec<usize> = (0..candidates.len()).filter(|&i| reads_host(i)).collect();
        if !members.iter().any(|&i| coarser(i)) {
            continue;
        }
        for &i in &members {
            grouped[i] = true;
        }
        groups.push(SharedPaneChain {
            members,
            pane_width_ms: candidates[host].pane_width_ms,
            pane_count: candidates[host].pane_count,
        });
    }
    groups.sort_by_key(|group| group.members[0]);
    groups
}

/// The number of group keys of a build grouped by listed keys; 0 for any
/// other build.
fn group_key_count(build: &OperatorNode) -> usize {
    match build.asap() {
        Some(ASAPOp::SummaryAgg {
            reduction: Reduction::Reduce(by),
            ..
        }) if !by.is_without() => by.len(),
        _ => 0,
    }
}

/// Whether `member`'s build is `host`'s ([`same_build_over_one_input`]),
/// or the same but grouped by fewer of `host`'s keys, of a family whose
/// states merge across groups ([`FamilyCapability::merges_groups`]), so a
/// rollup of `host`'s states answers it.
fn rolls_up_into(member: &PaneCandidate, host: &PaneCandidate) -> bool {
    if same_build_over_one_input(member, host) {
        return true;
    }
    let (
        Some(ASAPOp::SummaryAgg {
            child: range,
            family,
            input,
            reduction: Reduction::Reduce(by),
            grouping,
            filter,
        }),
        Some(ASAPOp::SummaryAgg {
            child: host_range,
            family: host_family,
            input: host_input,
            reduction: Reduction::Reduce(host_by),
            grouping: host_grouping,
            filter: host_filter,
        }),
    ) = (member.build.asap(), host.build.asap())
    else {
        return false;
    };
    let (below, host_below) = (range.children()[0], host_range.children()[0]);
    // Both ranges read the same rows below, so a key names one column.
    !by.is_without()
        && !host_by.is_without()
        && by.len() < host_by.len()
        && by.keys().iter().all(|key| host_by.keys().contains(key))
        && FamilyCapability::merges_groups(family)
        && same_family(family, host_family)
        && (Rc::ptr_eq(below, host_below) || below == host_below)
        && input == host_input
        && grouping == host_grouping
        && filter == host_filter
}

/// Whether two pane candidates build one family (at any size) over the
/// same input below their ranges, read the same way, with the same output
/// column names.
fn same_build_over_one_input(a: &PaneCandidate, b: &PaneCandidate) -> bool {
    let (
        Some(ASAPOp::SummaryAgg {
            child: range,
            family,
            input,
            reduction,
            grouping,
            filter,
        }),
        Some(ASAPOp::SummaryAgg {
            child: range_b,
            family: family_b,
            input: input_b,
            reduction: reduction_b,
            grouping: grouping_b,
            filter: filter_b,
        }),
    ) = (a.build.asap(), b.build.asap())
    else {
        return false;
    };
    let (below, below_b) = (range.children()[0], range_b.children()[0]);
    let names = |build: &OperatorNode| -> Vec<(String, Option<String>)> {
        build
            .schema
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.table.clone()))
            .collect()
    };
    names(&a.build) == names(&b.build)
        && same_family(family, family_b)
        && (Rc::ptr_eq(below, below_b) || below == below_b)
        && input == input_b
        && reduction == reduction_b
        && grouping == grouping_b
        && filter == filter_b
}

/// The member of `group` (positions in `candidates`) with the longest
/// window.
fn longest(candidates: &[PaneCandidate], group: &[usize]) -> usize {
    *group
        .iter()
        .max_by_key(|&&i| candidates[i].window_ms())
        .expect("a group has members")
}

/// The build one chain shared by `group` (positions in `candidates`) is
/// made of: the longest member's, sized for every member
/// ([`sized_for_all`]); `None` when their sizes have no shared size.
fn shared_chain_build(
    candidates: &[PaneCandidate],
    group: &[usize],
) -> Result<Option<Rc<OperatorNode>>, PerComputationCandidateError> {
    // Of the longest windows, the finest grouping: a rollup group's host.
    let host = *group
        .iter()
        .max_by_key(|&&i| {
            (
                candidates[i].window_ms(),
                group_key_count(&candidates[i].build),
            )
        })
        .expect("a group has members");
    let members: Vec<&Rc<OperatorNode>> = std::iter::once(host)
        .chain(group.iter().copied().filter(|&i| i != host))
        .map(|i| &candidates[i].build)
        .collect();
    sized_for_all(&members)
}

/// `roots` with the build of each of `candidates` built as its tumbling
/// panes, and every guarantee composed again. The members of each
/// [`SharedPaneChain`] (with its [`shared_chain_build`]) read that one
/// chain: each merges the newest panes covering its own window.
fn in_panes(
    roots: &[QueryRoot],
    candidates: &[PaneCandidate],
    shared_chains: &[(SharedPaneChain, Rc<OperatorNode>)],
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    let mut replaced: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    for (shared, build) in shared_chains {
        let chain = pane_chain(&PaneCandidate {
            build: Rc::clone(build),
            pane_width_ms: shared.pane_width_ms,
            pane_count: shared.pane_count,
        })?;
        for &i in &shared.members {
            let newest = u32::try_from(candidates[i].window_ms() / shared.pane_width_ms)
                .expect("a member's window is at most the chain's");
            let merge = Rc::new(OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![Rc::clone(&chain)],
                newest_panes: (newest < shared.pane_count).then_some(newest),
                group_by: None,
            }))?);
            // A coarser member rolls the chain's groups up into its own.
            let member = &candidates[i].build;
            let read = if group_key_count(member) == group_key_count(build) {
                merge
            } else {
                regrouped(member, &merge).ok_or(PerComputationCandidateError::Unsupported(
                    "a coarser pane build whose keys the shared chain does not hold",
                ))?
            };
            replaced.insert(Rc::as_ptr(member), read);
        }
    }
    for candidate in candidates {
        if let std::collections::hash_map::Entry::Vacant(entry) =
            replaced.entry(Rc::as_ptr(&candidate.build))
        {
            entry.insert(Rc::new(OperatorNode::new(Operator::ASAP(
                ASAPOp::SummaryMerge {
                    children: vec![pane_chain(candidate)?],
                    newest_panes: None,
                    group_by: None,
                },
            ))?));
        }
    }
    let mut rebuilt: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    let mut out = Vec::new();
    for root in roots {
        out.push(match root {
            QueryRoot::Operator(node) => {
                QueryRoot::Operator(rebuild(node, &replaced, &mut rebuilt)?)
            }
            QueryRoot::Scalar(expr) => {
                let mut nodes = HashMap::new();
                for node in expr.operator_refs() {
                    nodes.insert(Rc::as_ptr(node), rebuild(node, &replaced, &mut rebuilt)?);
                }
                QueryRoot::Scalar(
                    expr.map_operator_refs(&mut |node| nodes[&Rc::as_ptr(node)].clone()),
                )
            }
        });
    }
    recompose_guarantees(&out)
}

/// `roots` with the `SummaryMerge` `merge` (over one [`ASAPOp::PaneChain`]
/// of `roots`) reading the newest panes of another chain over the same
/// input: `pane_count` panes of `pane_width_ms` of family `family` (a size
/// that serves every reader of the chain). The merge reads the newest
/// panes covering its own window (its newest panes, or its whole chain),
/// which `pane_width_ms` divides and the new chain covers; every guarantee
/// is composed again.
pub fn with_pane_chain(
    roots: &[QueryRoot],
    merge: &Rc<OperatorNode>,
    family: FieldDataType,
    pane_width_ms: u64,
    pane_count: u32,
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    let Some(ASAPOp::SummaryMerge {
        children,
        newest_panes: own_newest,
        ..
    }) = merge.asap()
    else {
        unreachable!("a merge over a pane chain");
    };
    let own = &children[0];
    let Some(ASAPOp::PaneChain {
        child,
        input,
        reduction,
        grouping,
        filter,
        pane_width_ms: own_width,
        pane_count: own_count,
        ..
    }) = own.asap()
    else {
        unreachable!("a merge over a pane chain");
    };
    // The merge's window: its newest panes, or the whole chain.
    let window_ms = own_width * u64::from(own_newest.unwrap_or(*own_count));
    let newest =
        u32::try_from(window_ms / pane_width_ms).expect("a reader's window is at most the chain's");
    let mut chain = OperatorNode::new(Operator::ASAP(ASAPOp::PaneChain {
        child: Rc::clone(child),
        family,
        input: input.clone(),
        reduction: reduction.clone(),
        grouping: grouping.clone(),
        filter: filter.clone(),
        pane_width_ms,
        pane_count,
    }))?;
    for (field, kept) in chain.schema.fields.iter_mut().zip(&own.schema.fields) {
        field.name = kept.name.clone();
        field.table = kept.table.clone();
    }
    let read = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![Rc::new(chain)],
        newest_panes: (newest < pane_count).then_some(newest),
        group_by: None,
    }))?;
    let replaced = HashMap::from([(Rc::as_ptr(merge), Rc::new(read))]);
    let mut rebuilt = HashMap::new();
    let mut out = Vec::new();
    for root in roots {
        out.push(match root {
            QueryRoot::Operator(node) => {
                QueryRoot::Operator(rebuild(node, &replaced, &mut rebuilt)?)
            }
            QueryRoot::Scalar(expr) => {
                let mut nodes = HashMap::new();
                for node in expr.operator_refs() {
                    nodes.insert(Rc::as_ptr(node), rebuild(node, &replaced, &mut rebuilt)?);
                }
                QueryRoot::Scalar(
                    expr.map_operator_refs(&mut |node| nodes[&Rc::as_ptr(node)].clone()),
                )
            }
        });
    }
    recompose_guarantees(&out)
}

/// `roots` with the summary build `build` (a `SummaryAgg` of `roots`)
/// built at family `family` instead (the same algorithm at another size),
/// its output names kept; every guarantee is composed again.
pub fn with_resized_build(
    roots: &[QueryRoot],
    build: &Rc<OperatorNode>,
    family: FieldDataType,
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    let Some(ASAPOp::SummaryAgg {
        child,
        input,
        reduction,
        grouping,
        filter,
        ..
    }) = build.asap()
    else {
        unreachable!("a summary build");
    };
    let mut resized = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
        child: Rc::clone(child),
        family,
        input: input.clone(),
        reduction: reduction.clone(),
        grouping: grouping.clone(),
        filter: filter.clone(),
    }))?;
    for (field, kept) in resized.schema.fields.iter_mut().zip(&build.schema.fields) {
        field.name = kept.name.clone();
        field.table = kept.table.clone();
    }
    let replaced = HashMap::from([(Rc::as_ptr(build), Rc::new(resized))]);
    let mut rebuilt = HashMap::new();
    let mut out = Vec::new();
    for root in roots {
        out.push(match root {
            QueryRoot::Operator(node) => {
                QueryRoot::Operator(rebuild(node, &replaced, &mut rebuilt)?)
            }
            QueryRoot::Scalar(expr) => {
                let mut nodes = HashMap::new();
                for node in expr.operator_refs() {
                    nodes.insert(Rc::as_ptr(node), rebuild(node, &replaced, &mut rebuilt)?);
                }
                QueryRoot::Scalar(
                    expr.map_operator_refs(&mut |node| nodes[&Rc::as_ptr(node)].clone()),
                )
            }
        });
    }
    recompose_guarantees(&out)
}

/// The [`ASAPOp::PaneChain`] of `candidate`'s panes, with the build's
/// output names.
fn pane_chain(candidate: &PaneCandidate) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(ASAPOp::SummaryAgg {
        child: range,
        family,
        input,
        reduction,
        grouping,
        filter,
    }) = candidate.build.asap()
    else {
        unreachable!("a pane candidate's build is a SummaryAgg");
    };
    let mut chain = OperatorNode::new(Operator::ASAP(ASAPOp::PaneChain {
        child: Rc::clone(range.children()[0]),
        family: family.clone(),
        input: input.clone(),
        reduction: reduction.clone(),
        grouping: grouping.clone(),
        filter: filter.clone(),
        pane_width_ms: candidate.pane_width_ms,
        pane_count: candidate.pane_count,
    }))?;
    for (field, kept) in chain
        .schema
        .fields
        .iter_mut()
        .zip(&candidate.build.schema.fields)
    {
        field.name = kept.name.clone();
        field.table = kept.table.clone();
    }
    Ok(Rc::new(chain))
}

/// `node` with every replaced build below it swapped for its shared build;
/// the same `Rc` when nothing below changed.
fn rebuild(
    node: &Rc<OperatorNode>,
    replaced: &HashMap<*const OperatorNode, Rc<OperatorNode>>,
    rebuilt: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if let Some(done) = rebuilt.get(&Rc::as_ptr(node)) {
        return Ok(Rc::clone(done));
    }
    let out = match replaced.get(&Rc::as_ptr(node)) {
        Some(build) => Rc::clone(build),
        None => {
            let mut changed = false;
            let mut children = HashMap::new();
            for child in node.children() {
                let new = rebuild(child, replaced, rebuilt)?;
                changed |= !Rc::ptr_eq(&new, child);
                children.insert(Rc::as_ptr(child), new);
            }
            if changed {
                Rc::new(node.with_new_children(|child| children[&Rc::as_ptr(child)].clone())?)
            } else {
                Rc::clone(node)
            }
        }
    };
    rebuilt.insert(Rc::as_ptr(node), Rc::clone(&out));
    Ok(out)
}

/// One coarse build the rollup rule can answer from a finer one: the
/// coarse `SummaryAgg`, the finer node it reads, and the node that replaces
/// the coarse value or state.
#[derive(Debug, Clone)]
pub struct RollupCandidate {
    /// The coarse build (`SummaryAgg`). An exact accumulator's every
    /// reader is a `FinalizeExactAccumulator`, which the rollup replaces; a
    /// sketch's every reader is an estimate its family answers, and the
    /// rollup replaces the build itself.
    pub coarse: Rc<OperatorNode>,
    /// For an exact accumulator, the finer build's
    /// `FinalizeExactAccumulator`, whose values the rollup reads; for a
    /// sketch, the finest build of the same algorithm over the same rows
    /// (a `SummaryAgg`), whose states the rollup merges. Both are already
    /// in the candidate.
    pub finer: Rc<OperatorNode>,
    /// For an exact accumulator, the finalized exact accumulator over
    /// `finer`'s values, grouped by the coarse keys, with the coarse value's
    /// column names; for a sketch, the regrouping `SummaryMerge` of
    /// `finer`'s states by the coarse keys, at `finer`'s size (a Pass 2
    /// candidate resizes `finer` for every reader).
    pub rollup: Rc<OperatorNode>,
}

/// A Pass 2 candidate of the rollup rule: the candidate's roots with the
/// coarse build of each of `rollup_candidates` answered from its finer
/// build, and every guarantee composed again; the error when that fails.
#[derive(Debug)]
pub struct RollupPass2Candidate {
    /// The coarse builds rolled up, in [`rollup_candidates`] order.
    pub rollup_candidates: Vec<RollupCandidate>,
    /// The candidate's roots with those builds rolled up and every guarantee
    /// composed again; `Err` when the rebuilt DAG does not derive or
    /// compose, which drops only this candidate.
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
}

/// The most rollup or top-k reuse candidates one candidate may have: every
/// non-empty subset is a Pass 2 candidate, about a million at 20.
pub const MAX_REUSE_CANDIDATES: usize = 20;

/// The exact accumulator that merges finalized values of `family` across
/// disjoint groups, by the family-capability table: a family whose states
/// merge over disjoint (or overlapping) selections into an exact state.
/// `None` for a family without such a merge: counter states merge only
/// consecutive windows, and sketch states have no regrouping merge in the IR.
pub fn rollup_kind(family: &FieldDataType) -> Option<ExactKind> {
    let FieldDataType::ExactAggregate(kind, _) = family else {
        return None;
    };
    // An exact accumulator's merged state is exact; a counter state merges
    // only consecutive windows.
    if FamilyCapability::of(family).merge_inputs? == MergeInputRule::DisjointInTime {
        return None;
    }
    match kind {
        // Counts add up.
        ExactKind::Sum | ExactKind::Count => Some(ExactKind::Sum),
        ExactKind::Min => Some(ExactKind::Min),
        ExactKind::Max => Some(ExactKind::Max),
        ExactKind::Rate | ExactKind::IRate | ExactKind::Increase => None,
    }
}

/// The rollup candidates of the candidate with query roots `roots`, children
/// first: each coarse exact accumulator build read only through its
/// finalized value, with its coarsest finer build of the same family over
/// the same rows that the candidate finalizes; then each coarse sketch build
/// read only by estimates, with the finest build of its algorithm over the
/// same rows whose keys include its own ([`sketch_rollup_candidates`]).
pub fn rollup_candidates(roots: &[QueryRoot]) -> Vec<RollupCandidate> {
    let mut nodes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for node in roots.iter().flat_map(root_operators) {
        collect_children_first(node, &mut seen, &mut nodes);
    }
    let mut readers: HashMap<*const OperatorNode, Vec<&Rc<OperatorNode>>> = HashMap::new();
    for node in &nodes {
        for child in node.children() {
            readers.entry(Rc::as_ptr(child)).or_default().push(node);
        }
    }
    let finalized = |build: &Rc<OperatorNode>| -> Vec<&Rc<OperatorNode>> {
        readers
            .get(&Rc::as_ptr(build))
            .map_or_else(Vec::new, |readers| {
                readers
                    .iter()
                    .filter(|r| matches!(r.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. })))
                    .copied()
                    .collect()
            })
    };
    let builds: Vec<&Rc<OperatorNode>> = nodes.iter().filter(|n| grouped_exact(n)).collect();
    let mut candidates = Vec::new();
    for &coarse in &builds {
        let finals = finalized(coarse);
        // Every reader of the coarse state reads its finalized value.
        if finals.is_empty() || finals.len() != readers[&Rc::as_ptr(coarse)].len() {
            continue;
        }
        let mut best: Option<(usize, RollupCandidate)> = None;
        for &fine in &builds {
            let Some(keys) = finer_keys(coarse, fine) else {
                continue;
            };
            if best.as_ref().is_some_and(|(k, _)| *k <= keys) {
                continue;
            }
            let Some(&finer) = finalized(fine).first() else {
                continue;
            };
            if let Some(rollup) = rolled_up(coarse, finer, finals[0]) {
                best = Some((
                    keys,
                    RollupCandidate {
                        coarse: Rc::clone(coarse),
                        finer: Rc::clone(finer),
                        rollup,
                    },
                ));
            }
        }
        candidates.extend(best.map(|(_, candidate)| candidate));
    }
    candidates.extend(sketch_rollup_candidates(&nodes, &readers));
    candidates
}

/// The sketch rollup candidates among `nodes` (children first), whose
/// readers are `readers`: each per-group sketch build grouped by listed keys
/// and read only by estimates its family answers, answered from the finest
/// build ([`finer_keys`] with the most keys, and no build finer than it) of
/// the same algorithm (at any size), update and filter over the same rows,
/// whose family's states merge into a state with a guarantee
/// ([`regroupable`]). Every coarse build of one finest build is one
/// choice in [`rollup_pass2_candidates`], so the choices stay few however
/// many groupings a workload asks for.
fn sketch_rollup_candidates(
    nodes: &[Rc<OperatorNode>],
    readers: &HashMap<*const OperatorNode, Vec<&Rc<OperatorNode>>>,
) -> Vec<RollupCandidate> {
    let builds: Vec<&Rc<OperatorNode>> = nodes.iter().filter(|n| regroupable(n)).collect();
    // The finer build with the most keys; among several (groupings that
    // are not nested), the one whose sorted key names come first, so the
    // choice does not depend on query order.
    let key_names = |build: &OperatorNode| -> Vec<ColumnName> {
        let mut names: Vec<ColumnName> = build
            .schema
            .fields
            .iter()
            .filter(|field| field.is_plain())
            .map(column_name)
            .collect();
        names.sort();
        names
    };
    let finest = |coarse: &OperatorNode| {
        builds
            .iter()
            .filter_map(|&fine| Some((finer_keys(coarse, fine)?, fine)))
            .min_by(|(keys, fine), (other_keys, other)| {
                other_keys
                    .cmp(keys)
                    .then_with(|| key_names(fine).cmp(&key_names(other)))
            })
            .map(|(_, fine)| fine)
    };
    let mut candidates = Vec::new();
    for &coarse in &builds {
        let Some(ASAPOp::SummaryAgg { family, .. }) = coarse.asap() else {
            continue;
        };
        if !readers
            .get(&Rc::as_ptr(coarse))
            .is_some_and(|readers| read_with_merged_guarantee(family, readers))
        {
            continue;
        }
        // A build finer than the one with the most keys would have more.
        let Some(fine) = finest(coarse) else {
            continue;
        };
        if let Some(rollup) = regrouped(coarse, fine) {
            candidates.push(RollupCandidate {
                coarse: Rc::clone(coarse),
                finer: Rc::clone(fine),
                rollup,
            });
        }
    }
    candidates
}

/// Whether every one of `readers`, the readers of a build of `family`, is
/// an estimate whose merged readout keeps a guarantee: not a heap's top-k
/// or a UnivMon's entropy.
fn read_with_merged_guarantee(family: &FieldDataType, readers: &[&Rc<OperatorNode>]) -> bool {
    let capability = FamilyCapability::of(family);
    readers.iter().all(|reader| match reader.asap() {
        Some(ASAPOp::SummaryEstimate { query, .. }) => {
            capability.answers(query)
                && matches!(
                    capability.merged_error(query),
                    Some(MergedError::Exact | MergedError::AsIfBuiltOverUnion)
                )
        }
        _ => false,
    })
}

/// Whether `node` is a per-group sketch build grouped by listed keys whose
/// states a rollup may merge across groups
/// ([`FamilyCapability::merges_groups`]).
fn regroupable(node: &OperatorNode) -> bool {
    matches!(
        node.asap(),
        Some(ASAPOp::SummaryAgg {
            family: family @ FieldDataType::Sketch(..),
            reduction: Reduction::Reduce(by),
            ..
        }) if !by.is_without() && FamilyCapability::merges_groups(family)
    )
}

/// The regrouping `SummaryMerge` of `fine`'s states by `coarse`'s keys, in
/// `coarse`'s order and with its column names; `None` when a key is not in
/// `fine`'s state.
fn regrouped(coarse: &OperatorNode, fine: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    let Some(ASAPOp::SummaryAgg {
        reduction: Reduction::Reduce(by),
        ..
    }) = coarse.asap()
    else {
        return None;
    };
    let keys = (0..by.keys().len())
        .map(|key| {
            let name = column_name(&coarse.schema.fields[key]);
            fine.schema
                .fields
                .iter()
                .position(|field| field.is_plain() && column_name(field) == name)
        })
        .collect::<Option<Vec<_>>>()?;
    regroup_merge(fine, keys, coarse).ok()
}

/// `SummaryMerge { [state], group_by: keys }`, its columns named as
/// `named`'s (the coarse build it stands for).
fn regroup_merge(
    state: &Rc<OperatorNode>,
    keys: Vec<usize>,
    named: &OperatorNode,
) -> Result<Rc<OperatorNode>, asap_types::ir::SchemaDerivationError> {
    let merge = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![Rc::clone(state)],
        group_by: Some(keys),
        newest_panes: None,
    }))?;
    let mut schema = merge.schema.clone();
    for (field, coarse) in schema.fields.iter_mut().zip(&named.schema.fields) {
        field.name = coarse.name.clone();
        field.table = coarse.table.clone();
    }
    Ok(Rc::new(OperatorNode::with_schema(merge.operator, schema)))
}

/// Whether `node` is an exact accumulator build grouped by listed keys whose
/// family merges across groups ([`rollup_kind`]).
fn grouped_exact(node: &OperatorNode) -> bool {
    matches!(
        node.asap(),
        Some(ASAPOp::SummaryAgg {
            family,
            reduction: Reduction::Reduce(by),
            grouping: GroupingStrategy::PerSubpopulationInstance,
            ..
        }) if !by.is_without() && rollup_kind(family).is_some()
    )
}

/// The number of group keys of `fine` when it can answer `coarse`: the same
/// family (a sketch's algorithm at any size), update and filter over the
/// same rows, and group keys (by column) that strictly include the coarse
/// ones.
fn finer_keys(coarse: &OperatorNode, fine: &OperatorNode) -> Option<usize> {
    let (
        Some(ASAPOp::SummaryAgg {
            child,
            family,
            input,
            reduction: Reduction::Reduce(by),
            filter,
            ..
        }),
        Some(ASAPOp::SummaryAgg {
            child: fine_child,
            family: fine_family,
            input: fine_input,
            reduction: Reduction::Reduce(fine_by),
            filter: fine_filter,
            ..
        }),
    ) = (coarse.asap(), fine.asap())
    else {
        return None;
    };
    let names = |node: &OperatorNode, keys: &[usize]| -> Option<Vec<ColumnName>> {
        keys.iter()
            .map(|&k| node.schema.fields.get(k).map(column_name))
            .collect()
    };
    let coarse_keys = names(child, by.keys())?;
    let fine_keys = names(fine_child, fine_by.keys())?;
    let same_filter = match (filter, fine_filter) {
        (None, None) => true,
        // A filter indexes its input's columns: equal only over equal rows.
        (a, b) => a == b && (Rc::ptr_eq(child, fine_child) || child == fine_child),
    };
    (same_family(family, fine_family)
        && input == fine_input
        && same_filter
        && fine_keys.len() > coarse_keys.len()
        && coarse_keys.iter().all(|key| fine_keys.contains(key))
        && same_rows(child, fine_child))
    .then_some(fine_keys.len())
}

type ColumnName = (Option<String>, String);

fn column_name(field: &asap_types::ir::schema::Field) -> ColumnName {
    (field.table.clone(), field.name.clone())
}

/// Whether `a` and `b` output the same rows: the same or equal nodes, or
/// time-series scans of one metric without predicates (projecting perhaps
/// other labels) under the same time ranges and shifts.
fn same_rows(a: &Rc<OperatorNode>, b: &Rc<OperatorNode>) -> bool {
    if Rc::ptr_eq(a, b) || a == b {
        return true;
    }
    match (a.non_asap(), b.non_asap()) {
        (
            Some(NonASAPOp::Scan {
                source: source @ Source::TimeSeries { .. },
                predicates,
                ..
            }),
            Some(NonASAPOp::Scan {
                source: other,
                predicates: other_predicates,
                ..
            }),
        ) => source == other && predicates.is_empty() && other_predicates.is_empty(),
        (
            Some(NonASAPOp::TimeRange { range, kind, child }),
            Some(NonASAPOp::TimeRange {
                range: other_range,
                kind: other_kind,
                child: other,
            }),
        ) => range == other_range && kind == other_kind && same_rows(child, other),
        (
            Some(NonASAPOp::TimeShift { shift, child }),
            Some(NonASAPOp::TimeShift {
                shift: other_shift,
                child: other,
            }),
        ) => shift == other_shift && same_rows(child, other),
        _ => false,
    }
}

/// The finalized value of `coarse` (`coarse_value`) as an exact
/// accumulator over `finer` (the finer build's finalized values), grouped by
/// the coarse keys, finalized, with `coarse_value`'s column names; `None`
/// when it cannot be built, the finer values are not Float64 (the only
/// update type exact accumulators execute), or it differs from
/// `coarse_value` in type.
fn rolled_up(
    coarse: &OperatorNode,
    finer: &Rc<OperatorNode>,
    coarse_value: &OperatorNode,
) -> Option<Rc<OperatorNode>> {
    use crate::per_computation_candidates::{exact_accumulator, partitions_over};
    let Some(ASAPOp::SummaryAgg {
        child,
        family,
        reduction,
        ..
    }) = coarse.asap()
    else {
        return None;
    };
    let Some(ASAPOp::FinalizeExactAccumulator { child: fine }) = finer.asap() else {
        return None;
    };
    let Some(ASAPOp::SummaryAgg {
        reduction: Reduction::Reduce(fine_by),
        ..
    }) = fine.asap()
    else {
        return None;
    };
    // A grouped accumulator outputs its keys, then its value.
    let value = fine_by.len();
    // Exact accumulators update on Float64 values only: an integer count is
    // not summed again.
    if finer.schema.fields.get(value).map(|f| &f.dtype)
        != Some(&FieldDataType::Plain(
            asap_types::ir::schema::DataType::Float64,
        ))
    {
        return None;
    }
    let regrouped = partitions_over(reduction, &child.schema, &finer.schema)?;
    if regrouped
        .group_keys()
        .is_some_and(|keys| keys.iter().any(|&k| k >= value))
    {
        return None;
    }
    let kind = rollup_kind(family)?;
    let intent = match kind {
        ExactKind::Min => AggIntent::Min { col: Some(value) },
        ExactKind::Max => AggIntent::Max { col: Some(value) },
        _ => AggIntent::Sum { col: Some(value) },
    };
    let state = exact_accumulator(kind, &intent, finer, regrouped).ok()?;
    let mut finalized =
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
            child: state,
        }))
        .ok()?;
    let types = |node: &OperatorNode| -> Vec<FieldDataType> {
        node.schema.fields.iter().map(|f| f.dtype.clone()).collect()
    };
    if types(&finalized) != types(coarse_value) {
        return None;
    }
    // A count summed is named after the sum: rename to the coarse columns.
    if finalized.schema.fields != coarse_value.schema.fields {
        let cols = coarse_value
            .schema
            .fields
            .iter()
            .enumerate()
            .map(|(column, field)| asap_types::ir::ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(column),
            })
            .collect();
        finalized = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols,
            qualifier: None,
            child: finalized,
        }))
        .ok()?;
    }
    Some(finalized)
}

/// The independent choices among rollup candidates `candidates`
/// ([`rollup_candidates`]), in order: each exact accumulator's alone, and
/// every sketch candidate of one finest build together, since they share
/// that build's size.
pub fn rollup_choices(candidates: &[RollupCandidate]) -> Vec<Vec<RollupCandidate>> {
    let mut choices: Vec<Vec<RollupCandidate>> = Vec::new();
    for candidate in candidates {
        let sketch = is_sketch_build(&candidate.finer);
        match choices.iter_mut().find(|choice| {
            sketch
                && is_sketch_build(&choice[0].finer)
                && Rc::ptr_eq(&choice[0].finer, &candidate.finer)
        }) {
            Some(choice) => choice.push(candidate.clone()),
            None => choices.push(vec![candidate.clone()]),
        }
    }
    choices
}

fn is_sketch_build(node: &OperatorNode) -> bool {
    matches!(
        node.asap(),
        Some(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(..),
            ..
        })
    )
}

/// Every Pass 2 candidate of the rollup rule for the candidate with query
/// roots `roots` and rollup candidates `candidates` ([`rollup_candidates`]):
/// one per non-empty subset of their choices ([`rollup_choices`]), in
/// binary-counter order (the first the lowest bit). A chosen sketch choice
/// resizes its finest build for every reader, its own and each coarse
/// build's ([`sized_for_all`]: the strictest consumer's size), and each
/// coarse build becomes the regrouping merge of the resized build's
/// states. An error only for more than [`MAX_REUSE_CANDIDATES`] choices; a
/// Pass 2 candidate whose composition fails carries its own error.
pub fn rollup_pass2_candidates(
    roots: &[QueryRoot],
    candidates: &[RollupCandidate],
) -> Result<Vec<RollupPass2Candidate>, PerComputationCandidateError> {
    Ok(subsets(&rollup_choices(candidates), "rollup")?
        .into_iter()
        .map(|choices| {
            let chosen: Vec<RollupCandidate> = choices.iter().flatten().cloned().collect();
            let values: HashMap<*const OperatorNode, &RollupCandidate> = chosen
                .iter()
                .filter(|candidate| !is_sketch_build(&candidate.finer))
                .map(|candidate| (Rc::as_ptr(&candidate.coarse), candidate))
                .collect();
            // Every finalized value of a chosen coarse exact build is
            // replaced.
            let mut replaced: HashMap<*const OperatorNode, Rc<OperatorNode>> = roots
                .iter()
                .flat_map(root_operators)
                .flat_map(OperatorNode::reachable)
                .filter_map(|node| match node.asap() {
                    Some(ASAPOp::FinalizeExactAccumulator { child }) => {
                        let candidate = values.get(&Rc::as_ptr(child))?;
                        Some((Rc::as_ptr(&node), Rc::clone(&candidate.rollup)))
                    }
                    _ => None,
                })
                .collect();
            let roots = sketch_rollups(&choices).and_then(|sketches| {
                replaced.extend(sketches);
                replace_all(roots, &replaced)
            });
            RollupPass2Candidate {
                roots,
                rollup_candidates: chosen,
            }
        })
        .collect())
}

/// The replacements of the sketch rollup choices among `choices`
/// ([`rollup_choices`]): each finest build by the build sized for it and
/// every coarse build it serves, and each coarse build by the regrouping
/// merge of that build's states. An error when a finest build and its
/// coarse builds have no shared size, or a node does not derive.
fn sketch_rollups(
    choices: &[Vec<RollupCandidate>],
) -> Result<Vec<(*const OperatorNode, Rc<OperatorNode>)>, PerComputationCandidateError> {
    let unsupported = || {
        PerComputationCandidateError::Unsupported(
            "a sketch rollup whose finest and coarse builds have no shared size",
        )
    };
    let mut replaced = Vec::new();
    for choice in choices {
        let fine = &choice[0].finer;
        if !is_sketch_build(fine) {
            continue;
        }
        let mut members = vec![fine];
        members.extend(choice.iter().map(|candidate| &candidate.coarse));
        let sized = sized_for_all(&members)?.ok_or_else(unsupported)?;
        replaced.push((Rc::as_ptr(fine), Rc::clone(&sized)));
        for candidate in choice {
            let Some(ASAPOp::SummaryMerge {
                group_by: Some(keys),
                ..
            }) = candidate.rollup.asap()
            else {
                unreachable!("a sketch rollup candidate's rollup is a regrouping merge");
            };
            let merge = regroup_merge(&sized, keys.clone(), &candidate.coarse)?;
            replaced.push((Rc::as_ptr(&candidate.coarse), merge));
        }
    }
    Ok(replaced)
}

/// Every non-empty subset of `candidates`, in binary-counter order (the
/// first candidate the lowest bit); an error past [`MAX_REUSE_CANDIDATES`].
fn subsets<T: Clone>(
    candidates: &[T],
    rule: &'static str,
) -> Result<Vec<Vec<T>>, PerComputationCandidateError> {
    let n = candidates.len();
    if n > MAX_REUSE_CANDIDATES {
        return Err(PerComputationCandidateError::TooManyCandidates {
            counted: match rule {
                "rollup" => "rollup choices",
                _ => "top-k reuse candidates",
            },
            found: n,
            max: MAX_REUSE_CANDIDATES,
        });
    }
    Ok((1u64..(1u64 << n))
        .map(|mask| {
            (0..n)
                .filter(|&i| mask >> i & 1 == 1)
                .map(|i| candidates[i].clone())
                .collect()
        })
        .collect())
}

/// `roots` with every node in `replaced` swapped for its replacement, and
/// the nodes a replacement reads rebuilt too (a rollup may read a build
/// another rollup replaces); every guarantee composed again.
fn replace_all(
    roots: &[QueryRoot],
    replaced: &HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    fn walk(
        node: &Rc<OperatorNode>,
        replaced: &HashMap<*const OperatorNode, Rc<OperatorNode>>,
        rebuilt: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
        if let Some(done) = rebuilt.get(&Rc::as_ptr(node)) {
            return Ok(Rc::clone(done));
        }
        let source = replaced.get(&Rc::as_ptr(node)).unwrap_or(node);
        let mut changed = !Rc::ptr_eq(source, node);
        let mut children = HashMap::new();
        for child in source.children() {
            let new = walk(child, replaced, rebuilt)?;
            changed |= !Rc::ptr_eq(&new, child);
            children.insert(Rc::as_ptr(child), new);
        }
        let out = if !changed {
            Rc::clone(node)
        } else if children
            .iter()
            .all(|(old, new)| std::ptr::eq(*old, Rc::as_ptr(new)))
        {
            Rc::clone(source)
        } else {
            Rc::new(source.with_new_children(|child| children[&Rc::as_ptr(child)].clone())?)
        };
        rebuilt.insert(Rc::as_ptr(node), Rc::clone(&out));
        Ok(out)
    }
    let mut rebuilt = HashMap::new();
    let mut out = Vec::new();
    for root in roots {
        out.push(match root {
            QueryRoot::Operator(node) => QueryRoot::Operator(walk(node, replaced, &mut rebuilt)?),
            QueryRoot::Scalar(expr) => {
                let mut nodes = HashMap::new();
                for node in expr.operator_refs() {
                    nodes.insert(Rc::as_ptr(node), walk(node, replaced, &mut rebuilt)?);
                }
                QueryRoot::Scalar(
                    expr.map_operator_refs(&mut |node| nodes[&Rc::as_ptr(node)].clone()),
                )
            }
        });
    }
    recompose_guarantees(&out)
}

/// Exact top-k computations of one input and partition with at least two
/// different `k`, in the order a children-first walk meets them, and the
/// ranking they all read in a reuse candidate: `Limit(k_max)` over `Sort`
/// of the input, partitioned as they are.
#[derive(Debug, Clone)]
pub struct TopKReuseCandidate {
    /// The plain `Aggregate` top-k nodes of one input and partition, in
    /// children-first order; at least two different `k`.
    pub members: Vec<Rc<OperatorNode>>,
    /// `Limit(k_max)` over a descending `Sort` of their input, partitioned
    /// as they are: what every member reads in a reuse candidate.
    pub ranking: Rc<OperatorNode>,
}

/// A Pass 2 candidate of the exact top-k reuse rule: the candidate's roots
/// with every member of each of `topk_reuse_candidates` reading its
/// ranking, and every guarantee composed again; the error when that fails.
#[derive(Debug)]
pub struct TopKReusePass2Candidate {
    /// The top-k reuse candidates applied, in [`topk_reuse_candidates`]
    /// order.
    pub topk_reuse_candidates: Vec<TopKReuseCandidate>,
    /// The candidate's roots with every member of those candidates reading
    /// its ranking and every guarantee composed again; `Err` when the
    /// rebuilt DAG does not derive or compose, which drops only this
    /// candidate.
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
}

/// The exact top-k reuse candidates of the candidate with query roots `roots`:
/// plain (exact) `Aggregate` top-k nodes over an exact input, unfiltered and
/// grouped by listed keys, grouped by equal input and partition.
pub fn topk_reuse_candidates(roots: &[QueryRoot]) -> Vec<TopKReuseCandidate> {
    let mut nodes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for node in roots.iter().flat_map(root_operators) {
        collect_children_first(node, &mut seen, &mut nodes);
    }
    let mut groups: Vec<Vec<Rc<OperatorNode>>> = Vec::new();
    for node in nodes.into_iter().filter(|n| exact_topk(n).is_some()) {
        let same = |member: &Rc<OperatorNode>| {
            let (Some((child, by, _)), Some((other, other_by, _))) =
                (exact_topk(member), exact_topk(&node))
            else {
                return false;
            };
            by == other_by && (Rc::ptr_eq(child, other) || child == other)
        };
        match groups.iter_mut().find(|g| same(&g[0])) {
            Some(group) => group.push(node),
            None => groups.push(vec![node]),
        }
    }
    groups
        .into_iter()
        .filter_map(|members| {
            let ks: Vec<usize> = members
                .iter()
                .filter_map(|m| exact_topk(m))
                .map(|t| t.2)
                .collect();
            let largest = *ks.iter().max()?;
            if ks.iter().all(|&k| k == largest) {
                return None;
            }
            let (child, by, _) = exact_topk(&members[0])?;
            let ranking = ranking(child, by, largest)?;
            Some(TopKReuseCandidate { members, ranking })
        })
        .collect()
}

/// The input, partition and `k` of an exact top-k over an exact input.
fn exact_topk(node: &OperatorNode) -> Option<(&Rc<OperatorNode>, &Reduction, usize)> {
    let Some(NonASAPOp::Aggregate {
        child,
        reduction: reduction @ Reduction::Reduce(by),
        measures,
        filters,
        having: None,
        ..
    }) = node.non_asap()
    else {
        return None;
    };
    let [AggIntent::TopK { k, .. }] = measures.as_slice() else {
        return None;
    };
    let exact = child
        .guarantee
        .as_ref()
        .is_some_and(asap_types::ir::properties::AccuracyGuarantee::is_exact);
    (exact && !by.is_without() && filters.iter().all(Option::is_none))
        .then_some((child, reduction, *k))
}

/// The `k` highest-ranked rows of `child` per partition: `Limit` over a
/// descending `Sort` by its ranked column, as exact top-k runs.
fn ranking(child: &Rc<OperatorNode>, by: &Reduction, k: usize) -> Option<Rc<OperatorNode>> {
    let Reduction::Reduce(keys) = by else {
        return None;
    };
    let value = crate::realization::ranked_column(child).ok()?;
    let sorted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Sort {
        keys: vec![SortKey {
            expr: ScalarExpr::Column(value),
            ascending: false,
            nulls_first: false,
        }],
        partition_by: keys.clone(),
        child: Rc::clone(child),
    }))
    .ok()?;
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Limit {
        n: Some(k),
        offset: 0,
        partition_by: keys.clone(),
        child: sorted,
    }))
    .ok()
}

/// Every Pass 2 candidate of the exact top-k reuse rule for the candidate
/// with query roots `roots` and top-k reuse candidates `candidates`
/// ([`topk_reuse_candidates`]): one per non-empty subset of them, in
/// binary-counter order. An error only for more than
/// [`MAX_REUSE_CANDIDATES`] of them.
pub fn topk_reuse_pass2_candidates(
    roots: &[QueryRoot],
    candidates: &[TopKReuseCandidate],
) -> Result<Vec<TopKReusePass2Candidate>, PerComputationCandidateError> {
    Ok(subsets(candidates, "top-k reuse")?
        .into_iter()
        .map(|chosen| {
            let mut replaced = HashMap::new();
            let mut failed = None;
            for candidate in &chosen {
                for member in &candidate.members {
                    match member.with_new_children(|_| Rc::clone(&candidate.ranking)) {
                        Ok(node) => {
                            replaced.insert(Rc::as_ptr(member), Rc::new(node));
                        }
                        Err(error) => failed = Some(error.into()),
                    }
                }
            }
            TopKReusePass2Candidate {
                roots: match failed {
                    Some(error) => Err(error),
                    None => replace_all(roots, &replaced),
                },
                topk_reuse_candidates: chosen,
            }
        })
        .collect())
}

fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

/// Distinct nodes (by identity) reachable from `roots`.
#[cfg(test)]
fn distinct_nodes(roots: &[QueryRoot]) -> usize {
    roots
        .iter()
        .flat_map(root_operators)
        .flat_map(OperatorNode::reachable)
        .map(|node| Rc::as_ptr(&node))
        .collect::<HashSet<_>>()
        .len()
}

/// Nodes reachable from both `a` and `b`.
#[cfg(test)]
fn common_nodes(a: &QueryRoot, b: &QueryRoot) -> HashSet<*const OperatorNode> {
    let reach = |root: &QueryRoot| -> HashSet<*const OperatorNode> {
        root_operators(root)
            .into_iter()
            .flat_map(OperatorNode::reachable)
            .map(|node| Rc::as_ptr(&node))
            .collect()
    };
    reach(a).intersection(&reach(b)).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::schema_support::with_promql_series_identity;
    use asap_types::ir::schema::SketchAlgorithm;

    fn roots(queries: &[&str]) -> Vec<QueryRoot> {
        queries
            .iter()
            .map(|q| {
                let root = lower_promql(q, AccuracyTarget::exact());
                QueryRoot::Operator(with_promql_series_identity(&root).unwrap())
            })
            .collect()
    }

    fn kinds(sharing_candidate: &SharingCandidate) -> &'static str {
        sharing_candidate.nodes[0].operator.kind_name()
    }

    /// The positions in `all` (by their nodes) of the sharing candidates
    /// `pass2_candidate` shares.
    fn shared_positions(
        all: &[SharingCandidate],
        pass2_candidate: &IdenticalExpressionPass2Candidate,
    ) -> Vec<usize> {
        positions(all, &pass2_candidate.sharing_candidates, |c| &c.nodes[0])
    }

    /// The positions in `all` of the items of `chosen`, matched by the node
    /// `node` gives for each.
    fn positions<T>(all: &[T], chosen: &[T], node: impl Fn(&T) -> &Rc<OperatorNode>) -> Vec<usize> {
        chosen
            .iter()
            .map(|item| {
                all.iter()
                    .position(|other| Rc::ptr_eq(node(other), node(item)))
                    .expect("a chosen item is listed")
            })
            .collect()
    }

    /// `sum by (job) (m)` and `max by (host, job) (m)` scan `m` with other
    /// labels, so nothing is shared until the scans are widened to
    /// `job, host`: then the scans and the instant time ranges above them
    /// are sharing candidates, and each root keeps its columns.
    #[test]
    fn scans_of_one_metric_with_other_labels_share_once_widened() {
        let roots = roots(&["sum by (job) (m)", "max by (host, job) (m)"]);
        assert!(sharing_candidates(&roots).is_empty());
        let widened = with_union_scan_labels(&roots).unwrap().unwrap();
        let scan_labels = |root: &QueryRoot| {
            let QueryRoot::Operator(node) = root else {
                unreachable!()
            };
            OperatorNode::reachable(node)
                .iter()
                .find_map(|n| match n.non_asap() {
                    Some(NonASAPOp::Scan { schema, .. }) => Some(
                        schema
                            .fields
                            .iter()
                            .map(|f| f.name.clone())
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(
            scan_labels(&widened[0]),
            ["ts", "value", "job", "host", PROMQL_SERIES_IDENTITY]
        );
        assert_eq!(scan_labels(&widened[0]), scan_labels(&widened[1]));
        // The unique key still names the series identity and ts.
        let QueryRoot::Operator(node) = &widened[0] else {
            unreachable!()
        };
        let scan = OperatorNode::reachable(node)
            .into_iter()
            .find(|n| matches!(n.non_asap(), Some(NonASAPOp::Scan { .. })))
            .unwrap();
        assert_eq!(scan.schema.unique_keys, [vec![4, 0]]);
        let kinds: Vec<_> = sharing_candidates(&widened).iter().map(kinds).collect();
        assert_eq!(kinds, ["Scan", "TimeRange"]);
        for (root, wide) in roots.iter().zip(&widened) {
            let (QueryRoot::Operator(root), QueryRoot::Operator(wide)) = (root, wide) else {
                unreachable!()
            };
            assert_eq!(root.schema, wide.schema);
        }
        // Scans that already agree are left alone.
        assert!(with_union_scan_labels(&widened).unwrap().is_none());
    }

    /// Two queries over the same range selector have two sharing candidates,
    /// the scans and the time ranges. Sharing the time range shares the
    /// scan below it, so there are two Pass 2 candidates: the scan alone shared, and
    /// both shared. Each query's value is unchanged.
    #[test]
    fn identical_range_selector_gives_one_pass2_candidate_per_distinct_subset() {
        let roots = roots(&[
            "sum by (job) (rate(m[1m]))",
            "topk by (job) (10, sum_over_time(m[1m]))",
        ]);
        let sharing = sharing_candidates(&roots);
        assert_eq!(
            sharing.iter().map(kinds).collect::<Vec<_>>(),
            ["Scan", "TimeRange"]
        );
        let pass2_candidates =
            identical_expression_pass2_candidates(&roots, &sharing_candidates(&roots));
        assert_eq!(
            pass2_candidates
                .iter()
                .map(|v| shared_positions(&sharing, v))
                .collect::<Vec<_>>(),
            [vec![0], vec![0, 1]]
        );
        for pass2_candidate in &pass2_candidates {
            assert_eq!(pass2_candidate.roots, roots, "values are unchanged");
            assert!(distinct_nodes(&pass2_candidate.roots) < distinct_nodes(&roots));
        }
        assert_eq!(
            common_nodes(&pass2_candidates[0].roots[0], &pass2_candidates[0].roots[1]).len(),
            1
        );
        assert_eq!(
            common_nodes(&pass2_candidates[1].roots[0], &pass2_candidates[1].roots[1]).len(),
            2
        );
    }

    /// Two pairs of queries over different metrics: each pair has a scan
    /// group and an instant-selector group, with two distinct ways to share
    /// (the scan, or the scan and the selector) or none, so 3 × 3 − 1 = 8
    /// Pass 2 candidates; each pair is shared alone or with the other.
    #[test]
    fn independent_sharing_candidates_give_every_non_empty_subset() {
        let roots = roots(&["sum(a)", "max(a)", "sum(b)", "max(b)"]);
        assert_eq!(
            sharing_candidates(&roots)
                .iter()
                .map(kinds)
                .collect::<Vec<_>>(),
            ["Scan", "TimeRange", "Scan", "TimeRange"]
        );
        let pass2_candidates =
            identical_expression_pass2_candidates(&roots, &sharing_candidates(&roots));
        assert_eq!(pass2_candidates.len(), 8);
        let shares = |v: &IdenticalExpressionPass2Candidate| {
            (
                !common_nodes(&v.roots[0], &v.roots[1]).is_empty(),
                !common_nodes(&v.roots[2], &v.roots[3]).is_empty(),
            )
        };
        assert_eq!(
            pass2_candidates.iter().map(shares).collect::<HashSet<_>>(),
            HashSet::from([(true, false), (false, true), (true, true)])
        );
        assert!(pass2_candidates.iter().all(|v| v.roots == roots));
    }

    /// Queries with nothing in common have no Pass 2 candidate.
    #[test]
    fn nothing_identical_gives_no_pass2_candidate() {
        assert!({
            let roots = roots(&["sum(rate(a[1m]))", "sum(rate(b[1m]))"]);
            identical_expression_pass2_candidates(&roots, &sharing_candidates(&roots))
        }
        .is_empty());
    }

    /// Every non-empty subset of the sharing candidates, applied, kept when it gives a
    /// distinct candidate: the definition the direct enumeration follows.
    fn brute_force(roots: &[QueryRoot]) -> Vec<Vec<usize>> {
        let sharing = sharing_candidates(roots);
        let mut subset = vec![false; sharing.len()];
        let mut out = Vec::new();
        while next_subset(&mut subset) {
            let (_, merged) = apply(roots, &sharing, &subset);
            if merged == subset {
                out.push((0..sharing.len()).filter(|&g| subset[g]).collect());
            }
        }
        out
    }

    /// The direct enumeration gives exactly the brute force's candidates, in
    /// the same order, on nested and independent groups.
    #[test]
    fn direct_enumeration_matches_the_brute_force() {
        for queries in [
            &[
                "sum by (job) (rate(m[1m]))",
                "topk by (job) (10, sum_over_time(m[1m]))",
            ][..],
            &["sum(a)", "max(a)", "sum(b)", "max(b)"],
            &["sum(abs(a))", "max(abs(a))", "min(a)", "sum(b)", "max(b)"],
            &["sum(abs(ceil(a)))", "max(abs(ceil(a)))", "min(ceil(a))"],
        ] {
            let roots = roots(queries);
            let sharing = sharing_candidates(&roots);
            let direct: Vec<_> = identical_expression_pass2_candidates(&roots, &sharing)
                .iter()
                .map(|v| shared_positions(&sharing, v))
                .collect();
            assert_eq!(direct, brute_force(&roots), "{queries:?}");
        }
    }

    /// Twenty nested sharing candidates (two identical chains of a keyed scan and 19
    /// filters) have 20 distinct candidates, one per depth shared, not 2^20
    /// subsets to try.
    #[test]
    fn twenty_nested_sharing_candidates_enumerate_quickly() {
        use crate::test_support::{filter, metric_scan_with_keys};
        use asap_types::ir::scalar::expr_ir::CompareOpKind;
        use asap_types::ir::ScalarExpr;
        let chain = || {
            let mut node = metric_scan_with_keys(&["job"], vec![vec![0, 2]]);
            for i in 0..19 {
                node = filter(
                    ScalarExpr::Compare {
                        left: Box::new(ScalarExpr::Column(1)),
                        op: CompareOpKind::Gt,
                        right: Box::new(ScalarExpr::literal_f64(i as f64)),
                        semantics: asap_types::ir::ExprSemantics::Promql,
                    },
                    node,
                );
            }
            QueryRoot::Operator(node)
        };
        let roots = [chain(), chain()];
        let sharing = sharing_candidates(&roots);
        assert_eq!(sharing.len(), 20);
        let start = std::time::Instant::now();
        let pass2_candidates = identical_expression_pass2_candidates(&roots, &sharing);
        assert_eq!(pass2_candidates.len(), 20);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    /// A count over a window, read from a CountSketch built in tumbling
    /// panes, keeps the guarantee of one build over the window: Pass 1
    /// offers CountSketch for counts, and its merged counters add.
    #[test]
    fn a_count_from_count_sketch_panes_keeps_its_guarantee() {
        use crate::per_computation_candidates::{
            compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
        };
        let root = lower_promql(
            "count_over_time(m[5m])",
            asap_types::ir::properties::AccuracyTarget::max_error(0.01).failure_probability(0.001),
        );
        let inventory = generate_per_computation_candidates(vec![(
            0,
            QueryRoot::Operator(with_promql_series_identity(&root).unwrap()),
        )])
        .unwrap();
        let choice: Vec<usize> = inventory
            .targets
            .iter()
            .map(|t| {
                t.choices
                    .candidates
                    .iter()
                    .position(|c| *c == ComputationCandidate::Summary(SketchAlgorithm::CountSketch))
                    .expect("CountSketch is offered for a count")
            })
            .collect();
        let roots: Vec<QueryRoot> = compose_logical_candidate(&inventory, &choice)
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        let candidates = pane_candidates(&roots, &[Some(60_000)]);
        let pass2_candidates = window_composition_pass2_candidates(&roots, &candidates).unwrap();
        let [pass2_candidate] = pass2_candidates.as_slice() else {
            panic!("one Pass 2 candidate: {pass2_candidates:?}")
        };
        let (QueryRoot::Operator(whole), QueryRoot::Operator(paned)) =
            (&roots[0], &pass2_candidate.roots.as_ref().unwrap()[0])
        else {
            panic!("operator roots")
        };
        assert!(whole.guarantee.is_some());
        assert_eq!(paned.guarantee, whole.guarantee);
    }

    /// The composed candidate of `queries` (each at its own ε, δ = 0.01)
    /// in which every target chose `family`.
    fn chose(queries: &[(&str, f64)], family: SketchAlgorithm) -> Vec<QueryRoot> {
        use crate::per_computation_candidates::ComputationCandidate;
        chose_where(queries, |c| {
            *c == ComputationCandidate::Summary(family.clone())
        })
    }

    /// The composed candidate of `queries` (each at its own ε, δ = 0.01)
    /// in which every target chose the first candidate `pick` accepts.
    fn chose_where(
        queries: &[(&str, f64)],
        pick: impl Fn(&crate::per_computation_candidates::ComputationCandidate) -> bool,
    ) -> Vec<QueryRoot> {
        use crate::per_computation_candidates::{
            compose_logical_candidate, generate_per_computation_candidates,
        };
        let roots = queries
            .iter()
            .enumerate()
            .map(|(i, (q, epsilon))| {
                let root = lower_promql(
                    q,
                    asap_types::ir::properties::AccuracyTarget::max_error(*epsilon)
                        .failure_probability(0.01)
                        .accuracy_metric(asap_types::ir::properties::AccuracyMetric::Rank),
                );
                (
                    i,
                    QueryRoot::Operator(with_promql_series_identity(&root).unwrap()),
                )
            })
            .collect();
        let inventory = generate_per_computation_candidates(roots).unwrap();
        let choice: Vec<usize> = inventory
            .targets
            .iter()
            .map(|t| {
                t.choices
                    .candidates
                    .iter()
                    .position(&pick)
                    .expect("the family is a candidate")
            })
            .collect();
        compose_logical_candidate(&inventory, &choice)
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect()
    }

    /// The summary builds `roots` read, by address, with each one's KLL `k`.
    fn kll_builds(roots: &[QueryRoot]) -> Vec<(*const OperatorNode, u32)> {
        let mut out: Vec<(*const OperatorNode, u32)> = Vec::new();
        for node in roots
            .iter()
            .flat_map(root_operators)
            .flat_map(OperatorNode::reachable)
        {
            if let Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }) = node.asap()
            {
                let asap_types::ir::schema::SketchParams::Kll { k, .. } = kind.params() else {
                    panic!("{kind:?}")
                };
                if !out.iter().any(|(p, _)| *p == Rc::as_ptr(&node)) {
                    out.push((Rc::as_ptr(&node), *k));
                }
            }
        }
        out
    }

    fn root_bound(root: &QueryRoot) -> f64 {
        let QueryRoot::Operator(node) = root else {
            panic!("operator root")
        };
        node.guarantee
            .as_ref()
            .and_then(|g| g.spec.error.evaluate())
            .expect("a constant bound")
    }

    /// p50 at ε = 0.01 and p99 at ε = 0.001 over one window, each with its
    /// own KLL: the rule shares one KLL with the larger `k`, and p50's
    /// guarantee is the shared KLL's, tighter than its own.
    #[test]
    fn quantiles_at_different_targets_share_one_kll_with_the_larger_k() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[5m])", 0.001),
            ],
            SketchAlgorithm::Kll,
        );
        let own = kll_builds(&roots);
        assert_eq!(own.len(), 2);
        assert!(own[0].1 < own[1].1);
        let sharing = capability_sharing_candidates(&roots);
        assert_eq!(sharing.len(), 1);
        assert!(
            matches!(&sharing[0].family, FieldDataType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::Kll)
        );
        let pass2_candidates = summary_capability_pass2_candidates(&roots, &sharing).unwrap();
        assert_eq!(pass2_candidates.len(), 1);
        let [shared_builds] = pass2_candidates[0].shared_builds.as_slice() else {
            panic!("one shared build: {:?}", pass2_candidates[0].shared_builds)
        };
        assert_eq!(positions(&sharing[0].members, shared_builds, |m| m), [0, 1]);
        let shared = kll_builds(pass2_candidates[0].roots.as_ref().unwrap());
        assert_eq!(shared.len(), 1, "one KLL serves both");
        assert_eq!(shared[0].1, own[1].1);
        assert!(
            root_bound(&pass2_candidates[0].roots.as_ref().unwrap()[0]) < root_bound(&roots[0])
        );
        assert_eq!(
            root_bound(&pass2_candidates[0].roots.as_ref().unwrap()[1]),
            root_bound(&roots[1])
        );
    }

    /// Three consumers at three targets: every subset of two or more of
    /// them shares one build (three pairs and all three), the others keep
    /// their own.
    #[test]
    fn every_subset_of_two_or_more_consumers_shares_one_build() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.05),
                ("quantile_over_time(0.9, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[5m])", 0.001),
            ],
            SketchAlgorithm::Kll,
        );
        let sharing = capability_sharing_candidates(&roots);
        let pass2_candidates = summary_capability_pass2_candidates(&roots, &sharing).unwrap();
        assert_eq!(
            pass2_candidates
                .iter()
                .map(|v| positions(&sharing[0].members, &v.shared_builds[0], |m| m))
                .collect::<Vec<_>>(),
            [vec![0, 1], vec![0, 2], vec![1, 2], vec![0, 1, 2]]
        );
        for pass2_candidate in &pass2_candidates {
            let builds = kll_builds(pass2_candidate.roots.as_ref().unwrap());
            assert_eq!(builds.len(), 4 - pass2_candidate.shared_builds[0].len());
        }
    }

    /// A capability sharing candidate of more than 20 members (over a million sharing subsets)
    /// is an error, not a silent skip or an endless enumeration.
    #[test]
    fn a_capability_sharing_candidate_past_20_members_is_an_error() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[5m])", 0.001),
            ],
            SketchAlgorithm::Kll,
        );
        let mut sharing = capability_sharing_candidates(&roots);
        let members = sharing[0].members.clone();
        sharing[0].members = members
            .iter()
            .cycle()
            .take(MAX_CAPABILITY_SHARING_MEMBERS + 1)
            .cloned()
            .collect();
        let error = summary_capability_pass2_candidates(&roots, &sharing).unwrap_err();
        assert!(matches!(
            error,
            PerComputationCandidateError::TooManyCandidates {
                max: MAX_CAPABILITY_SHARING_MEMBERS,
                ..
            }
        ));
        // The message names the count found and the cap from its constant.
        assert_eq!(
            error.to_string(),
            format!(
                "found {} summary builds in one capability sharing candidate, more than \
                 {MAX_CAPABILITY_SHARING_MEMBERS}",
                MAX_CAPABILITY_SHARING_MEMBERS + 1
            )
        );
    }

    /// Equal targets give identical builds, which are the
    /// identical-expression rule's; a different window or family is a
    /// different input or capability, never shared.
    #[test]
    fn equal_sizes_other_windows_and_other_families_are_not_capability_sharing_candidates() {
        let equal = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[5m])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        assert!(capability_sharing_candidates(&equal).is_empty());
        assert!(
            !identical_expression_pass2_candidates(&equal, &sharing_candidates(&equal)).is_empty()
        );
        let windows = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[10m])", 0.001),
            ],
            SketchAlgorithm::Kll,
        );
        assert!(capability_sharing_candidates(&windows).is_empty());
        let families = chose(
            &[
                ("quantile_over_time(0.5, lat[5m])", 0.01),
                ("quantile_over_time(0.99, lat[5m])", 0.001),
            ],
            SketchAlgorithm::DDSketch,
        );
        assert_eq!(capability_sharing_candidates(&families).len(), 1);
        assert!(matches!(
            &capability_sharing_candidates(&families)[0].family,
            FieldDataType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::DDSketch
        ));
    }

    /// Quantiles over other groupings share no capability build.
    #[test]
    fn different_groupings_are_not_capability_sharing_candidates() {
        let groupings = chose(
            &[
                ("quantile by (job) (0.5, lat)", 0.01),
                ("quantile (0.99, lat)", 0.001),
            ],
            SketchAlgorithm::Kll,
        );
        assert!(capability_sharing_candidates(&groupings).is_empty());
    }

    /// Each candidate's label (the groups it shares) and its roots agree:
    /// applying the label gives the same roots, sharing exactly the
    /// labelled groups.
    #[test]
    fn labels_and_roots_agree() {
        for queries in [
            &[
                "sum by (job) (rate(m[1m]))",
                "topk by (job) (10, sum_over_time(m[1m]))",
            ][..],
            &["sum(a)", "max(a)", "sum(b)", "max(b)"],
            &["sum(abs(a))", "max(abs(a))", "min(a)", "sum(b)", "max(b)"],
        ] {
            let roots = roots(queries);
            let sharing = sharing_candidates(&roots);
            for pass2_candidate in identical_expression_pass2_candidates(&roots, &sharing) {
                let shared = shared_positions(&sharing, &pass2_candidate);
                let label: Vec<bool> = (0..sharing.len()).map(|g| shared.contains(&g)).collect();
                let (applied, merged) = apply(&roots, &sharing, &label);
                assert_eq!(merged, label, "{queries:?}");
                assert_eq!(applied, pass2_candidate.roots, "{queries:?}");
                assert_eq!(
                    distinct_nodes(&applied),
                    distinct_nodes(&pass2_candidate.roots),
                    "{queries:?} {:?}",
                    shared
                );
                for (a, b) in [(0, 1), (2, 3)] {
                    if b < roots.len() {
                        assert_eq!(
                            common_nodes(&applied[a], &applied[b]).len(),
                            common_nodes(&pass2_candidate.roots[a], &pass2_candidate.roots[b])
                                .len()
                        );
                    }
                }
            }
        }
    }

    /// The pane candidates of `roots` as (pane width, pane count).
    fn panes(roots: &[QueryRoot], interval_ms: &[Option<u64>]) -> Vec<(u64, u32)> {
        pane_candidates(roots, interval_ms)
            .iter()
            .map(|rule_candidate| (rule_candidate.pane_width_ms, rule_candidate.pane_count))
            .collect()
    }

    /// A 5-min window evaluated every 2 min, 90 s or 1 min is built in panes
    /// of gcd(window, interval); a query evaluated once per window, or not
    /// at a fixed interval, has no panes.
    #[test]
    fn pane_width_is_the_gcd_of_the_window_and_the_interval() {
        let roots = chose(
            &[("quantile_over_time(0.99, m[5m])", 0.01)],
            SketchAlgorithm::Kll,
        );
        assert_eq!(panes(&roots, &[Some(120_000)]), [(60_000, 5)]);
        assert_eq!(panes(&roots, &[Some(90_000)]), [(30_000, 10)]);
        assert_eq!(panes(&roots, &[Some(60_000)]), [(60_000, 5)]);
        assert!(panes(&roots, &[Some(300_000)]).is_empty());
        assert!(panes(&roots, &[None]).is_empty());
    }

    /// Past their caps, the pane rule and the reuse subsets name how many
    /// candidates they found.
    #[test]
    fn too_many_pane_or_reuse_candidates_name_the_count_found() {
        let roots = chose(
            &[("quantile_over_time(0.99, m[5m])", 0.01)],
            SketchAlgorithm::Kll,
        );
        let panes: Vec<_> = pane_candidates(&roots, &[Some(60_000)])
            .into_iter()
            .cycle()
            .take(MAX_PANE_CANDIDATES + 1)
            .collect();
        let error = window_composition_pass2_candidates(&roots, &panes).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "found {} summary builds that could be built in tumbling panes, more than \
                 {MAX_PANE_CANDIDATES}",
                MAX_PANE_CANDIDATES + 1
            )
        );
        let error = subsets(&[0u8; MAX_REUSE_CANDIDATES + 1], "rollup").unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "found {} rollup choices, more than {MAX_REUSE_CANDIDATES}",
                MAX_REUSE_CANDIDATES + 1
            )
        );
    }

    /// A range read at an `@` anchor has no panes, however many shifts
    /// stack above the anchored one: `m[5m] @ 100` with an extra
    /// `offset 1m` shift between the range and the anchor is still anchored.
    #[test]
    fn an_anchor_below_stacked_shifts_has_no_panes() {
        use asap_types::ir::operator::TimeShift;
        let roots = chose(
            &[("quantile_over_time(0.99, m[5m] @ 100)", 0.01)],
            SketchAlgorithm::Kll,
        );
        let QueryRoot::Operator(root) = &roots[0] else {
            panic!("operator root")
        };
        let build = OperatorNode::reachable(root)
            .into_iter()
            .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .expect("a build");
        assert_eq!(pane_window_ms(&build), None, "one anchored shift");
        // Put an `offset 1m` shift between the range and its anchored shift.
        let restacked = build
            .with_new_children(|range| {
                Rc::new(
                    range
                        .with_new_children(|anchored| {
                            assert!(matches!(
                                anchored.non_asap(),
                                Some(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_some()
                            ));
                            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
                                shift: TimeShift {
                                    offset_ms: 60_000,
                                    at: None,
                                },
                                child: anchored.clone(),
                            }))
                            .unwrap()
                        })
                        .unwrap(),
                )
            })
            .unwrap();
        assert_eq!(pane_window_ms(&restacked), None, "anchor below an offset");
    }

    /// Exact accumulators whose states merge get panes; IRate, which reads
    /// the last two samples of the window, does not.
    #[test]
    fn exact_accumulators_that_merge_get_panes() {
        use crate::per_computation_candidates::ComputationCandidate;
        let accumulator =
            |c: &ComputationCandidate| matches!(c, ComputationCandidate::ExactAccumulator(_));
        for query in [
            "rate(m[5m])",
            "increase(m[5m])",
            "sum_over_time(m[5m])",
            "max_over_time(m[5m])",
        ] {
            let roots = chose_where(&[(query, 0.01)], accumulator);
            assert_eq!(panes(&roots, &[Some(60_000)]), [(60_000, 5)], "{query}");
        }
        let roots = chose_where(&[("irate(m[5m])", 0.01)], accumulator);
        assert!(panes(&roots, &[Some(60_000)]).is_empty());
    }

    /// The rule's candidate reads a `SummaryMerge` over one `PaneChain` of
    /// the build's family and size over the range's input, keeps the
    /// build's output names, and keeps the whole build's guarantee (a KLL
    /// merges into the state of one build over the union).
    #[test]
    fn panes_replace_the_build_with_a_merged_chain() {
        let roots = chose(
            &[("quantile_over_time(0.99, m[5m])", 0.01)],
            SketchAlgorithm::Kll,
        );
        let rule_candidates = pane_candidates(&roots, &[Some(60_000)]);
        let pass2_candidates =
            window_composition_pass2_candidates(&roots, &rule_candidates).unwrap();
        let [pass2_candidate] = pass2_candidates.as_slice() else {
            panic!("one Pass 2 candidate: {pass2_candidates:?}")
        };
        assert_eq!(
            positions(&rule_candidates, &pass2_candidate.pane_candidates, |c| &c
                .build),
            [0]
        );
        let panes = pass2_candidate.roots.as_ref().unwrap();
        let QueryRoot::Operator(root) = &panes[0] else {
            panic!("operator root")
        };
        root.validate_structure().unwrap();
        let QueryRoot::Operator(whole) = &roots[0] else {
            panic!("operator root")
        };
        assert_eq!(root.guarantee, whole.guarantee);
        assert!(root.guarantee.is_some());
        let merge = OperatorNode::reachable(root)
            .into_iter()
            .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryMerge { .. })))
            .expect("a merge");
        let Some(ASAPOp::SummaryMerge { children, .. }) = merge.asap() else {
            unreachable!()
        };
        let [chain] = children.as_slice() else {
            panic!("one chain")
        };
        let Some(ASAPOp::PaneChain {
            child,
            family,
            pane_width_ms: 60_000,
            pane_count: 5,
            ..
        }) = chain.asap()
        else {
            panic!("{:?}", chain.operator)
        };
        let Some(ASAPOp::SummaryAgg {
            child: range,
            family: whole_family,
            ..
        }) = rule_candidates[0].build.asap()
        else {
            unreachable!()
        };
        assert_eq!(family, whole_family);
        assert_eq!(child.operator, range.children()[0].operator);
        assert_eq!(chain.schema, rule_candidates[0].build.schema);
        assert_eq!(merge.coverage(), rule_candidates[0].build.coverage());
    }

    /// Two builds that could be split give three candidates: each alone and
    /// both, in binary-counter order. Their windows (5 and 2 min) nest on
    /// 1-min panes, so both together are also offered as one shared chain.
    #[test]
    fn every_non_empty_subset_of_the_pane_candidates_is_a_pass2_candidate() {
        let roots = chose(
            &[
                ("quantile_over_time(0.99, m[5m])", 0.01),
                ("quantile_over_time(0.5, m[2m])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let rule_candidates = pane_candidates(&roots, &[Some(60_000), Some(60_000)]);
        assert_eq!(rule_candidates.len(), 2);
        let subsets: Vec<_> = window_composition_pass2_candidates(&roots, &rule_candidates)
            .unwrap()
            .iter()
            .map(|v| {
                assert!(v.roots.is_ok());
                (
                    positions(&rule_candidates, &v.pane_candidates, |c| &c.build),
                    v.shared_chains.clone(),
                )
            })
            .collect();
        assert_eq!(
            subsets,
            [
                (vec![0], vec![]),
                (vec![1], vec![]),
                (vec![0, 1], vec![]),
                (
                    vec![0, 1],
                    vec![SharedPaneChain {
                        members: vec![0, 1],
                        pane_width_ms: 60_000,
                        pane_count: 5
                    }]
                )
            ]
        );
    }

    /// The pane chains in `roots`, one per distinct node.
    fn chains(roots: &[QueryRoot]) -> Vec<Rc<OperatorNode>> {
        let mut found: Vec<Rc<OperatorNode>> = Vec::new();
        for root in roots {
            for node in OperatorNode::reachable(operator(root)) {
                if matches!(node.asap(), Some(ASAPOp::PaneChain { .. }))
                    && !found.iter().any(|c| Rc::ptr_eq(c, &node))
                {
                    found.push(node);
                }
            }
        }
        found
    }

    /// The `newest_panes` of the one merge `root` reads.
    fn newest_panes(root: &QueryRoot) -> Option<u32> {
        OperatorNode::reachable(operator(root))
            .into_iter()
            .find_map(|n| match n.asap() {
                Some(ASAPOp::SummaryMerge { newest_panes, .. }) => Some(*newest_panes),
                _ => None,
            })
            .expect("a merge")
    }

    /// A 1-min and a 5-min KLL quantile over one metric, both every 10 s,
    /// are 6 and 30 panes of 10 s; the 1-min window is the newest 6 of the
    /// 5-min one. Besides each query's own chain, the rule offers one chain
    /// of 30 panes read by both: the 5-min query merges every pane, the
    /// 1-min one only the newest 6, and each keeps the guarantee of its own
    /// whole-window build (a KLL merges exactly).
    #[test]
    fn nested_windows_on_equal_panes_share_one_chain() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, m[1m])", 0.01),
                ("quantile_over_time(0.9, m[5m])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let rule_candidates = pane_candidates(&roots, &[Some(10_000), Some(10_000)]);
        assert_eq!(
            panes(&roots, &[Some(10_000), Some(10_000)]),
            [(10_000, 6), (10_000, 30)]
        );
        let pass2_candidates =
            window_composition_pass2_candidates(&roots, &rule_candidates).unwrap();
        let shared: Vec<_> = pass2_candidates
            .iter()
            .filter(|c| !c.shared_chains.is_empty())
            .collect();
        let [shared] = shared.as_slice() else {
            panic!("one shared-chain candidate: {pass2_candidates:?}")
        };
        assert_eq!(
            shared.shared_chains,
            [SharedPaneChain {
                members: vec![0, 1],
                pane_width_ms: 10_000,
                pane_count: 30
            }]
        );
        let shared_roots = shared.roots.as_ref().unwrap();
        let [chain] = chains(shared_roots)
            .try_into()
            .unwrap_or_else(|c: Vec<_>| panic!("one chain, found {}", c.len()));
        assert!(matches!(
            chain.asap(),
            Some(ASAPOp::PaneChain {
                pane_width_ms: 10_000,
                pane_count: 30,
                ..
            })
        ));
        assert_eq!(newest_panes(&shared_roots[0]), Some(6));
        assert_eq!(newest_panes(&shared_roots[1]), None);
        for (root, whole) in shared_roots.iter().zip(&roots) {
            let root = operator(root);
            root.validate_structure().unwrap();
            assert!(root.guarantee.is_some());
            assert_eq!(root.guarantee, operator(whole).guarantee);
        }
        // The per-query chains stay candidates: two chains, each read in full.
        let separate = pass2_candidates
            .iter()
            .find(|c| c.pane_candidates.len() == 2 && c.shared_chains.is_empty())
            .expect("both builds in their own panes");
        assert_eq!(chains(separate.roots.as_ref().unwrap()).len(), 2);
    }

    /// A 15-min quantile every minute and the same quantile every 5 min
    /// are 15 × 1-min and 3 × 5-min panes. The rule also offers one chain
    /// at the finer width, 15 × 1 min, that both read in full, each at its
    /// own interval; a 1-hour quantile every 5 min (12 × 5 min) reads the
    /// newest 60 of one 60 × 1-min chain with them.
    #[test]
    fn queries_at_different_intervals_share_one_chain_at_the_finer_width() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, m[15m])", 0.01),
                ("quantile_over_time(0.9, m[15m])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let grids = [Some(60_000), Some(300_000)];
        assert_eq!(panes(&roots, &grids), [(60_000, 15), (300_000, 3)]);
        let rule_candidates = pane_candidates(&roots, &grids);
        let pass2_candidates =
            window_composition_pass2_candidates(&roots, &rule_candidates).unwrap();
        let shared = pass2_candidates
            .iter()
            .find(|c| !c.shared_chains.is_empty())
            .expect("a shared-chain candidate");
        assert_eq!(
            shared.shared_chains,
            [SharedPaneChain {
                members: vec![0, 1],
                pane_width_ms: 60_000,
                pane_count: 15
            }]
        );
        let shared_roots = shared.roots.as_ref().unwrap();
        let [chain] = chains(shared_roots)
            .try_into()
            .unwrap_or_else(|c: Vec<_>| panic!("one chain, found {}", c.len()));
        assert!(matches!(
            chain.asap(),
            Some(ASAPOp::PaneChain {
                pane_width_ms: 60_000,
                pane_count: 15,
                ..
            })
        ));
        assert_eq!(newest_panes(&shared_roots[0]), None);
        assert_eq!(newest_panes(&shared_roots[1]), None);
        for (root, whole) in shared_roots.iter().zip(&roots) {
            operator(root).validate_structure().unwrap();
            assert_eq!(operator(root).guarantee, operator(whole).guarantee);
        }

        let roots = chose(
            &[
                ("quantile_over_time(0.5, m[15m])", 0.01),
                ("quantile_over_time(0.9, m[1h])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let rule_candidates = pane_candidates(&roots, &grids);
        let shared = window_composition_pass2_candidates(&roots, &rule_candidates)
            .unwrap()
            .into_iter()
            .find(|c| !c.shared_chains.is_empty())
            .expect("a shared-chain candidate");
        let shared_roots = shared.roots.unwrap();
        assert_eq!(newest_panes(&shared_roots[0]), Some(15));
        assert_eq!(newest_panes(&shared_roots[1]), None);
        assert!(matches!(
            chains(&shared_roots)[0].asap(),
            Some(ASAPOp::PaneChain {
                pane_width_ms: 60_000,
                pane_count: 60,
                ..
            })
        ));
    }

    /// A build whose panes the group's finest panes do not divide gets its
    /// own chain without breaking the group: 20-min and 10-min quantiles
    /// every 2 min (2-min panes) share one chain, and a 15-min quantile
    /// every 3 min (3-min panes) stays apart.
    #[test]
    fn a_build_on_an_incompatible_grid_does_not_break_a_shared_chain() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, m[20m])", 0.01),
                ("quantile_over_time(0.9, m[10m])", 0.01),
                ("quantile_over_time(0.99, m[15m])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let grids = [Some(120_000), Some(120_000), Some(180_000)];
        assert_eq!(
            panes(&roots, &grids),
            [(120_000, 10), (120_000, 5), (180_000, 5)]
        );
        let rule_candidates = pane_candidates(&roots, &grids);
        let shared = window_composition_pass2_candidates(&roots, &rule_candidates)
            .unwrap()
            .into_iter()
            .find(|c| c.pane_candidates.len() == 3 && !c.shared_chains.is_empty())
            .expect("a shared chain among all three builds in panes");
        assert_eq!(
            shared.shared_chains,
            [SharedPaneChain {
                members: vec![0, 1],
                pane_width_ms: 120_000,
                pane_count: 10
            }]
        );
        assert_eq!(chains(shared.roots.as_ref().unwrap()).len(), 2);
    }

    /// The shared chains (width, count) of every Pass 2 candidate that
    /// paned all of `rule_candidates`.
    fn shared_grids(
        roots: &[QueryRoot],
        rule_candidates: &[PaneCandidate],
    ) -> Vec<Vec<(u64, u32)>> {
        window_composition_pass2_candidates(roots, rule_candidates)
            .unwrap()
            .into_iter()
            .filter(|c| c.pane_candidates.len() == rule_candidates.len())
            .map(|c| {
                c.shared_chains
                    .iter()
                    .map(|shared| (shared.pane_width_ms, shared.pane_count))
                    .collect()
            })
            .collect()
    }

    /// Panes neither member's divide (3-min and 5-min panes of a 15-min
    /// window) are shared at their gcd, 1 min, as one more candidate beside
    /// the per-query chains; no member's own width serves both.
    #[test]
    fn a_shared_grid_at_the_gcd_of_the_members_widths_is_offered() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, m[15m])", 0.01),
                ("quantile_over_time(0.9, m[15m])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let grids = [Some(180_000), Some(300_000)];
        assert_eq!(panes(&roots, &grids), [(180_000, 5), (300_000, 3)]);
        let rule_candidates = pane_candidates(&roots, &grids);
        assert_eq!(
            shared_grids(&roots, &rule_candidates),
            [vec![], vec![(60_000, 15)]]
        );
    }

    /// A 15-min quantile every minute (15 × 1-min panes) and a 6-min one
    /// every 90 s (4 × 90-s panes): one chain of 30-s panes over 15 min
    /// serves both, the 6-min query merging the newest 12. It comes beside
    /// the per-query chains, and Stage 3 prices which is cheaper.
    #[test]
    fn queries_at_60_and_90_seconds_share_one_chain_of_30_second_panes() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, m[15m])", 0.01),
                ("quantile_over_time(0.9, m[6m])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let grids = [Some(60_000), Some(90_000)];
        assert_eq!(panes(&roots, &grids), [(60_000, 15), (90_000, 4)]);
        let rule_candidates = pane_candidates(&roots, &grids);
        assert_eq!(
            shared_grids(&roots, &rule_candidates),
            [vec![], vec![(30_000, 30)]]
        );
        let shared = window_composition_pass2_candidates(&roots, &rule_candidates)
            .unwrap()
            .into_iter()
            .find(|c| !c.shared_chains.is_empty())
            .unwrap();
        let shared_roots = shared.roots.unwrap();
        assert_eq!(newest_panes(&shared_roots[0]), None);
        assert_eq!(newest_panes(&shared_roots[1]), Some(12));
        for (root, whole) in shared_roots.iter().zip(&roots) {
            operator(root).validate_structure().unwrap();
            assert_eq!(operator(root).guarantee, operator(whole).guarantee);
        }
    }

    /// When the members' finest width divides the others, the gcd is that
    /// width: one shared candidate, not two equal ones. With 1-min, 2-min
    /// and 3-min panes, the divisible group (1 and 2 min, the 3-min build
    /// joining at 1 min too) is already the gcd grid.
    #[test]
    fn the_gcd_grid_is_not_listed_twice_when_a_member_width_is_the_gcd() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, m[12m])", 0.01),
                ("quantile_over_time(0.9, m[12m])", 0.01),
                ("quantile_over_time(0.99, m[12m])", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let grids = [Some(60_000), Some(120_000), Some(180_000)];
        let rule_candidates = pane_candidates(&roots, &grids);
        assert_eq!(
            shared_grids(&roots, &rule_candidates),
            [vec![], vec![(60_000, 12)]]
        );
    }

    /// The shared chain is sized for its strictest reader
    /// ([`size_for_all_readers`]): a 1-min quantile at ε = 0.01 and a
    /// 5-min one at ε = 0.05 read one chain of the 1-min query's `k`.
    #[test]
    fn a_shared_chain_is_sized_for_its_strictest_reader() {
        let roots = chose(
            &[
                ("quantile_over_time(0.5, m[1m])", 0.01),
                ("quantile_over_time(0.9, m[5m])", 0.05),
            ],
            SketchAlgorithm::Kll,
        );
        let family = |node: &OperatorNode| match node.asap() {
            Some(ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. }) => {
                family.clone()
            }
            _ => unreachable!(),
        };
        let strict = family(&builds(&roots[0])[0]);
        assert_ne!(strict, family(&builds(&roots[1])[0]));
        let rule_candidates = pane_candidates(&roots, &[Some(10_000), Some(10_000)]);
        let pass2_candidates =
            window_composition_pass2_candidates(&roots, &rule_candidates).unwrap();
        let shared = pass2_candidates
            .iter()
            .find(|c| !c.shared_chains.is_empty())
            .expect("a shared-chain candidate");
        let shared_roots = shared.roots.as_ref().unwrap();
        let [chain] = chains(shared_roots)
            .try_into()
            .unwrap_or_else(|c: Vec<_>| panic!("one chain, found {}", c.len()));
        assert_eq!(family(&chain), strict);
        // The 1-min query keeps its own guarantee; the 5-min one reads the
        // stricter state, so its guarantee is the 1-min build's size's,
        // not its own looser one.
        let guarantee = |root: &QueryRoot| operator(root).guarantee.clone().unwrap();
        assert_eq!(guarantee(&shared_roots[0]), guarantee(&roots[0]));
        assert_ne!(guarantee(&shared_roots[1]), guarantee(&roots[1]));
        assert_eq!(
            guarantee(&shared_roots[1]).spec,
            guarantee(&shared_roots[0]).spec
        );
    }
    /// The composed candidate of `queries` with every target at its exact
    /// option (an exact accumulator, or plain exact execution).
    fn exact(queries: &[&str]) -> Vec<QueryRoot> {
        use crate::per_computation_candidates::ComputationCandidate as C;
        let queries: Vec<(&str, f64)> = queries.iter().map(|q| (*q, 0.01)).collect();
        chose_where(&queries, |c| matches!(c, C::Exact | C::ExactAccumulator(_)))
    }

    fn operator(root: &QueryRoot) -> &Rc<OperatorNode> {
        let QueryRoot::Operator(node) = root else {
            panic!("operator root")
        };
        node
    }

    fn builds(root: &QueryRoot) -> Vec<Rc<OperatorNode>> {
        OperatorNode::reachable(operator(root))
            .into_iter()
            .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .collect()
    }

    /// `sum by (job)` is answered from `sum by (job, instance)`, which the
    /// workload computes anyway: the coarse query reads the finer query's
    /// finalized sums, summed by `job`, so it builds nothing over the raw
    /// rows; both values stay exact and keep their columns.
    #[test]
    fn rollup_from_a_finer_exact_sum() {
        let roots = exact(&["sum by (job) (m)", "sum by (job, instance) (m)"]);
        let rule_candidates = rollup_candidates(&roots);
        let [rule_candidate] = rule_candidates.as_slice() else {
            panic!("one candidate: {rule_candidates:?}")
        };
        assert!(Rc::ptr_eq(&rule_candidate.coarse, &builds(&roots[0])[0]));
        let pass2_candidates = rollup_pass2_candidates(&roots, &rule_candidates).unwrap();
        let [pass2_candidate] = pass2_candidates.as_slice() else {
            panic!("one Pass 2 candidate")
        };
        let rolled = pass2_candidate.roots.as_ref().unwrap();
        let coarse = builds(&rolled[0]);
        let fine = builds(&rolled[1]);
        assert_eq!(fine.len(), 1);
        // The finer build, then the sum over its values.
        assert_eq!(coarse.len(), 2);
        assert!(Rc::ptr_eq(&coarse[0], &fine[0]) || Rc::ptr_eq(&coarse[1], &fine[0]));
        let rollup = coarse.iter().find(|b| !Rc::ptr_eq(b, &fine[0])).unwrap();
        let Some(ASAPOp::SummaryAgg { child, family, .. }) = rollup.asap() else {
            unreachable!()
        };
        assert!(matches!(
            child.asap(),
            Some(ASAPOp::FinalizeExactAccumulator { .. })
        ));
        assert_eq!(
            *family,
            FieldDataType::ExactAggregate(ExactKind::Sum, asap_types::ir::schema::ExactParams::Sum)
        );
        for (before, after) in roots.iter().zip(rolled) {
            let (before, after) = (operator(before), operator(after));
            after.validate_structure().unwrap();
            assert_eq!(before.schema, after.schema);
            assert!(after
                .guarantee
                .as_ref()
                .is_some_and(asap_types::ir::properties::AccuracyGuarantee::is_exact));
        }
    }

    /// Minima roll up as minima, maxima as maxima. Integer counts do not
    /// roll up: exact accumulators execute only Float64 updates.
    #[test]
    fn extrema_roll_up_and_integer_counts_do_not() {
        assert!(rollup_candidates(&exact(&[
            "count by (job) (m)",
            "count by (job, instance) (m)"
        ]))
        .is_empty());
        for (query, kind) in [
            ("min by (job) (m)", ExactKind::Min),
            ("max by (job) (m)", ExactKind::Max),
        ] {
            let finer = query.replace("(job)", "(job, instance)");
            let roots = exact(&[query, &finer]);
            let rule_candidates = rollup_candidates(&roots);
            let [rule_candidate] = rule_candidates.as_slice() else {
                panic!("{query}: one candidate: {rule_candidates:?}")
            };
            let mut value = Rc::clone(&rule_candidate.rollup);
            if let Some(NonASAPOp::Project { child, .. }) = value.non_asap() {
                value = Rc::clone(child);
            }
            let Some(ASAPOp::FinalizeExactAccumulator { child: state }) = value.asap() else {
                panic!("a finalized value")
            };
            let Some(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(rolled, _),
                ..
            }) = state.asap()
            else {
                panic!("an exact accumulator")
            };
            assert_eq!(*rolled, kind, "{query}");
            let pass2_candidate = rollup_pass2_candidates(&roots, &rule_candidates)
                .unwrap()
                .remove(0);
            assert_eq!(
                operator(&pass2_candidate.roots.unwrap()[0]).schema,
                operator(&roots[0]).schema,
                "{query}"
            );
        }
    }

    /// The rule reuses only a finer computation the workload already has:
    /// keys that are not a superset, another metric, window or measure, or
    /// a lone query, give no pane candidate.
    #[test]
    fn rollup_needs_a_finer_computation_of_the_same_rows() {
        for queries in [
            &["sum by (job) (m)"][..],
            &["sum by (job) (m)", "sum by (instance) (m)"],
            &["sum by (job) (m)", "sum by (job, instance) (n)"],
            &["sum by (job) (m)", "max by (job, instance) (m)"],
            &[
                "sum by (job) (sum_over_time(m[5m]))",
                "sum by (job, instance) (sum_over_time(m[1m]))",
            ],
            &["sum by (job) (m)", "sum by (job) (m)"],
            &["sum by (job) (m)", "sum without (instance) (m)"],
            &["sum without (job) (m)", "sum by (job, instance) (m)"],
        ] {
            assert!(rollup_candidates(&exact(queries)).is_empty(), "{queries:?}");
        }
    }

    /// Rollup follows the family-capability table: sums, counts and
    /// extrema merge exactly across groups; counter states merge only in
    /// time, IRate not at all; a heap sketch's merged top-k has no
    /// guarantee, and no sketch's states can be regrouped in the IR.
    #[test]
    fn rollup_is_refused_for_families_without_an_exact_merge_across_groups() {
        use asap_types::ir::schema::{ExactParams as P, SketchParams};
        let exact = |kind, params| FieldDataType::ExactAggregate(kind, params);
        assert_eq!(
            rollup_kind(&exact(ExactKind::Sum, P::Sum)),
            Some(ExactKind::Sum)
        );
        assert_eq!(
            rollup_kind(&exact(ExactKind::Count, P::Count)),
            Some(ExactKind::Sum)
        );
        for family in [
            exact(ExactKind::Rate, P::Rate),
            exact(ExactKind::Increase, P::Increase),
            exact(ExactKind::IRate, P::IRate),
            FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::CmsWithHeap,
                    SketchParams::CmsWithHeap {
                        width: 64,
                        depth: 5,
                        heap_size: 10,
                    },
                ),
                GroupingStrategy::default(),
            ),
            FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::Kll,
                    SketchParams::Kll {
                        k: 200,
                        failure_probability: None,
                    },
                ),
                GroupingStrategy::default(),
            ),
        ] {
            assert_eq!(rollup_kind(&family), None, "{family:?}");
        }
    }

    /// #840's `quantile by (host, job)` and `quantile by (job)`, both KLL,
    /// the coarse one at a tighter ε: the coarse query reads the regrouping
    /// merge of the finer KLL's states by `job`, so one KLL is built, sized
    /// for its strictest reader (the larger `k`), and the coarse query keeps
    /// a guarantee within its own target.
    #[test]
    fn a_coarse_sketch_rolls_up_from_the_finer_build() {
        let roots = chose(
            &[
                ("quantile by (host, job) (0.5, data)", 0.01),
                ("quantile by (job) (0.5, data)", 0.001),
            ],
            SketchAlgorithm::Kll,
        );
        let ks: Vec<u32> = kll_builds(&roots).into_iter().map(|(_, k)| k).collect();
        let rule_candidates = rollup_candidates(&roots);
        let [rule_candidate] = rule_candidates.as_slice() else {
            panic!("one candidate: {rule_candidates:?}")
        };
        assert!(Rc::ptr_eq(&rule_candidate.coarse, &builds(&roots[1])[0]));
        assert!(Rc::ptr_eq(&rule_candidate.finer, &builds(&roots[0])[0]));
        let pass2_candidates = rollup_pass2_candidates(&roots, &rule_candidates).unwrap();
        let [pass2_candidate] = pass2_candidates.as_slice() else {
            panic!("one Pass 2 candidate")
        };
        let rolled = pass2_candidate.roots.as_ref().unwrap();
        // One KLL, at the larger k, read by both queries.
        assert_eq!(
            kll_builds(rolled)
                .into_iter()
                .map(|(_, k)| k)
                .collect::<Vec<_>>(),
            [*ks.iter().max().unwrap()]
        );
        let merge = OperatorNode::reachable(operator(&rolled[1]))
            .into_iter()
            .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryMerge { .. })))
            .expect("the coarse query reads a rollup");
        let Some(ASAPOp::SummaryMerge {
            children, group_by, ..
        }) = merge.asap()
        else {
            unreachable!()
        };
        assert_eq!(group_by, &Some(vec![1]));
        assert!(Rc::ptr_eq(&children[0], &builds(&rolled[0])[0]));
        for (before, after) in roots.iter().zip(rolled) {
            let (before, after) = (operator(before), operator(after));
            after.validate_structure().unwrap();
            assert_eq!(before.schema, after.schema);
        }
        // Each query's guarantee is the shared KLL's, within its target.
        assert!(
            root_bound(&rolled[1]) <= 0.001,
            "{}",
            root_bound(&rolled[1])
        );
        assert!(root_bound(&rolled[0]) <= 0.001);
    }

    /// Every coarser grouping of one finest build is one choice: two coarse
    /// builds of a three-grouping chain give one Pass 2 candidate, in which
    /// both read the finest build's states. A finer build that is itself
    /// coarse is not a source.
    #[test]
    fn coarse_groupings_of_one_finest_build_roll_up_together() {
        let roots = chose(
            &[
                ("quantile by (job) (0.9, m)", 0.01),
                ("quantile by (job, instance) (0.9, m)", 0.01),
                ("quantile by (job, instance, zone) (0.9, m)", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let rule_candidates = rollup_candidates(&roots);
        assert_eq!(rule_candidates.len(), 2);
        let finest = &builds(&roots[2])[0];
        assert!(rule_candidates
            .iter()
            .all(|candidate| Rc::ptr_eq(&candidate.finer, finest)));
        assert_eq!(rollup_choices(&rule_candidates).len(), 1);
        let pass2_candidates = rollup_pass2_candidates(&roots, &rule_candidates).unwrap();
        let [pass2_candidate] = pass2_candidates.as_slice() else {
            panic!("one Pass 2 candidate")
        };
        let rolled = pass2_candidate.roots.as_ref().unwrap();
        assert_eq!(kll_builds(rolled).len(), 1);
    }

    /// A quantile over every series rolls up from the KLLs by job: a
    /// rollup to no keys.
    #[test]
    fn a_global_quantile_rolls_up_to_no_keys() {
        let roots = chose(
            &[
                ("quantile(0.9, m)", 0.01),
                ("quantile by (job) (0.9, m)", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        let rule_candidates = rollup_candidates(&roots);
        let [rule_candidate] = rule_candidates.as_slice() else {
            panic!("one candidate: {rule_candidates:?}")
        };
        assert!(matches!(
            rule_candidate.rollup.asap(),
            Some(ASAPOp::SummaryMerge { group_by: Some(keys), .. }) if keys.is_empty()
        ));
        let pass2 = rollup_pass2_candidates(&roots, &rule_candidates).unwrap();
        let rolled = pass2[0].roots.as_ref().unwrap();
        assert_eq!(kll_builds(rolled).len(), 1);
        operator(&rolled[0]).validate_structure().unwrap();
    }

    /// Groupings that are not nested: `by (job)` rolls up from `by (job,
    /// instance)` or `by (job, zone)`; the source is the one whose sorted
    /// key names come first, whatever the query order.
    #[test]
    fn the_rollup_source_does_not_depend_on_query_order() {
        for queries in [
            [
                "quantile by (job) (0.9, m)",
                "quantile by (job, zone) (0.9, m)",
                "quantile by (job, instance) (0.9, m)",
            ],
            [
                "quantile by (job) (0.9, m)",
                "quantile by (job, instance) (0.9, m)",
                "quantile by (job, zone) (0.9, m)",
            ],
        ] {
            let roots = chose(&queries.map(|q| (q, 0.01)), SketchAlgorithm::Kll);
            let rule_candidates = rollup_candidates(&roots);
            let [rule_candidate] = rule_candidates.as_slice() else {
                panic!("one candidate: {rule_candidates:?}")
            };
            let instance = queries.iter().position(|q| q.contains("instance")).unwrap();
            assert!(Rc::ptr_eq(
                &rule_candidate.finer,
                &builds(&roots[instance])[0]
            ));
        }
    }

    /// A rollup needs the same rows: a coarse KLL over another metric is
    /// none.
    #[test]
    fn sketch_rollup_needs_the_same_rows() {
        let other_metric = chose(
            &[
                ("quantile by (job) (0.9, n)", 0.01),
                ("quantile by (job, instance) (0.9, m)", 0.01),
            ],
            SketchAlgorithm::Kll,
        );
        assert!(rollup_candidates(&other_metric).is_empty());
    }

    /// Two exact top-k over the same per-job sums: both read one ranking,
    /// the 10 highest sums, and the top-5 is computed from it; each keeps
    /// its `k`, output and exact value.
    #[test]
    fn exact_topk_reuses_the_larger_ranking() {
        let roots = exact(&["topk(5, sum by (job) (m))", "topk(10, sum by (job) (m))"]);
        let rule_candidates = topk_reuse_candidates(&roots);
        let [rule_candidate] = rule_candidates.as_slice() else {
            panic!("one candidate: {rule_candidates:?}")
        };
        assert_eq!(rule_candidate.members.len(), 2);
        let Some(NonASAPOp::Limit { n: Some(10), .. }) = rule_candidate.ranking.non_asap() else {
            panic!("{:?}", rule_candidate.ranking.operator)
        };
        let pass2_candidate = topk_reuse_pass2_candidates(&roots, &rule_candidates)
            .unwrap()
            .remove(0);
        let reused = pass2_candidate.roots.unwrap();
        let read = |root: &QueryRoot| {
            let Some(NonASAPOp::Aggregate {
                child, measures, ..
            }) = operator(root).non_asap()
            else {
                panic!("top-k aggregate")
            };
            (Rc::clone(child), measures[0].clone())
        };
        let ((a, small), (b, large)) = (read(&reused[0]), read(&reused[1]));
        assert!(Rc::ptr_eq(&a, &b), "one ranking");
        assert!(matches!(small, AggIntent::TopK { k: 5, .. }));
        assert!(matches!(large, AggIntent::TopK { k: 10, .. }));
        for (before, after) in roots.iter().zip(&reused) {
            let (before, after) = (operator(before), operator(after));
            after.validate_structure().unwrap();
            assert_eq!(before.schema, after.schema);
            assert!(after
                .guarantee
                .as_ref()
                .is_some_and(asap_types::ir::properties::AccuracyGuarantee::is_exact));
        }
    }

    /// Top-k over other partitions or inputs, or of one `k`, share no
    /// ranking; a top-k a heap sketch answers is not reused.
    #[test]
    fn topk_reuse_needs_exact_topk_of_one_input_and_partition() {
        for queries in [
            &[
                "topk(5, sum by (job) (m))",
                "topk by (job) (10, sum by (job, instance) (m))",
            ][..],
            &["topk(5, sum by (job) (m))", "topk(10, sum by (job) (n))"],
            &["topk(5, sum by (job) (m))", "topk(5, max by (job) (m))"],
            &[
                "topk(5, sum by (job) (m))",
                "topk by (job) (10, sum by (job) (m))",
            ],
        ] {
            assert!(
                topk_reuse_candidates(&exact(queries)).is_empty(),
                "{queries:?}"
            );
        }
        use crate::per_computation_candidates::{
            compose_logical_candidate, generate_per_computation_candidates,
            ComputationCandidate as C,
        };
        let inventory = generate_per_computation_candidates(
            [
                "topk by (job) (5, sum_over_time(m[1m]))",
                "topk by (job) (10, sum_over_time(m[1m]))",
            ]
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let root = lower_promql(
                    q,
                    asap_types::ir::properties::AccuracyTarget::max_error(0.01)
                        .failure_probability(0.01),
                );
                (i, QueryRoot::Operator(root))
            })
            .collect(),
        )
        .unwrap();
        // The CountSketch heap, the top-k's fused whole-expression one.
        let heap = |c: &C| matches!(c, C::Summary(SketchAlgorithm::CountSketchWithHeap));
        let choice: Vec<usize> = inventory
            .targets
            .iter()
            .map(|t| {
                (0..t.choice_count())
                    .position(|i| t.computation_for(i).is_some_and(|(c, _)| heap(c)))
                    .unwrap_or(0)
            })
            .collect();
        let heaps: Vec<QueryRoot> = compose_logical_candidate(&inventory, &choice)
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        assert!(OperatorNode::reachable(operator(&heaps[0]))
            .iter()
            .any(|n| matches!(n.asap(), Some(ASAPOp::SummaryEstimate { .. }))));
        assert!(topk_reuse_candidates(&heaps).is_empty());
    }
}
