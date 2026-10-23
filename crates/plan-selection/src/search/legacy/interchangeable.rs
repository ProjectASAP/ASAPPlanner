//! Interchangeable queries (joint-planning-search design, method 4 for
//! one class of alternatives): queries that differ at most in a quantile
//! rank take one shared choice.
//!
//! Two queries are interchangeable when their workload entries are equal
//! but for the query text, neither has a latency bound, each reads only
//! its own Pass 1 targets, and for every choice of their own candidates
//! the two queries, each planned alone, compose to the same plan up to the
//! quantile rank `q` of their quantile computations and estimates; and, when
//! their ranks differ, no two members have the same rank and no other query
//! can share a node that carries one of their ranks (a ratio `q0.9 / q0.5`
//! shares the 0.5 query's exact quantile, not the 0.9 query's, so it tells
//! them apart; two 0.9 duplicates share theirs, which a 0.5 member cannot
//! join). Duplicates are a class of their own. Neither
//! a cost, a summary's size nor a check reads `q`: a quantile estimate is
//! priced per group, and its sketch is sized for the rank or value error,
//! the same at every rank.
//!
//! Swapping two members then maps every plan to one of the same price, and
//! every member contributes the same charges to whatever summary build it
//! reads. Under cost model 1 a
//! build's price is the cheapest of its materializations, and each of
//! those is affine in its number of readers from the class (rebuilt per
//! reader when not materialized; built once at ingestion, or once per
//! evaluation of the class's common recurrence, however many read it). A
//! minimum of affine functions is concave, so splitting the class over
//! several builds never costs less than putting all of it on the best one
//! of them: some cheapest plan has every member read the same summary
//! builds, with the same Pass 1 candidate. The default search keeps only
//! those plans:
//!
//! - Pass 1: a whole-workload choice must give every member of a class the
//!   same candidate for each of its targets ([`Classes::invalid_prefix`]);
//! - Pass 2: a Stage 1 candidate must have every member of a class read the
//!   same summary builds, by node ([`Classes::members_read_same_builds`]): the
//!   same builds shared, in tumbling panes or whole alike (a build over a
//!   member's quantile reads its rank and stays its own).
//!
//! The members' identical nodes are not made one node before the Pass 2
//! rules run: the identical-expression rule runs after the others, and
//! sharing first would reach plans in another order (two duplicates' builds
//! shared, then capability-shared with a third query's, is not a plan of
//! the exhaustive search and can cost more than the best one).
//!
//! The argument needs the sum of node charges: classes are formed only
//! under cost model 1 without a measured cost table (whose calibration is
//! fitted over every candidate), and a latency bound, which is not a
//! charge, keeps a query out of every class.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_logical_asap_optimization::asap_aware_cse::{sharing_candidates, with_union_scan_labels};
use asap_logical_asap_optimization::per_computation_candidates::{
    compose_logical_candidate, PerComputationCandidates,
};
use asap_types::ir::flat::flatten;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::SketchStatistic;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::workload::QueryWorkloadEntry;

use crate::search::legacy::query_candidates::{next_digits, query_digits};
use crate::{CostModel, PlanningModels};

/// The classes of interchangeable queries of one workload, each of two or
/// more queries.
pub(crate) struct Classes {
    /// Each class: its members' query indices, in workload order, and each
    /// member's own digits of a whole-workload choice, in increasing order.
    classes: Vec<Vec<(usize, Vec<usize>)>>,
}

impl Classes {
    /// No class: every query chooses on its own.
    pub(crate) fn none() -> Self {
        Self {
            classes: Vec::new(),
        }
    }

