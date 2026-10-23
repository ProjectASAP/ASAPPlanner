//! Final summary sizes in the memo MILP (sizing once): each choice is its
//! candidate with its summaries at one final size, so its coefficients and
//! the accuracy rules read the size Stage 3 takes.
//!
//! The final size of a summary is its Pass 1 (or shared-build threshold)
//! size enlarged for the smallest `δ_q / (n_q · V_q)` among the queries `q`
//! reaching it ([`crate::resize_for_failure_probability`]): `δ_q / V_q` is
//! the query's ([`crate::failure_probability_per_value`]) and `n_q` the
//! number of summaries its plan's value composes, which depends on the
//! plan. So each candidate is offered at every such size its group's
//! queries may ask for (`levels`), and a choice at a size admits, per
//! query, the largest `n_q` its summaries still meet (`input_limits`):
//! the model links it to the summaries the query's chosen plan reaches.
//!
//! Every limit is one the plan Stage 3 sizes satisfies: `n_q` counts
//! distinct summaries (never more than Stage 3's), `V_q` is the smallest
//! a candidate's root gives, and a query whose plan may be exact over
//! approximate inputs (a count of rows), whose summaries Stage 3 then does
//! not enlarge for it, gets none. So the model stays a lower bound.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use asap_logical_asap_optimization::per_computation_candidates::{
    recompose_guarantees, with_resized_summaries,
};
use asap_types::ir::schema::{FieldDataType, SketchParams};
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::search::memo::{CouplingKind, ImplementationCandidate, Memo};

/// What sizing once reads from the memo, computed once per memo.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sizing {
    /// Per query, `δ_q / V_q` for each `V_q` a root of it may give, the
    /// largest first; empty when its summaries are not enlarged for it, or
    /// its plan may be exact over approximate inputs.
    pub per_value: Vec<Vec<f64>>,
    /// Per query, at most how many summaries its plan's value composes:
    /// the most any plan of each of its root groups composes.
    pub max_inputs: Vec<usize>,
    /// Per group, the per-input failure probabilities its summaries may be
    /// sized for, the largest first. Groups whose builds one coupling site
    /// shares share them, so their readers may take one size.
    pub levels: Vec<Vec<f64>>,
}

/// The summary states `candidate` builds itself (not in a child group),
/// each once, in the order they are first reached.
pub(crate) fn private_summaries(
    memo: &Memo,
    candidate: &ImplementationCandidate,
) -> Vec<Rc<OperatorNode>> {
    let children: HashSet<*const OperatorNode> = candidate
        .children
        .iter()
        .map(|h| Rc::as_ptr(&memo.groups[h.0 as usize].node))
        .collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut stack = vec![Rc::clone(&candidate.node)];
    while let Some(node) = stack.pop() {
        if children.contains(&Rc::as_ptr(&node)) || !seen.insert(Rc::as_ptr(&node)) {
            continue;
        }
        if summary_params(&node).is_some() {
            out.push(Rc::clone(&node));
        }
        stack.extend(node.children().into_iter().rev().cloned());
    }
    out
}

/// A sketch summary state's parameters; `None` for any other node.
fn summary_params(node: &OperatorNode) -> Option<&SketchParams> {
    match node.asap()? {
        ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(kind, _),
            ..
        }
        | ASAPOp::PaneChain {
            family: FieldDataType::Sketch(kind, _),
            ..
        } => Some(kind.params()),
        _ => None,
    }
}

