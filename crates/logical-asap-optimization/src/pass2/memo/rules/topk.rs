//! Exact ranking reuse with explicit exact-input groups. No global selection
//! flags are required: only exact implementations can supply these inputs.
use super::reuse::{append_producer, append_reader, eligible, replace};
use super::shared_producer::{add_shared_producer_candidates, Reader, SharedProducer};
use crate::pass2::asap_aware_cse::topk_reuse_candidates;
use crate::pass2::memo::candidates::{CandidateMemo, OriginOfCandidateForMemoGroup};
use crate::pass2::memo::check_generation_deadline;
use crate::pass2::memo::state_groups::StateGroupExpansionError;
use crate::pass2::memo::MemoGroupId;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::properties::AccuracyGuarantee;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

pub(crate) fn add_topk_candidates(
    memo: &mut CandidateMemo,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    add_shared_producer_candidates(
        memo,
        &mut TopKRankings {
            exact_groups: HashMap::new(),
        },
        deadline,
    )
}

/// Exact top-k readers of one input and partition with different `k`: the
/// covering need is the ranking with the largest `k`, `Limit(k_max)` over
/// `Sort` of an exact input ([`topk_reuse_candidates`]); each reader derives
/// `Limit(k) ▸ Project` from it.
struct TopKRankings {
    /// Each input group's exact-only group ([`exact_input_group`]).
    exact_groups: HashMap<MemoGroupId, Option<MemoGroupId>>,
}

/// One input's top-k readers' shared ranking: `Limit(k_max)` over `Sort`,
/// and the readers' groups, whose queries the producer serves.
struct SharedRanking {
    ranking: Rc<OperatorNode>,
    reader_groups: Vec<usize>,
}

impl SharedProducer for TopKRankings {
    /// The ranking of the reader's input and partition, at the largest `k`
    /// of its set.
    type Need = Rc<OperatorNode>;
    type Covering = SharedRanking;

    fn sets(
        &mut self,
        memo: &CandidateMemo,
        deadline: Option<Instant>,
    ) -> Result<Vec<Vec<Reader<Rc<OperatorNode>>>>, StateGroupExpansionError> {
        let mut owners = HashMap::new();
        let mut roots = Vec::new();
        for (group, entry) in memo.groups.iter().enumerate() {
            check_generation_deadline(deadline)?;
            let [input] = entry.logical_node.children()[..] else {
                continue;
            };
            let exact = Rc::new(
                (**input)
                    .clone()
                    .with_guarantee(Some(AccuracyGuarantee::exact(
                        "exact input candidate group",
                    ))),
            );
            let Ok(copy) = entry.logical_node.with_new_children(|_| Rc::clone(&exact)) else {
                continue;
            };
            let copy = Rc::new(copy);
            owners.insert(Rc::as_ptr(&copy), group);
            roots.push(QueryRoot::Operator(copy));
        }
        Ok(topk_reuse_candidates(&roots)
            .into_iter()
            .map(|reuse| {
                reuse
                    .members
                    .iter()
                    .filter_map(|node| {
                        Some(Reader {
                            group: *owners.get(&Rc::as_ptr(node))?,
                            need: Rc::clone(&reuse.ranking),
                        })
                    })
                    .collect()
            })
            .collect())
    }

    fn coverings(
        &self,
        set: &[Reader<Rc<OperatorNode>>],
        _deadline: Option<Instant>,
    ) -> Result<Vec<SharedRanking>, StateGroupExpansionError> {
        // The largest `k` holds every smaller top-k, so it serves the set.
        Ok(vec![SharedRanking {
            ranking: Rc::clone(&set[0].need),
            reader_groups: set.iter().map(|reader| reader.group).collect(),
        }])
    }