    /// The classes of interchangeable queries of `per_computation` (the
    /// Pass 1 inventory of the workload, already pruned for the deployment's
    /// capabilities); none unless `models` prices by cost model 1 without a
    /// measured cost table.
    pub(crate) fn of<Id: Clone>(
        per_computation: &PerComputationCandidates<Id>,
        entries: &[QueryWorkloadEntry],
        models: &PlanningModels<'_>,
    ) -> Self {
        let additive = matches!(
            models.plan_selection.cost_model,
            CostModel::UsageOverTime(_)
        ) && models
            .plan_selection
            .measured_operation_costs
            .operators
            .is_empty();
        if !additive {
            return Self::none();
        }
        let digits: Vec<Vec<usize>> = per_computation
            .roots
            .iter()
            .map(|(_, root)| query_digits(per_computation, root))
            .collect();
        // A query reading a target another query reads is in no class.
        let own = |q: usize| {
            !digits[q].is_empty()
                && (0..digits.len())
                    .all(|other| other == q || digits[other].iter().all(|d| !digits[q].contains(d)))
        };
        let mut classes: Vec<Vec<(usize, Vec<usize>)>> = Vec::new();
        // Each class's first member's plan alone, rank-masked, per own choice.
        let mut signatures: Vec<Vec<Option<serde_json::Value>>> = Vec::new();
        for (q, entry) in entries.iter().enumerate() {
            if entry.requirements.max_latency_ms.is_some() || !own(q) {
                continue;
            }
            let signature = alone_signature(per_computation, &digits[q], q, true);
            let found = classes
                .iter()
                .zip(&signatures)
                .position(|(class, first_signature)| {
                    let (first, first_digits) = &class[0];
                    same_entry(&entries[*first], entry)
                        && first_digits.len() == digits[q].len()
                        && first_digits.iter().zip(&digits[q]).all(|(&a, &b)| {
                            per_computation.targets[a].choice_count()
                                == per_computation.targets[b].choice_count()
                        })
                        && *first_signature == signature
                });
            match found {
                Some(c) => classes[c].push((q, digits[q].clone())),
                None => {
                    classes.push(vec![(q, digits[q].clone())]);
                    signatures.push(signature);
                }
            }
        }
        // Members of different ranks are interchangeable only if no two of
        // them have the same rank (a pair of duplicates can share its exact
        // quantile, which a third member of another rank cannot join) and
        // no other query can share a node that carries a member's rank (a
        // ratio `q0.9 / q0.5` can share the 0.5 member's exact quantile but
        // not the 0.9 member's). Duplicates within such a class form a class
        // of their own.
        let mut alike = Vec::new();
        for class in classes {
            if class.len() < 2 {
                continue;
            }
            let ranked: Vec<Vec<Option<serde_json::Value>>> = class
                .iter()
                .map(|(m, member_digits)| {
                    alone_signature(per_computation, member_digits, *m, false)
                })
                .collect();
            if ranked.iter().all(|signature| *signature == ranked[0]) {
                alike.push(class);
                continue;
            }
            let mut same_rank: Vec<Vec<(usize, Vec<usize>)>> = Vec::new();
            let mut rank_of: Vec<&Vec<Option<serde_json::Value>>> = Vec::new();
            for (member, signature) in class.iter().zip(&ranked) {
                match rank_of.iter().position(|seen| *seen == signature) {
                    Some(r) => same_rank[r].push(member.clone()),
                    None => {
                        rank_of.push(signature);
                        same_rank.push(vec![member.clone()]);
                    }
                }
            }
            if same_rank.iter().any(|members| members.len() > 1) {
                alike.extend(same_rank);
                continue;
            }
            let members: Vec<usize> = class.iter().map(|(m, _)| *m).collect();
            alike.push(
                class
                    .into_iter()
                    .filter(|(m, _)| {
                        (0..entries.len())
                            .filter(|x| !members.contains(x))
                            .all(|x| !shares_a_rank_node(per_computation, &digits, x, *m))
                    })
                    .collect(),
            );
        }
        let mut classes = alike;
        classes.retain(|class| class.len() >= 2);
        Self { classes }
    }

    /// `Some(n)` when every whole-workload choice that agrees with `choice`
    /// on its first `n` digits gives two members of a class different
    /// candidates; `None` when `choice` gives each class one.
    pub(crate) fn invalid_prefix(&self, choice: &[usize]) -> Option<usize> {
        self.classes
            .iter()
            .filter_map(|class| {
                let (_, first) = &class[0];
                class[1..]
                    .iter()
                    .filter_map(|(_, member)| {
                        // The first digit position where this member differs.
                        let position =
                            (0..first.len()).find(|&i| choice[first[i]] != choice[member[i]])?;
                        Some(first[position].max(member[position]) + 1)
                    })
                    .min()
            })
            .min()
    }

    /// Whether every member of each class reads the same summary builds
    /// (`SummaryAgg` and `PaneChain` nodes, by node) in the Stage 1
    /// candidate with `roots`, other than builds over a member's quantile
    /// (e.g. the sums of an average of per-series quantiles), which read
    /// its rank.
    pub(crate) fn members_read_same_builds(&self, roots: &[QueryRoot]) -> bool {
        if self.classes.is_empty() {
            return true;
        }
        // Whether a rank lies at or below each node, by node.
        let mut ranked: HashMap<*const OperatorNode, bool> = HashMap::new();
        let builds = |q: usize, ranked: &mut HashMap<*const OperatorNode, bool>| {
            root_nodes(&roots[q])
                .into_iter()
                .filter(|node| {
                    matches!(
                        node.asap(),
                        Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
                    ) && !rank_below(node, ranked)
                })
                .map(|node| Rc::as_ptr(&node))
                .collect::<HashSet<_>>()
        };
        self.classes.iter().all(|class| {
            let first = builds(class[0].0, &mut ranked);
            class[1..]
                .iter()
                .all(|(q, _)| builds(*q, &mut ranked) == first)
        })
    }

    /// Each class's members, by query index, for the search report.
    pub(crate) fn members(&self) -> Vec<Vec<usize>> {
        self.classes
            .iter()
            .map(|class| class.iter().map(|(q, _)| *q).collect())
            .collect()
    }
}

