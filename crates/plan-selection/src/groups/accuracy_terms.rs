//! Row (K)'s terms per query (module documentation of [`super`], "Row (K)
//! terms"), each checked by `asap-accuracy`'s composition and Stage 3's own
//! check ([`guarantee_check`]) of an output built over the candidates.
//!
//! For query `q`, a term is one path of edges `(u, c, i)` from an
//! approximate producer `u` up to an output candidate of `q`. A set `S` of
//! terms (one term, or a pair) is checked on the output `q` has with `u`'s
//! values approximate along exactly the paths of `S`: each consumer on a
//! path is its node rebuilt over the path's producer at the path's input
//! and an exact stand-in at every other input, with its guarantee composed
//! again ([`composed_guarantee`]), as the realizer recomposes a selected
//! plan; an output reference no path of `S` starts at reads exact
//! stand-ins of its output candidates. Each output is then checked as the
//! selected plan is: [`guarantee_check`] with the deployment's model.
//!
//! Terms whose paths go through the same operator nodes (candidates of one
//! operator node at other timings or materializations build the same node)
//! are checked once, together. What a check builds is kept for the later
//! checks ([`BuiltBefore`]): a consumer's outputs depend only on the paths
//! below it.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use asap_accuracy::propagation::{composed_guarantee, root_guarantee, with_guarantees};
use asap_types::ir::properties::AccuracyGuarantee;
use asap_types::ir::{OperatorNode, OperatorResultKind, QueryRoot};
use asap_types::stages::{output_references, PhysicalASAPMemoCandidates, PhysicalCandidateId};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::{AccuracyTerm, AccuracyWeight, CandidateData, UnusableReason};
use crate::checks::guarantee_check;
use crate::report::RejectionCause;
use crate::SelectionModels;

/// What row (K) is built from.
#[derive(Debug, Clone, Default)]
pub(crate) struct AccuracyTerms {
    /// Per query: its terms, one per path of edges from an approximate
    /// producer `u` whose values reach its output.
    pub(crate) terms: Vec<Vec<AccuracyTerm>>,
    /// Per query: the pairs `(t1, t2)`, `t1 < t2`, of indices into its
    /// terms, each with weight `One` alone, whose outputs miss `q`'s target when
    /// both are approximate.
    pub(crate) excluded_pairs: Vec<Vec<(usize, usize)>>,
}

/// The outcome of one check of a query with a set of terms approximate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Every output is exact: no value of the set reaches the output.
    Exact,
    /// Every output meets the target.
    Meets,
    Fails(UnusableReason),
}

