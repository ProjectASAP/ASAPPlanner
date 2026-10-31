//! Accuracy weights and pair exclusions per query (module documentation of
//! [`super`], "Accuracy weights"), each from `asap-accuracy`'s composition
//! and Stage 3's own check ([`guarantee_check`]) of an output built over
//! the candidates.
//!
//! For query `q` and a set `S` of approximate candidates (one candidate, or
//! a pair), the outputs `q` can have with exactly `S` approximate are built
//! from the candidates' operator nodes: a candidate in `S` is its own node;
//! a candidate with no member of `S` below it stands in as its node with an
//! exact guarantee; any other candidate is its node rebuilt over each
//! combination of its producers' outputs, with its guarantee composed again
//! ([`composed_guarantee`]), as the realizer recomposes a selected plan.
//! Outputs over which readers compose the same guarantee are kept once
//! ([`reader_key`]). Each output is then checked as
//! the selected plan is: [`guarantee_check`] with the deployment's model.
//! `S` is usable for `q` only when every output passes: a candidate that
//! fails through one reader is unusable through every reader (the largest
//! weight counts, as before). The candidates of one operator node (other
//! timings or materializations) build the same node, so they are checked
//! once, together.
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use asap_accuracy::propagation::{composed_guarantee, root_guarantee, with_guarantees};
use asap_types::ir::properties::AccuracyGuarantee;
use asap_types::ir::{OperatorNode, OperatorResultKind, QueryRoot};
use asap_types::stages::{output_references, PhysicalASAPMemoCandidates, PhysicalCandidateId};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::{AccuracyWeight, CandidateData, UnusableReason};
use crate::checks::guarantee_check;
use crate::report::RejectionCause;
use crate::SelectionModels;

/// The most outputs one candidate (or query) is built as for one set of
/// approximate candidates; beyond it the set is unusable
/// ([`UnusableReason::NotChecked`]).
pub(crate) const MAX_COMPOSITIONS: usize = 64;

/// What the accuracy rows (#1095 §2.0 row 9) are built from.
#[derive(Debug, Clone, Default)]
pub(crate) struct AccuracyPositions {
    /// Per query: `w(q, k)` for each approximate candidate `k` whose values
    /// reach `q`'s output.
    pub(crate) weights: Vec<BTreeMap<PhysicalCandidateId, AccuracyWeight>>,
    /// Per query: the pairs `(k1, k2)`, `k1 < k2`, each usable alone, whose
    /// outputs miss `q`'s target when both are approximate:
    /// `a[q, k1] + a[q, k2] ≤ 1`.
    pub(crate) excluded_pairs: Vec<Vec<(PhysicalCandidateId, PhysicalCandidateId)>>,
}

/// The outcome of one check of a query with a set of approximate candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Every output is exact: no value of the set reaches the output.
    Exact,
    /// Every output meets the target.
    Meets,
    Fails(UnusableReason),
}