/// Whether `a` and `b` are the same workload entry but for the query text.
fn same_entry(a: &QueryWorkloadEntry, b: &QueryWorkloadEntry) -> bool {
    // Every field but the query text: a new field must be compared here.
    let QueryWorkloadEntry {
        query: _,
        requirements,
        predictability,
        recurrence,
        range_evaluation,
    } = a;
    *requirements == b.requirements
        && *predictability == b.predictability
        && *recurrence == b.recurrence
        && *range_evaluation == b.range_evaluation
}

/// Every node `root` reaches.
fn root_nodes(root: &QueryRoot) -> Vec<Rc<OperatorNode>> {
    let starts: Vec<&Rc<OperatorNode>> = match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    };
    starts
        .into_iter()
        .flat_map(OperatorNode::reachable)
        .collect()
}

/// Whether a node carrying a quantile rank lies at or below `node`,
/// memoized by node in `ranked`.
fn rank_below(node: &Rc<OperatorNode>, ranked: &mut HashMap<*const OperatorNode, bool>) -> bool {
    if let Some(&known) = ranked.get(&Rc::as_ptr(node)) {
        return known;
    }
    let below = carries_a_rank(node)
        || node
            .children()
            .into_iter()
            .any(|child| rank_below(child, ranked));
    ranked.insert(Rc::as_ptr(node), below);
    below
}

/// Query `q` of `per_computation` planned alone with each choice of its own
/// candidates (over `digits`, in mixed-radix order): the composed plan, with
/// every quantile rank masked when `masked`, or `None` when it does not
/// compose.
fn alone_signature<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    digits: &[usize],
    q: usize,
    masked: bool,
) -> Vec<Option<serde_json::Value>> {
    let alone = PerComputationCandidates {
        roots: vec![per_computation.roots[q].clone()],
        targets: per_computation.targets.clone(),
    };
    let mut choice = vec![0; per_computation.targets.len()];
    let mut signature = Vec::new();
    loop {
        signature.push(
            compose_logical_candidate(&alone, &choice)
                .ok()
                .and_then(|composed| {
                    let roots: Vec<QueryRoot> =
                        composed.into_iter().map(|(_, root)| root).collect();
                    let mut flat = serde_json::to_value(flatten(&roots).0).ok()?;
                    if masked {
                        mask_quantile_ranks(&mut flat);
                    }
                    Some(flat)
                }),
        );
        if !next_digits(per_computation, digits, &mut choice) {
            return signature;
        }
    }
}

/// Whether queries `x` and `m` of `per_computation`, for some choice of
/// their own candidates (over `digits[x]` and `digits[m]`) composed
/// together, have identical nodes that carry a quantile rank (with each
/// metric's scans widened to one label set): a node the identical-
/// expression rule could make one.
fn shares_a_rank_node<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    digits: &[Vec<usize>],
    x: usize,
    m: usize,
) -> bool {
    let pair = PerComputationCandidates {
        roots: vec![
            per_computation.roots[x].clone(),
            per_computation.roots[m].clone(),
        ],
        targets: per_computation.targets.clone(),
    };
    let pair_digits: Vec<usize> = digits[x].iter().chain(&digits[m]).copied().collect();
    let mut choice = vec![0; per_computation.targets.len()];
    loop {
        if let Ok(composed) = compose_logical_candidate(&pair, &choice) {
            let roots: Vec<QueryRoot> = composed.into_iter().map(|(_, root)| root).collect();
            let widened = with_union_scan_labels(&roots).ok().flatten();
            for roots in std::iter::once(&roots).chain(widened.as_ref()) {
                if sharing_candidates(roots)
                    .iter()
                    .any(|candidate| carries_a_rank(&candidate.nodes[0]))
                {
                    return true;
                }
            }
        }
        if !next_digits(per_computation, &pair_digits, &mut choice) {
            return false;
        }
    }
}

/// Whether `node` computes or estimates a quantile at some rank.
fn carries_a_rank(node: &OperatorNode) -> bool {
    match &node.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
            matches!(query, SketchStatistic::Quantile { .. })
        }
        Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) => measures
            .iter()
            .any(|measure| matches!(measure, AggIntent::Quantile { .. })),
        _ => false,
    }
}

/// `value`, a serialized plan, with the rank `q` of every quantile (a
/// quantile measure's or a sketch estimate's, and the estimate a
/// guarantee's provenance names) masked.
fn mask_quantile_ranks(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            // A sketch estimate's `{"Quantile": {"q": ..}}`.
            if let Some(serde_json::Value::Object(quantile)) = map.get_mut("Quantile") {
                if let Some(q) = quantile.get_mut("q") {
                    *q = serde_json::Value::from(0);
                }
            }
            // A quantile measure's `{"kind": "quantile", "q": .., ..}`.
            if map.get("kind").and_then(serde_json::Value::as_str) == Some("quantile") {
                if let Some(q) = map.get_mut("q") {
                    *q = serde_json::Value::from(0);
                }
            }
            map.values_mut().for_each(mask_quantile_ranks);
        }
        // A guarantee's provenance names the estimate it evaluates.
        serde_json::Value::String(text) if text.starts_with("Quantile { q: ") => {
            *text = "Quantile".to_string();
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(mask_quantile_ranks),
        _ => {}
    }
}