/// [`AccuracyTerms`] of `physical`'s queries over the candidate links left
/// (`producer_candidates_per_input`, `output_candidates`).
pub(crate) fn accuracy_terms(
    physical: &PhysicalASAPMemoCandidates,
    candidate_data: &[CandidateData],
    producer_candidates_per_input: &[Vec<Vec<PhysicalCandidateId>>],
    output_candidates: &[Vec<PhysicalCandidateId>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &SelectionModels<'_>,
) -> AccuracyTerms {
    let below = approximate_below(candidate_data, producer_candidates_per_input);
    let output_refs = output_references(&physical.query_roots);
    let exact_stand_ins: Vec<std::cell::OnceCell<Rc<OperatorNode>>> =
        (0..physical.candidates.len())
            .map(|_| std::cell::OnceCell::new())
            .collect();
    let mut built_before = BuiltBefore::default();
    let mut result = AccuracyTerms::default();
    for (query, root) in physical.query_roots.iter().enumerate() {
        let references: Vec<usize> = (0..output_refs.len())
            .filter(|&reference| output_refs[reference].0 == query)
            .collect();
        let Some(entry) = entries.get(query) else {
            result.terms.push(Vec::new());
            result.excluded_pairs.push(Vec::new());
            continue;
        };
        let checker = Checker {
            physical,
            exact_stand_ins: &exact_stand_ins,
            producer_candidates_per_input,
            output_candidates,
            below: &below,
            query,
            root,
            references: &references,
            entry,
            data,
            models,
        };
        let paths = paths_to_approximate_producers(
            candidate_data,
            producer_candidates_per_input,
            output_candidates,
            &below,
            &references,
        );
        // Paths through the same operator nodes build the same outputs:
        // checked once, together.
        let mut outcomes: HashMap<NodePath, Outcome> = HashMap::new();
        let mut terms = Vec::new();
        for path in paths {
            let key = checker.node_path(&path);
            let outcome = *outcomes
                .entry(key)
                .or_insert_with(|| checker.check(&[&path], &mut built_before));
            let weight = match outcome {
                Outcome::Exact => continue,
                Outcome::Meets => AccuracyWeight::One,
                Outcome::Fails(reason) => AccuracyWeight::Unusable(reason),
            };
            terms.push(AccuracyTerm { weight, ..path });
        }
        let mut pair_outcomes: HashMap<(NodePath, NodePath), Outcome> = HashMap::new();
        let mut excluded = Vec::new();
        for first in 0..terms.len() {
            for second in first + 1..terms.len() {
                let (one, other) = (&terms[first], &terms[second]);
                // A pair with a term sized for fewer than two shares is
                // never read together: the shares row already excludes it.
                if [one, other].iter().any(|term| {
                    term.weight != AccuracyWeight::One
                        || candidate_data[term.producer().0].accuracy_budget_parts < 2
                }) || !read_together(one, other)
                {
                    continue;
                }
                let key = (checker.node_path(one), checker.node_path(other));
                let outcome = *pair_outcomes
                    .entry(key)
                    .or_insert_with(|| checker.check(&[one, other], &mut built_before));
                if let Outcome::Fails(_) = outcome {
                    excluded.push((first, second));
                }
            }
        }
        result.terms.push(terms);
        result.excluded_pairs.push(excluded);
    }
    result
}

/// Per candidate, the approximate candidates at or below it through the
/// links, ascending. A fixpoint, so cycles of candidate links are covered.
fn approximate_below(
    candidate_data: &[CandidateData],
    producer_candidates_per_input: &[Vec<Vec<PhysicalCandidateId>>],
) -> Vec<BTreeSet<usize>> {
    let mut below: Vec<BTreeSet<usize>> = candidate_data
        .iter()
        .enumerate()
        .map(|(class, data)| match data.approximate {
            true => BTreeSet::from([class]),
            false => BTreeSet::new(),
        })
        .collect();
    loop {
        let mut changed = false;
        for class in 0..below.len() {
            let mut grown = below[class].clone();
            for producer in producer_candidates_per_input[class].iter().flatten() {
                grown.extend(below[producer.0].iter().copied());
            }
            if grown.len() != below[class].len() {
                below[class] = grown;
                changed = true;
            }
        }
        if !changed {
            return below;
        }
    }
}

/// Every path of edges from an output candidate of `references` down to an
/// approximate producer, no candidate twice on one path (links can close a
/// cycle), in order of reference, then depth first by input and producer.
/// Each is returned as a term with weight `One`, set by the caller.
fn paths_to_approximate_producers(
    candidate_data: &[CandidateData],
    producer_candidates_per_input: &[Vec<Vec<PhysicalCandidateId>>],
    output_candidates: &[Vec<PhysicalCandidateId>],
    below: &[BTreeSet<usize>],
    references: &[usize],
) -> Vec<AccuracyTerm> {
    struct Walk<'a> {
        candidate_data: &'a [CandidateData],
        producer_candidates_per_input: &'a [Vec<Vec<PhysicalCandidateId>>],
        below: &'a [BTreeSet<usize>],
        reference: usize,
        found: Vec<AccuracyTerm>,
    }
    impl Walk<'_> {
        fn walk(&mut self, candidates: &mut Vec<PhysicalCandidateId>, inputs: &mut Vec<usize>) {
            let candidate = *candidates.last().expect("a path has a candidate");
            if self.candidate_data[candidate.0].approximate {
                self.found.push(AccuracyTerm {
                    output_reference: self.reference,
                    candidates: candidates.clone(),
                    inputs: inputs.clone(),
                    weight: AccuracyWeight::One,
                });
            }
            for (input, producers) in self.producer_candidates_per_input[candidate.0]
                .iter()
                .enumerate()
            {
                for &producer in producers {
                    if self.below[producer.0].is_empty() || candidates.contains(&producer) {
                        continue;
                    }
                    candidates.push(producer);
                    inputs.push(input);
                    self.walk(candidates, inputs);
                    candidates.pop();
                    inputs.pop();
                }
            }
        }
    }
    let mut walk = Walk {
        candidate_data,
        producer_candidates_per_input,
        below,
        reference: 0,
        found: Vec::new(),
    };
    for &reference in references {
        walk.reference = reference;
        for &output in &output_candidates[reference] {
            if !below[output.0].is_empty() {
                walk.walk(&mut vec![output], &mut Vec::new());
            }
        }
    }
    walk.found
}