/// Per group, whether a candidate reading it is exact over one of its
/// approximate candidates (a count of rows): a plan may read it while its
/// queries' values ask nothing of it.
pub(crate) fn read_through_exact(memo: &Memo) -> Vec<bool> {
    let guarantee = |node: &Rc<OperatorNode>| {
        recompose_guarantees(&[QueryRoot::Operator(Rc::clone(node))])
            .ok()
            .and_then(|roots| match &roots[0] {
                QueryRoot::Operator(root) => root.guarantee.clone(),
                _ => None,
            })
    };
    let exact = |node: &Rc<OperatorNode>| guarantee(node).is_some_and(|g| g.is_exact());
    let mut out = vec![false; memo.groups.len()];
    for candidates in &memo.candidates {
        for candidate in candidates {
            for h in &candidate.children {
                let h = h.0 as usize;
                if out[h] {
                    continue;
                }
                let child_node = Rc::as_ptr(&memo.groups[h].node);
                out[h] = memo.candidates[h].iter().any(|d| {
                    let replaced = HashMap::from([(child_node, Rc::clone(&d.node))]);
                    let composed = crate::search::memo::brute_force::substituted(
                        &candidate.node,
                        &replaced,
                        &mut HashMap::new(),
                    );
                    !exact(&d.node) && exact(&composed)
                });
            }
        }
    }
    out
}

/// What sizing once reads from `memo` for `entries` over `data`.
pub(crate) fn sizing(memo: &Memo, entries: &[QueryWorkloadEntry], data: &DataWorkload) -> Sizing {
    let query_count = entries.len();
    let through_exact = read_through_exact(memo);
    let mut may_be_exact = vec![false; query_count];
    for (g, group) in memo.groups.iter().enumerate() {
        if through_exact[g] {
            for &q in &group.queries {
                may_be_exact[q] = true;
            }
        }
    }
    // Each query's roots: as given, and each candidate of its root groups.
    let mut roots: Vec<Vec<QueryRoot>> = (0..query_count)
        .map(|q| memo.roots.get(q).cloned().into_iter().collect())
        .collect();
    for (g, group) in memo.groups.iter().enumerate() {
        for &q in &group.roots_of {
            if !matches!(memo.roots.get(q), Some(QueryRoot::Operator(_))) {
                continue;
            }
            for candidate in &memo.candidates[g] {
                roots[q].push(QueryRoot::Operator(Rc::clone(&candidate.node)));
            }
        }
    }
    let per_value: Vec<Vec<f64>> = (0..query_count)
        .map(|q| {
            if may_be_exact[q] {
                return Vec::new();
            }
            let target = &entries[q].requirements.accuracy;
            let mut values: Vec<f64> = roots[q]
                .iter()
                .filter_map(|root| crate::failure_probability_per_value(root, target, data))
                .collect();
            values.sort_by(|a, b| b.total_cmp(a));
            values.dedup();
            values
        })
        .collect();
    // The most summaries a plan of each group composes: its candidate's
    // own and its children's, the most over its candidates.
    let mut most_below = vec![0usize; memo.groups.len()];
    let order: Vec<usize> = match memo.topological_order() {
        Some(order) => order.into_iter().map(|g| g.0 as usize).collect(),
        None => (0..memo.groups.len()).collect(),
    };
    for g in order {
        most_below[g] = memo.candidates[g]
            .iter()
            .map(|c| {
                private_summaries(memo, c).len()
                    + c.children
                        .iter()
                        .map(|h| most_below[h.0 as usize])
                        .sum::<usize>()
            })
            .max()
            .unwrap_or(0);
    }
    let mut max_inputs = vec![0; query_count];
    for (g, group) in memo.groups.iter().enumerate() {
        for &q in &group.roots_of {
            max_inputs[q] += most_below[g];
        }
    }
    // Groups one site's shared build or chain links take one set of levels.
    let mut component: Vec<usize> = (0..memo.groups.len()).collect();
    fn find(component: &mut [usize], g: usize) -> usize {
        let mut root = g;
        while component[root] != root {
            root = component[root];
        }
        component[g] = root;
        root
    }
    let mut first_of_site: HashMap<u64, usize> = HashMap::new();
    for (g, candidates) in memo.candidates.iter().enumerate() {
        for candidate in candidates {
            for site in &candidate.coupling_sites {
                if !matches!(
                    site.kind,
                    CouplingKind::SharedBuildSize | CouplingKind::PaneChain
                ) {
                    continue;
                }
                let first = *first_of_site.entry(site.key).or_insert(g);
                let (a, b) = (find(&mut component, first), find(&mut component, g));
                component[a] = b;
            }
        }
    }
    let mut of_component: HashMap<usize, BTreeSet<u64>> = HashMap::new();
    for (g, group) in memo.groups.iter().enumerate() {
        let root = find(&mut component, g);
        let set = of_component.entry(root).or_default();
        for &q in &group.queries {
            for &p in &per_value[q] {
                for n in 1..=max_inputs[q].max(1) {
                    set.insert((p / n as f64).to_bits());
                }
            }
        }
    }
    let levels = (0..memo.groups.len())
        .map(|g| {
            let root = find(&mut component, g);
            let mut levels: Vec<f64> = of_component[&root]
                .iter()
                .map(|&bits| f64::from_bits(bits))
                .collect();
            levels.sort_by(|a, b| b.total_cmp(a));
            levels
        })
        .collect();
    Sizing {
        per_value,
        max_inputs,
        levels,
    }
}