/// [`AccuracyPositions`] of `physical`'s queries over the candidate links
/// left (`producer_candidates_per_input`, `output_candidates`).
pub(crate) fn accuracy_positions(
    physical: &PhysicalASAPMemoCandidates,
    candidate_data: &[CandidateData],
    producer_candidates_per_input: &[Vec<Vec<PhysicalCandidateId>>],
    output_candidates: &[Vec<PhysicalCandidateId>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &SelectionModels<'_>,
) -> AccuracyPositions {
    let below = approximate_below(candidate_data, producer_candidates_per_input);
    let output_refs = output_references(&physical.query_roots);
    let exact_stand_ins: Vec<OnceCell<Rc<OperatorNode>>> = (0..physical.candidates.len())
        .map(|_| OnceCell::new())
        .collect();
    let mut positions = AccuracyPositions::default();
    for (query, root) in physical.query_roots.iter().enumerate() {
        let references: Vec<usize> = (0..output_refs.len())
            .filter(|&reference| output_refs[reference].0 == query)
            .collect();
        let reachable: BTreeSet<usize> = references
            .iter()
            .flat_map(|&reference| &output_candidates[reference])
            .flat_map(|class| below[class.0].iter().copied())
            .collect();
        let Some(entry) = entries.get(query) else {
            positions.weights.push(BTreeMap::new());
            positions.excluded_pairs.push(Vec::new());
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
        // Candidates of one operator node (other timings or
        // materializations) build the same node: checked once, together.
        let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for &candidate in &reachable {
            groups
                .entry(physical.candidates[candidate].operator_node.0)
                .or_default()
                .push(candidate);
        }
        let mut weights = BTreeMap::new();
        let mut usable: Vec<&[usize]> = Vec::new();
        for group in groups.values() {
            let weight = match checker.check(group) {
                Outcome::Exact => continue,
                Outcome::Meets => {
                    usable.push(group);
                    AccuracyWeight::One
                }
                Outcome::Fails(reason) => AccuracyWeight::Unusable(reason),
            };
            for &candidate in group {
                weights.insert(PhysicalCandidateId(candidate), weight);
            }
        }
        let usable_candidates: Vec<usize> = usable
            .iter()
            .flat_map(|group| group.iter().copied())
            .collect();
        let group_of: HashMap<usize, usize> = usable
            .iter()
            .enumerate()
            .flat_map(|(index, group)| group.iter().map(move |&candidate| (candidate, index)))
            .collect();
        let group_pairs: BTreeSet<(usize, usize)> = pairs_that_meet(
            &usable_candidates,
            &references,
            physical,
            producer_candidates_per_input,
            output_candidates,
            &below,
        )
        .into_iter()
        .map(|(first, second)| (group_of[&first], group_of[&second]))
        .filter(|(first, second)| first != second)
        .map(|(first, second)| (first.min(second), first.max(second)))
        // A pair with a member sized for fewer than two shares is never
        // read together: the shares row already excludes it.
        .filter(|&(first, second)| {
            [first, second]
                .iter()
                .all(|&group| candidate_data[usable[group][0]].accuracy_budget_parts >= 2)
        })
        .collect();
        let mut excluded = Vec::new();
        for (first, second) in group_pairs {
            let both: Vec<usize> = usable[first]
                .iter()
                .chain(usable[second])
                .copied()
                .collect();
            if let Outcome::Fails(_) = checker.check(&both) {
                for &one in usable[first] {
                    for &other in usable[second] {
                        excluded.push((
                            PhysicalCandidateId(one.min(other)),
                            PhysicalCandidateId(one.max(other)),
                        ));
                    }
                }
            }
        }
        excluded.sort_unstable();
        positions.weights.push(weights);
        positions.excluded_pairs.push(excluded);
    }
    positions
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

/// The pairs of `usable` (ascending, `first < second`) one plan of the
/// query can read together where their values meet: under two different
/// inputs of one candidate, under two output references of a scalar root,
/// or one below the other.
fn pairs_that_meet(
    usable: &[usize],
    references: &[usize],
    physical: &PhysicalASAPMemoCandidates,
    producer_candidates_per_input: &[Vec<Vec<PhysicalCandidateId>>],
    output_candidates: &[Vec<PhysicalCandidateId>],
    below: &[BTreeSet<usize>],
) -> BTreeSet<(usize, usize)> {
    let usable: BTreeSet<usize> = usable.iter().copied().collect();
    let mut pairs = BTreeSet::new();
    let mut add_across = |sides: &[BTreeSet<usize>]| {
        for (index, left) in sides.iter().enumerate() {
            for right in &sides[index + 1..] {
                for &a in left {
                    for &b in right {
                        if a != b {
                            pairs.insert((a.min(b), a.max(b)));
                        }
                    }
                }
            }
        }
    };
    let side = |producers: &[PhysicalCandidateId]| -> BTreeSet<usize> {
        producers
            .iter()
            .flat_map(|producer| below[producer.0].iter().copied())
            .filter(|candidate| usable.contains(candidate))
            .collect()
    };
    for producers_per_input in producer_candidates_per_input
        .iter()
        .take(physical.candidates.len())
    {
        if producers_per_input.len() < 2 {
            continue;
        }
        let sides: Vec<BTreeSet<usize>> = producers_per_input
            .iter()
            .map(|producers| side(producers))
            .collect();
        add_across(&sides);
    }
    let sides: Vec<BTreeSet<usize>> = references
        .iter()
        .map(|&reference| side(&output_candidates[reference]))
        .collect();
    add_across(&sides);
    for &upper in &usable {
        for &lower in &below[upper] {
            if lower != upper && usable.contains(&lower) {
                pairs.insert((lower.min(upper), lower.max(upper)));
            }
        }
    }
    pairs
}

/// One query's checks.
struct Checker<'a> {
    physical: &'a PhysicalASAPMemoCandidates,
    /// Per candidate, its node with an exact guarantee, built once.
    exact_stand_ins: &'a [OnceCell<Rc<OperatorNode>>],
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

/// The outputs one candidate is built as, or why they are not checked.
type Built = Result<Vec<Rc<OperatorNode>>, UnusableReason>;

impl Checker<'_> {
    /// Checks the query's outputs with exactly `approximate` approximate.
    fn check(&self, approximate: &[usize]) -> Outcome {
        let mut memo: HashMap<usize, Built> = HashMap::new();
        let mut keys = Keys::new();
        let mut per_reference: Vec<Vec<Rc<OperatorNode>>> = Vec::new();
        for &reference in self.references {
            let mut outputs: Vec<Rc<OperatorNode>> = Vec::new();
            for &class in &self.output_candidates[reference] {
                match self.built(class.0, approximate, &mut memo, &mut keys, &mut Vec::new()) {
                    Ok(built) => outputs.extend(built),
                    Err(reason) => return Outcome::Fails(reason),
                }
            }
            per_reference.push(distinct_for_readers(outputs, &mut keys));
        }
        let Some(roots) = self.roots(&per_reference) else {
            return Outcome::Fails(UnusableReason::NotChecked);
        };
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

    /// The query roots over each combination of `per_reference`'s outputs;
    /// `None` beyond [`MAX_COMPOSITIONS`].
    fn roots(&self, per_reference: &[Vec<Rc<OperatorNode>>]) -> Option<Vec<QueryRoot>> {
        let combinations = combinations(per_reference)?;
        Some(
            combinations
                .into_iter()
                .map(|chosen| match self.root {
                    QueryRoot::Operator(_) => QueryRoot::Operator(chosen[0].clone()),
                    QueryRoot::Scalar(expr) => {
                        let mut next = chosen.into_iter();
                        QueryRoot::Scalar(expr.map_operator_refs(&mut |_| {
                            next.next().expect("one output per reference")
                        }))
                    }
                })
                .collect(),
        )
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

    /// The outputs `class` is built as with `approximate` approximate
    /// (module documentation). `visiting` holds the candidates being built,
    /// so a cycle of links builds nothing.
    fn built(
        &self,
        class: usize,
        approximate: &[usize],
        memo: &mut HashMap<usize, Built>,
        keys: &mut Keys,
        visiting: &mut Vec<usize>,
    ) -> Built {
        if let Some(done) = memo.get(&class) {
            return done.clone();
        }
        if visiting.contains(&class) {
            return Ok(Vec::new());
        }
        let node = self.physical.physical_node(PhysicalCandidateId(class));
        let reaches = |candidate: usize| {
            approximate
                .iter()
                .any(|member| self.below[candidate].contains(member))
        };
        let producers = &self.producer_candidates_per_input[class];
        let below_inputs = producers
            .iter()
            .flatten()
            .any(|producer| reaches(producer.0));
        let built = if !reaches(class) {
            Ok(vec![self.exact_stand_in(class)])
        } else if !below_inputs {
            // `class` is in `approximate`, over exact inputs: its node with
            // every guarantee below it composed, as the realizer does.
            with_guarantees(node, &mut HashMap::new())
                .map(|node| vec![node])
                .map_err(|_| UnusableReason::NotChecked)
        } else {
            visiting.push(class);
            let mut per_input = Vec::with_capacity(producers.len());
            let mut failed = None;
            for input in producers {
                let mut outputs = Vec::new();
                for producer in input.iter().filter(|producer| reaches(producer.0)) {
                    match self.built(producer.0, approximate, memo, keys, visiting) {
                        Ok(built) => outputs.extend(built),
                        Err(reason) => failed = Some(reason),
                    }
                }
                // One exact stand-in for the producers with no member below:
                // one with no approximate candidate below, when there is one.
                let exact = input
                    .iter()
                    .filter(|producer| !reaches(producer.0))
                    .min_by_key(|producer| !self.below[producer.0].is_empty());
                if let Some(exact) = exact {
                    outputs.push(self.exact_stand_in(exact.0));
                }
                per_input.push(distinct_for_readers(outputs, keys));
            }
            visiting.pop();
            match failed {
                Some(reason) => Err(reason),
                None => rebuilt(node, &per_input, keys),
            }
        };
        memo.insert(class, built.clone());
        built
    }
}

/// `node` rebuilt over each combination of `per_input`'s outputs, with its
/// timing and materialization and its guarantee composed again; outputs the
/// same for their readers kept once.
fn rebuilt(node: &Rc<OperatorNode>, per_input: &[Vec<Rc<OperatorNode>>], keys: &mut Keys) -> Built {
    let combinations = combinations(per_input).ok_or(UnusableReason::NotChecked)?;
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

/// Every choice of one entry per list, in order; `None` beyond
/// [`MAX_COMPOSITIONS`]. One empty list gives none.
fn combinations<T: Clone>(lists: &[Vec<T>]) -> Option<Vec<Vec<T>>> {
    let mut combinations: Vec<Vec<T>> = vec![Vec::new()];
    for list in lists {
        if combinations.len().saturating_mul(list.len()) > MAX_COMPOSITIONS {
            return None;
        }
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
    Some(combinations)
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