/// Whether one plan of the query can read the values of both terms: their
/// paths start at different output references, or leave each other at
/// two different inputs of one consumer, or one path continues the other.
/// Two paths through different candidates of one output reference or of
/// one consumer's input are never in one plan.
pub(super) fn read_together(one: &AccuracyTerm, other: &AccuracyTerm) -> bool {
    if one.output_reference != other.output_reference {
        return true;
    }
    for depth in 0.. {
        match (one.candidates.get(depth), other.candidates.get(depth)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(_), Some(_)) => return false,
            _ => return true,
        }
        match (one.inputs.get(depth), other.inputs.get(depth)) {
            (Some(a), Some(b)) if a != b => return true,
            _ => {}
        }
    }
    unreachable!()
}

/// A term's path by operator nodes: `(output reference, operator node per
/// candidate, inputs)`.
type NodePath = (usize, Vec<usize>, Vec<usize>);

/// One query's checks.
struct Checker<'a> {
    physical: &'a PhysicalASAPMemoCandidates,
    /// Per candidate, its node with an exact guarantee, built once.
    exact_stand_ins: &'a [std::cell::OnceCell<Rc<OperatorNode>>],
    producer_candidates_per_input: &'a [Vec<Vec<PhysicalCandidateId>>],
    output_candidates: &'a [Vec<PhysicalCandidateId>],
    below: &'a [BTreeSet<usize>],
    query: usize,
    root: &'a QueryRoot<asap_types::stages::MemoGroupId>,
    references: &'a [usize],
    entry: &'a QueryWorkloadEntry,
    data: &'a DataWorkload,
    models: &'a SelectionModels<'a>,
}

/// The outputs one consumer is built as, or why they are not checked.
type Built = Result<Vec<Rc<OperatorNode>>, UnusableReason>;

/// The tails of the paths below one candidate: per path, its candidates
/// and inputs from that candidate down.
type Tails = Vec<(Vec<PhysicalCandidateId>, Vec<usize>)>;

/// What the checks built, kept across checks and queries: per candidate
/// and the tails of the approximate paths below it, its outputs.
#[derive(Default)]
struct BuiltBefore {
    outputs: HashMap<Tails, Built>,
    /// The [`reader_key`]s of every node built so far.
    keys: Keys,
}