/// `version` with its own summaries (not its children's) at each size
/// `levels` gives them, each distinct size once: first as it is (`None`),
/// then enlarged for each per-input failure probability, largest first.
pub(crate) fn at_levels(
    memo: &Memo,
    version: &ImplementationCandidate,
    levels: &[f64],
    data: &DataWorkload,
) -> Vec<(ImplementationCandidate, Option<f64>)> {
    let own = private_summaries(memo, version);
    let mut out = vec![(version.clone(), None)];
    if own.is_empty() {
        return out;
    }
    let own_set: HashSet<*const OperatorNode> = own.iter().map(Rc::as_ptr).collect();
    let mut sizes: Vec<Vec<SketchParams>> = vec![sizes_of(&own)];
    for &delta in levels {
        let resize = |state: &OperatorNode, params: &SketchParams| {
            if !own_set.contains(&std::ptr::from_ref(state)) {
                return None;
            }
            crate::params_for_failure_probability(state, params, delta, data)
        };
        let Ok(roots) =
            with_resized_summaries(&[QueryRoot::Operator(Rc::clone(&version.node))], &resize)
        else {
            continue;
        };
        let QueryRoot::Operator(node) = &roots[0] else {
            continue;
        };
        let resized = ImplementationCandidate {
            node: Rc::clone(node),
            ..version.clone()
        };
        let size = sizes_of(&private_summaries(memo, &resized));
        if sizes.contains(&size) {
            continue;
        }
        sizes.push(size);
        out.push((resized, Some(delta)));
    }
    out
}

fn sizes_of(states: &[Rc<OperatorNode>]) -> Vec<SketchParams> {
    states
        .iter()
        .filter_map(|s| summary_params(s).cloned())
        .collect()
}

/// Per query reaching `version`'s group, the most summaries its plan's
/// value may compose with `version`'s own summaries at their size: the
/// largest `n` for which none of them is enlarged for `δ_q / (n · V_q)`
/// (the largest `V_q`'s share, the smallest `V_q`). Only the queries it
/// limits below their [`Sizing::max_inputs`], with that limit.
pub(crate) fn input_limits(
    memo: &Memo,
    version: &ImplementationCandidate,
    queries: &[usize],
    sizing: &Sizing,
    data: &DataWorkload,
) -> Vec<(usize, usize)> {
    let own = private_summaries(memo, version);
    if own.is_empty() {
        return Vec::new();
    }
    let met = |delta: f64| {
        own.iter().all(|state| {
            let Some(params) = summary_params(state) else {
                return true;
            };
            crate::params_for_failure_probability(state, params, delta, data)
                .is_none_or(|enlarged| enlarged == *params)
        })
    };
    let mut out = Vec::new();
    for &q in queries {
        let Some(&per_value) = sizing.per_value[q].first() else {
            continue;
        };
        let most = sizing.max_inputs[q];
        let limit = (1..=most)
            .take_while(|&n| met(per_value / n as f64))
            .last()
            .unwrap_or(0);
        if limit < most {
            out.push((q, limit));
        }
    }
    out
}