    fn derive(
        &mut self,
        memo: &mut CandidateMemo,
        reader: &Reader<Rc<OperatorNode>>,
        covering: &SharedRanking,
    ) -> Result<usize, StateGroupExpansionError> {
        // Interning the ranking is idempotent, so every reader of the set
        // reads the producer the first one interned.
        let Some(input_group) = memo.groups[covering.reader_groups[0]]
            .child_groups
            .first()
            .copied()
        else {
            return Ok(0);
        };
        let Some(exact_input) = exact_input_group(memo, input_group, &mut self.exact_groups)?
        else {
            return Ok(0);
        };
        let input = Rc::clone(&memo.groups[exact_input.0 as usize].logical_node);
        let queries: Vec<_> = covering
            .reader_groups
            .iter()
            .flat_map(|&group| memo.groups[group].queries.iter().copied())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let sort = covering.ranking.children()[0]
            .with_new_children(|_| Rc::clone(&input))
            .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
        let sort_group = append_producer(
            memo,
            Rc::new(sort),
            &queries,
            OriginOfCandidateForMemoGroup::TopKReuse,
            1,
        )?;
        let sort = Rc::clone(&memo.groups[sort_group.0 as usize].logical_node);
        let ranking = covering
            .ranking
            .with_new_children(|_| Rc::clone(&sort))
            .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
        let ranking_group = append_producer(
            memo,
            Rc::new(ranking),
            &queries,
            OriginOfCandidateForMemoGroup::TopKReuse,
            1,
        )?;
        let ranking = Rc::clone(&memo.groups[ranking_group.0 as usize].logical_node);
        let owner = reader.group;
        let logical = Rc::clone(&memo.groups[owner].logical_node);
        let Some(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(keys),
            measures,
            ..
        }) = logical.non_asap()
        else {
            return Ok(0);
        };
        let [AggIntent::TopK { k, .. }] = measures.as_slice() else {
            return Ok(0);
        };
        let Some(original) = memo.candidates[owner].iter().find(|candidate| eligible(candidate) && matches!(candidate.root_node.non_asap(), Some(NonASAPOp::Aggregate {measures, ..}) if matches!(measures.as_slice(), [AggIntent::TopK {..}]))).cloned() else { return Ok(0) };
        let limited = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Limit {
            n: Some(*k),
            offset: 0,
            partition_by: keys.clone(),
            child: Rc::clone(&ranking),
        }))
        .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
        let columns = asap_types::ir::schema::topk::ranked_row_projection(
            &ranking,
            &Reduction::Reduce(keys.clone()),
            &logical.schema.fields[keys.len()].name,
        )
        .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
        let reader = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols: columns,
            qualifier: None,
            child: limited,
        }))
        .map_err(|error| StateGroupExpansionError::ReuseRule(error.to_string()))?;
        Ok(usize::from(append_reader(
            memo,
            owner,
            &original,
            reader,
            OriginOfCandidateForMemoGroup::TopKReuse,
        )?))
    }
}

fn exact_input_group(
    memo: &mut CandidateMemo,
    group: MemoGroupId,
    known: &mut HashMap<MemoGroupId, Option<MemoGroupId>>,
) -> Result<Option<MemoGroupId>, StateGroupExpansionError> {
    if let Some(result) = known.get(&group) {
        return Ok(*result);
    }
    // An in-progress marker also prevents malformed cyclic inventories recursing.
    known.insert(group, None);
    let originals = memo.candidates[group.0 as usize].clone();
    let original_children: Vec<_> = originals
        .iter()
        .map(|candidate| candidate.input_groups.clone())
        .collect();
    let mut candidates = Vec::new();
    for mut candidate in originals {
        if !eligible(&candidate) {
            continue;
        }
        let boundaries: HashSet<_> = candidate
            .input_groups
            .iter()
            .map(|id| Rc::as_ptr(&memo.groups[id.0 as usize].logical_node))
            .collect();
        let mut pending = vec![Rc::clone(&candidate.root_node)];
        let mut seen = HashSet::new();
        let mut exact = true;
        while let Some(node) = pending.pop() {
            if boundaries.contains(&Rc::as_ptr(&node)) || !seen.insert(Rc::as_ptr(&node)) {
                continue;
            }
            if matches!(node.asap(), Some(ASAPOp::SummaryAgg {family, ..} | ASAPOp::PaneChain {family, ..}) if !matches!(family, FieldDataType::ExactAggregate(..)))
            {
                exact = false;
                break;
            }
            pending.extend(node.children().into_iter().cloned());
        }
        if !exact {
            continue;
        }
        let mut replacements = HashMap::new();
        let mut children = Vec::new();
        for child in &candidate.input_groups {
            let Some(exact_child) = exact_input_group(memo, *child, known)? else {
                exact = false;
                break;
            };
            replacements.insert(
                Rc::as_ptr(&memo.groups[child.0 as usize].logical_node),
                Rc::clone(&memo.groups[exact_child.0 as usize].logical_node),
            );
            children.push(exact_child);
        }
        if !exact {
            continue;
        }
        candidate.root_node = replace(&candidate.root_node, &replacements);
        candidate.input_groups = children;
        candidates.push(candidate);
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    if candidates.len() == original_children.len()
        && candidates
            .iter()
            .zip(&original_children)
            .all(|(candidate, children)| candidate.input_groups == *children)
    {
        // An already exact-only group remains the shared source for ordinary
        // consumers too; cloning it would unnecessarily split scan/state reuse.
        known.insert(group, Some(group));
        return Ok(Some(group));
    }
    let node = Rc::new((*candidates[0].root_node).clone().with_guarantee(Some(
        AccuracyGuarantee::exact("only exact implementations and exact inputs"),
    )));
    let queries = memo.groups[group.0 as usize].queries.clone();
    let id = append_producer(
        memo,
        node,
        &queries,
        OriginOfCandidateForMemoGroup::TopKReuse,
        1,
    )?;
    for candidate in &mut candidates {
        candidate.fused_groups = Vec::new();
    }
    memo.candidates[id.0 as usize] = candidates;
    known.insert(group, Some(id));
    Ok(Some(id))
}