#[cfg(test)]
thread_local! {
    /// Set by a test to check with no kept builds: every check builds
    /// from scratch ([`BuiltBefore`] cleared) and builds every output
    /// candidate of a reference no path starts at (no exact stand-in
    /// skipped).
    pub(crate) static FRESH_BUILDS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Checker<'_> {
    fn node_path(&self, term: &AccuracyTerm) -> NodePath {
        (
            term.output_reference,
            term.candidates
                .iter()
                .map(|candidate| self.physical.candidates[candidate.0].operator_node.0)
                .collect(),
            term.inputs.clone(),
        )
    }

    /// Checks the query's outputs with the values of `terms`' producers
    /// approximate along their paths. `before` holds what earlier checks
    /// built, for every query.
    fn check(&self, terms: &[&AccuracyTerm], before: &mut BuiltBefore) -> Outcome {
        #[cfg(test)]
        let fresh = FRESH_BUILDS.with(std::cell::Cell::get);
        #[cfg(not(test))]
        let fresh = false;
        if fresh {
            *before = BuiltBefore::default();
        }
        let mut per_reference: Vec<Vec<Rc<OperatorNode>>> = Vec::new();
        for &reference in self.references {
            let tails: Tails = terms
                .iter()
                .filter(|term| term.output_reference == reference)
                .map(|term| (term.candidates.clone(), term.inputs.clone()))
                .collect();
            let outputs = if tails.is_empty() {
                // Exact stand-ins that are not summary states are the same
                // for readers ([`reader_key`]): only the first is kept.
                let mut outputs = Vec::new();
                let mut exact_value_stand_in = false;
                for &class in &self.output_candidates[reference] {
                    if !fresh
                        && self.physical.physical_node(class).result_kind
                            != OperatorResultKind::State
                        && std::mem::replace(&mut exact_value_stand_in, true)
                    {
                        continue;
                    }
                    outputs.push(self.exact_stand_in(class.0));
                }
                outputs
            } else {
                match self.built(tails, before) {
                    Ok(built) => built,
                    Err(reason) => return Outcome::Fails(reason),
                }
            };
            per_reference.push(distinct_for_readers(outputs, &mut before.keys));
        }
        let roots = self.roots(&per_reference);
        let mut outcome = Outcome::Exact;
        for root in roots {
            if root_guarantee(&root).is_some_and(|g| g.is_exact()) {
                continue;
            }
            match guarantee_check(
                self.query + 1,
                &root,
                &self.entry.requirements.accuracy,
                self.data,
                self.models,
            ) {
                Ok(_) => outcome = Outcome::Meets,
                Err(failure) => {
                    return Outcome::Fails(match failure.cause {
                        RejectionCause::NoAccuracyGuarantee => UnusableReason::NoAccuracyGuarantee,
                        _ => UnusableReason::AccuracyTargetMissed,
                    })
                }
            }
        }
        outcome
    }

    /// The query roots over each combination of `per_reference`'s outputs.
    fn roots(&self, per_reference: &[Vec<Rc<OperatorNode>>]) -> Vec<QueryRoot> {
        combinations(per_reference)
            .into_iter()
            .map(|chosen| match self.root {
                QueryRoot::Operator(_) => QueryRoot::Operator(chosen[0].clone()),
                QueryRoot::Scalar(expr) => {
                    let mut next = chosen.into_iter();
                    QueryRoot::Scalar(
                        expr.map_operator_refs(&mut |_| {
                            next.next().expect("one output per reference")
                        }),
                    )
                }
            })
            .collect()
    }

    /// `class`'s node with an exact guarantee, built once.
    fn exact_stand_in(&self, class: usize) -> Rc<OperatorNode> {
        self.exact_stand_ins[class]
            .get_or_init(|| {
                let node = self.physical.physical_node(PhysicalCandidateId(class));
                Rc::new(
                    (**node)
                        .clone()
                        .with_guarantee(Some(AccuracyGuarantee::exact("an exact stand-in"))),
                )
            })
            .clone()
    }

    /// The outputs of the candidate every tail of `tails` starts at, with
    /// the values of each tail's last candidate approximate along it.
    fn built(&self, tails: Tails, before: &mut BuiltBefore) -> Built {
        if let Some(done) = before.outputs.get(&tails) {
            return done.clone();
        }
        let class = tails[0].0[0];
        let node = self.physical.physical_node(class);
        let continuing: Vec<&(Vec<PhysicalCandidateId>, Vec<usize>)> = tails
            .iter()
            .filter(|(candidates, _)| candidates.len() > 1)
            .collect();
        let built = if continuing.is_empty() {
            // `class` is the approximate producer of every tail, over exact
            // inputs: its node with every guarantee below it composed, as
            // the realizer does.
            with_guarantees(node, &mut HashMap::new())
                .map(|node| vec![node])
                .map_err(|_| UnusableReason::NotChecked)
        } else {
            let producers = &self.producer_candidates_per_input[class.0];
            let mut per_input = Vec::with_capacity(producers.len());
            let mut failed = None;
            for (input, producers) in producers.iter().enumerate() {
                let below: Tails = continuing
                    .iter()
                    .filter(|(_, inputs)| inputs[0] == input)
                    .map(|(candidates, inputs)| (candidates[1..].to_vec(), inputs[1..].to_vec()))
                    .collect();
                if below.is_empty() {
                    // One exact stand-in: a producer with no approximate
                    // candidate below, when there is one.
                    let exact = producers
                        .iter()
                        .min_by_key(|producer| !self.below[producer.0].is_empty());
                    per_input.push(
                        exact
                            .map(|exact| self.exact_stand_in(exact.0))
                            .into_iter()
                            .collect(),
                    );
                } else {
                    match self.built(below, before) {
                        Ok(built) => per_input.push(built),
                        Err(reason) => {
                            failed = Some(reason);
                            per_input.push(Vec::new());
                        }
                    }
                }
            }
            match failed {
                Some(reason) => Err(reason),
                None => rebuilt(node, &per_input, &mut before.keys),
            }
        };
        before.outputs.insert(tails, built.clone());
        built
    }
}

/// `node` rebuilt over each combination of `per_input`'s outputs, with its
/// timing and materialization and its guarantee composed again; outputs the
/// same for their readers kept once.
fn rebuilt(node: &Rc<OperatorNode>, per_input: &[Vec<Rc<OperatorNode>>], keys: &mut Keys) -> Built {
    let combinations = combinations(per_input);
    let mut outputs = Vec::with_capacity(combinations.len());
    for chosen in combinations {
        let mut next = chosen.into_iter();
        let rebuilt = node
            .with_new_children(|_| next.next().expect("one output per input"))
            .map_err(|_| UnusableReason::NotChecked)?;
        let guarantee = composed_guarantee(&rebuilt);
        outputs.push(Rc::new(
            rebuilt
                .with_guarantee(guarantee)
                .with_timing(node.timing)
                .with_materialization(node.materialization),
        ));
    }
    Ok(distinct_for_readers(outputs, keys))
}

/// Every choice of one entry per list, in order. One empty list gives none.
fn combinations<T: Clone>(lists: &[Vec<T>]) -> Vec<Vec<T>> {
    let mut combinations: Vec<Vec<T>> = vec![Vec::new()];
    for list in lists {
        combinations = combinations
            .into_iter()
            .flat_map(|prefix| {
                list.iter().map(move |item| {
                    let mut next = prefix.clone();
                    next.push(item.clone());
                    next
                })
            })
            .collect();
    }
    combinations
}

/// `outputs` with outputs the same for their readers kept once (the
/// first): equal [`reader_key`]s.
fn distinct_for_readers(outputs: Vec<Rc<OperatorNode>>, keys: &mut Keys) -> Vec<Rc<OperatorNode>> {
    let mut seen = HashSet::new();
    outputs
        .into_iter()
        .filter(|output| seen.insert(reader_key(output, keys)))
        .collect()
}

/// Per node (by address): its [`reader_key`], and whether binding
/// statistics leaves its guarantee as it is; the node is held so its
/// address is not reused during the check.
type Keys = HashMap<*const OperatorNode, (u64, bool, Rc<OperatorNode>)>;

/// A key equal for outputs over which readers compose the same guarantee:
/// an exact node as one key (an exact summary state, a stand-in, by its
/// address); a node with a guarantee (not a summary state) by its bound and failure
/// probability (binding keeps a node's guarantee and ignores its inputs;
/// provenance is not read); a node with none that binding cannot give one
/// as one key; any other node (a summary state, whose readers read its
/// inputs' guarantees, or a node with none that binding may still give
/// one) by its operator kind and its inputs' keys.
fn reader_key(node: &Rc<OperatorNode>, keys: &mut Keys) -> u64 {
    key_and_settled(node, keys).0
}

/// [`reader_key`], and whether binding statistics leaves `node`'s
/// guarantee as it is: a guarantee with no unknown statistic, or none over
/// inputs that are all settled.
fn key_and_settled(node: &Rc<OperatorNode>, keys: &mut Keys) -> (u64, bool) {
    if let Some(&(key, settled, _)) = keys.get(&Rc::as_ptr(node)) {
        return (key, settled);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Exact whatever its inputs (an exact stand-in among them).
    if node
        .guarantee
        .as_ref()
        .is_some_and(|guarantee| guarantee.is_exact())
    {
        "exact".hash(&mut hasher);
        if node.result_kind == OperatorResultKind::State {
            Rc::as_ptr(node).hash(&mut hasher);
        }
        let known = (hasher.finish(), true);
        keys.insert(Rc::as_ptr(node), (known.0, known.1, node.clone()));
        return known;
    }
    let spec = node.guarantee.as_ref().map(|guarantee| &guarantee.spec);
    format!("{spec:?}").hash(&mut hasher);
    let state = node.result_kind == OperatorResultKind::State;
    let known = match &node.guarantee {
        Some(guarantee) if !state => (
            hasher.finish(),
            guarantee.spec.error.evaluate().is_some()
                && guarantee.spec.failure_probability.evaluate().is_some(),
        ),
        _ => {
            let children: Vec<(u64, bool)> = node
                .children()
                .into_iter()
                .map(|child| key_and_settled(child, keys))
                .collect();
            let settled = node.guarantee.is_none() && children.iter().all(|(_, settled)| *settled);
            if !(settled && !state) {
                node.operator.kind_name().hash(&mut hasher);
                state.hash(&mut hasher);
                for (key, _) in &children {
                    key.hash(&mut hasher);
                }
            }
            (hasher.finish(), settled)
        }
    };
    keys.insert(Rc::as_ptr(node), (known.0, known.1, node.clone()));
    known
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(reference: usize, candidates: &[usize], inputs: &[usize]) -> AccuracyTerm {
        AccuracyTerm {
            output_reference: reference,
            candidates: candidates.iter().map(|&c| PhysicalCandidateId(c)).collect(),
            inputs: inputs.to_vec(),
            weight: AccuracyWeight::One,
        }
    }

    // One state `u = 9` read by both operands of a quotient `0` through a
    // shared merge `5`: `0` reads estimate `1` at input 0 and estimate `2`
    // at input 1, each over `5`. The two paths leave each other at `0`'s
    // two inputs, so one plan depends on both; two paths through different
    // candidates of one input, or of one output reference, are never in
    // one plan; a path ending on the other's path is.
    #[test]
    fn terms_read_together_where_their_paths_can_meet() {
        let numerator = term(0, &[0, 1, 5, 9], &[0, 0, 0]);
        let denominator = term(0, &[0, 2, 5, 9], &[1, 0, 0]);
        assert!(read_together(&numerator, &denominator));
        let other_estimate = term(0, &[0, 3, 5, 9], &[0, 0, 0]);
        assert!(!read_together(&numerator, &other_estimate));
        let other_output = term(0, &[4, 5, 9], &[0, 0]);
        assert!(!read_together(&numerator, &other_output));
        let above = term(0, &[0, 1, 5], &[0, 0]);
        assert!(read_together(&numerator, &above));
        let second_reference = term(1, &[4, 5, 9], &[0, 0]);
        assert!(read_together(&numerator, &second_reference));
    }
}
