//! Tumbling-pane window summaries: what an [`ASAPOp::PaneChain`] computes,
//! as the plain `SummaryAgg` nodes it stands for.
//!
//! A chain of `n` panes of width `w` over `child` is pane `i` (newest first,
//! `0 ≤ i < n`): the chain's `SummaryAgg` over a range `TimeRange(w)` over
//! `child` shifted back by `i·w`. A `SummaryMerge` over the chain is the
//! state of one build over `TimeRange(n·w)` ([`whole_window_build`]) when
//! the family merges (the family-capability table), and the executor runs
//! the chain as its panes ([`expand_pane_chains`]). A merge of only the
//! newest `m < n` panes (`SummaryMerge::newest_panes`) is the state of one
//! build over `TimeRange(m·w)` ([`merged_window_build`]): one chain kept for
//! the longest window serves every query whose window, on the same panes,
//! ends at the same time and is shorter.
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use super::asap::ASAPOp;
use super::node::{Operator, OperatorNode};
use super::non_asap::{NonASAPOp, TimeRangeKind};
use super::operator_properties::TimeShift;
use super::query::QueryRoot;
use crate::ir::properties::ExecutionTiming;
use crate::ir::SchemaDerivationError;

fn invalid(message: &str) -> SchemaDerivationError {
    SchemaDerivationError::InvalidScalarSignature(message.into())
}

/// The `PaneChain` a summary-state node carries merged: the chain itself,
/// or the one chain a `SummaryMerge` reads, in full or its newest panes
/// ([`merged_pane_count`]). `None` for any other node.
pub fn merged_chain(state: &OperatorNode) -> Option<&OperatorNode> {
    match state.asap()? {
        ASAPOp::PaneChain { .. } => Some(state),
        ASAPOp::SummaryMerge { children, .. } => match children.as_slice() {
            [child] if matches!(child.asap(), Some(ASAPOp::PaneChain { .. })) => Some(child),
            _ => None,
        },
        _ => None,
    }
}

/// How many of its chain's panes, newest first, a summary-state node with
/// a [`merged_chain`] merges: the merge's `newest_panes`, otherwise the
/// chain's `pane_count`. `None` for any other node.
fn merged_pane_count(state: &OperatorNode) -> Option<u32> {
    let Some(ASAPOp::PaneChain { pane_count, .. }) = merged_chain(state)?.asap() else {
        return None;
    };
    match state.asap()? {
        ASAPOp::SummaryMerge {
            newest_panes: Some(newest),
            ..
        } => Some(*newest),
        _ => Some(*pane_count),
    }
}

/// The `SummaryAgg` over `TimeRange(pane_count · pane_width_ms)` over the
/// chain's input whose state the merged panes of `chain` equal.
pub fn whole_window_build(chain: &OperatorNode) -> Result<OperatorNode, SchemaDerivationError> {
    let Some(ASAPOp::PaneChain { pane_count, .. }) = chain.asap() else {
        return Err(invalid("expected a pane chain"));
    };
    newest_panes_build(chain, *pane_count)
}

/// The `SummaryAgg` whose state a summary-state node with a
/// [`merged_chain`] carries: one build over the window of the panes it
/// merges ([`merged_pane_count`]).
pub fn merged_window_build(state: &OperatorNode) -> Result<OperatorNode, SchemaDerivationError> {
    let (Some(chain), Some(pane_count)) = (merged_chain(state), merged_pane_count(state)) else {
        return Err(invalid("expected merged panes of one pane chain"));
    };
    newest_panes_build(chain, pane_count)
}

/// The `SummaryAgg` over `TimeRange(pane_count · pane_width_ms)` over the
/// chain's input: the state of the chain's newest `pane_count` panes.
fn newest_panes_build(
    chain: &OperatorNode,
    pane_count: u32,
) -> Result<OperatorNode, SchemaDerivationError> {
    let Some(ASAPOp::PaneChain {
        child,
        pane_width_ms,
        ..
    }) = chain.asap()
    else {
        return Err(invalid("expected a pane chain"));
    };
    let window = pane_width_ms
        .checked_mul(u64::from(pane_count))
        .ok_or_else(|| invalid("pane chain window overflows"))?;
    build_over(chain, range(Rc::clone(child), window)?)
}

/// Pane `index` (0 = newest) of `chain`: its `SummaryAgg` over the range
/// `TimeRange(pane_width_ms)` ending `index · pane_width_ms` before the
/// chain's input.
pub fn pane(chain: &OperatorNode, index: u32) -> Result<OperatorNode, SchemaDerivationError> {
    let Some(ASAPOp::PaneChain {
        child,
        pane_width_ms,
        ..
    }) = chain.asap()
    else {
        return Err(invalid("expected a pane chain"));
    };
    let back = i64::try_from(pane_width_ms.saturating_mul(u64::from(index)))
        .map_err(|_| invalid("pane offset overflows"))?;
    build_over(chain, range(shifted(child, back)?, *pane_width_ms)?)
}

/// `child` evaluated `back_ms` earlier: one `TimeShift`, folded into a
/// `TimeShift` directly below.
fn shifted(
    child: &Rc<OperatorNode>,
    back_ms: i64,
) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    if back_ms == 0 {
        return Ok(Rc::clone(child));
    }
    let (shift, input) = match child.non_asap() {
        Some(NonASAPOp::TimeShift {
            shift,
            child: input,
        }) => (
            TimeShift {
                offset_ms: shift
                    .offset_ms
                    .checked_add(back_ms)
                    .ok_or_else(|| invalid("pane offset overflows"))?,
                at: shift.at,
            },
            input,
        ),
        _ => (
            TimeShift {
                offset_ms: back_ms,
                at: None,
            },
            child,
        ),
    };
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
        shift,
        child: Rc::clone(input),
    }))
}

fn range(
    child: Rc<OperatorNode>,
    width_ms: u64,
) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(width_ms),
        kind: TimeRangeKind::Range,
        child,
    }))
}

/// The chain's `SummaryAgg` over `input`, with the chain's column names.
fn build_over(
    chain: &OperatorNode,
    input: Rc<OperatorNode>,
) -> Result<OperatorNode, SchemaDerivationError> {
    let Some(ASAPOp::PaneChain {
        family,
        input: update,
        reduction,
        grouping,
        filter,
        ..
    }) = chain.asap()
    else {
        return Err(invalid("expected a pane chain"));
    };
    let mut build = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
        child: input,
        family: family.clone(),
        input: update.clone(),
        reduction: reduction.clone(),
        grouping: grouping.clone(),
        filter: filter.clone(),
    }))?;
    for (field, kept) in build.schema.fields.iter_mut().zip(&chain.schema.fields) {
        field.name = kept.name.clone();
        field.table = kept.table.clone();
    }
    Ok(build)
}

/// `root` with every `PaneChain` expanded into its panes: a `SummaryMerge`
/// reads each pane of each chain it read, newest first, as plain
/// `SummaryAgg` nodes the executor runs; a merge of the newest `n` panes
/// reads the first `n`, the same nodes a merge of the whole chain reads. Every other node keeps its
/// guarantee, timing and materialization. Each pane takes its chain's
/// timing and materialization, and the range and shift it reads take the
/// chain's timing: a chain materialized at ingestion time expands into
/// panes each built and stored at ingestion time. Expanding before timings
/// are applied leaves the panes untimed. A chain read by anything but a
/// `SummaryMerge` is an error.
pub fn expand_pane_chains(
    root: &Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    expand(root, &mut ExpansionMemo::default())
}

/// A workload with its pane chains expanded
/// ([`expand_pane_chains_in_workload`]).
#[derive(Debug, Clone)]
pub struct PaneChainExpansion {
    /// The workload's roots, in order, with every chain expanded.
    pub roots: Vec<QueryRoot>,
    /// Each expanded chain with its panes some merge reads, newest first,
    /// in the order the chains were first reached.
    pub chains: Vec<ExpandedPaneChain>,
    /// Every other node of the input workload the expansion reached, with
    /// the node that replaces it (itself when nothing below it changed),
    /// so a caller can carry what it knows about a node to the expanded
    /// workload.
    pub expanded_nodes: Vec<(Rc<OperatorNode>, Rc<OperatorNode>)>,
}

/// One expanded chain: the `PaneChain` node of the input workload and the
/// `SummaryAgg` panes that replace it.
#[derive(Debug, Clone)]
pub struct ExpandedPaneChain {
    /// The chain node, as the input workload holds it.
    pub chain: Rc<OperatorNode>,
    /// Its panes in the expanded workload that some merge reads, newest
    /// first.
    pub panes: Vec<Rc<OperatorNode>>,
}

/// [`expand_pane_chains`] over every root of one workload, with one memo, so
/// a node several roots share stays one node and a chain several merges
/// read expands into one set of panes. A standalone scalar root has the
/// operators it reads expanded. Also returns each chain's panes, so a
/// caller can carry what it knows about a chain (e.g. its retention) to
/// them.
pub fn expand_pane_chains_in_workload(
    roots: &[QueryRoot],
) -> Result<PaneChainExpansion, SchemaDerivationError> {
    let mut memo = ExpansionMemo::default();
    let roots = roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => expand(node, &mut memo).map(QueryRoot::Operator),
            QueryRoot::Scalar(expr) => {
                let mut error = None;
                let expr = expr.map_operator_refs(&mut |node| {
                    expand(node, &mut memo).unwrap_or_else(|e| {
                        error.get_or_insert(e);
                        Rc::clone(node)
                    })
                });
                match error {
                    Some(e) => Err(e),
                    None => Ok(QueryRoot::Scalar(expr)),
                }
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let chains = memo
        .chains
        .into_iter()
        .map(|chain| {
            let pane_count = match chain.asap() {
                Some(ASAPOp::PaneChain { pane_count, .. }) => *pane_count,
                _ => unreachable!("only chains are recorded"),
            };
            let panes = (0..pane_count)
                .map_while(|index| memo.panes.get(&(Rc::as_ptr(&chain), index)).cloned())
                .collect();
            ExpandedPaneChain { chain, panes }
        })
        .collect();
    Ok(PaneChainExpansion {
        roots,
        chains,
        expanded_nodes: memo.expanded,
    })
}

/// The untimed `node` with `timing` written into it and into every untimed
/// node below it: the nodes [`pane`] created, over the chain's input, which
/// the timing pass already timed.
fn timed_like(node: &Rc<OperatorNode>, timing: ExecutionTiming) -> OperatorNode {
    OperatorNode::with_schema(
        node.operator.map_children(|child| match child.timing {
            Some(_) => Rc::clone(child),
            None => Rc::new(timed_like(child, timing)),
        }),
        node.schema.clone(),
    )
    .with_guarantee(node.guarantee.clone())
    .with_materialization(node.materialization)
    .with_timing(Some(timing))
}

/// What one expansion has built: each expanded node by its input's
/// address, and each pane by its chain's address and index, so a chain two
/// merges read expands into one set of panes.
#[derive(Default)]
struct ExpansionMemo {
    nodes: HashMap<*const OperatorNode, Rc<OperatorNode>>,
    panes: HashMap<(*const OperatorNode, u32), Rc<OperatorNode>>,
    /// Each chain expanded, in the order first reached.
    chains: Vec<Rc<OperatorNode>>,
    /// Each expanded node with its input, in the order expanded.
    expanded: Vec<(Rc<OperatorNode>, Rc<OperatorNode>)>,
}

fn expand(
    node: &Rc<OperatorNode>,
    memo: &mut ExpansionMemo,
) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    if let Some(done) = memo.nodes.get(&Rc::as_ptr(node)) {
        return Ok(Rc::clone(done));
    }
    let out = match &node.operator {
        Operator::ASAP(ASAPOp::PaneChain { .. }) => {
            return Err(invalid("a pane chain is read only through a summary merge"))
        }
        Operator::ASAP(ASAPOp::SummaryMerge {
            children,
            newest_panes,
        }) => {
            let mut inputs = Vec::new();
            for child in children {
                match child.asap() {
                    Some(ASAPOp::PaneChain { pane_count, .. }) => {
                        for index in 0..newest_panes.unwrap_or(*pane_count) {
                            let key = (Rc::as_ptr(child), index);
                            if let Some(done) = memo.panes.get(&key) {
                                inputs.push(Rc::clone(done));
                                continue;
                            }
                            let pane = Rc::new(pane(child, index)?);
                            let pane = match child.timing {
                                Some(timing) => Rc::new(
                                    timed_like(&pane, timing)
                                        .with_materialization(child.materialization),
                                ),
                                None => pane,
                            };
                            if index == 0 {
                                memo.chains.push(Rc::clone(child));
                            }
                            memo.panes.insert(key, Rc::clone(&pane));
                            inputs.push(pane);
                        }
                    }
                    _ => inputs.push(expand(child, memo)?),
                }
            }
            let merge = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
                children: inputs,
                newest_panes: None,
            }))?;
            Rc::new(
                OperatorNode::with_schema(merge.operator, node.schema.clone())
                    .with_guarantee(node.guarantee.clone())
                    .with_timing(node.timing)
                    .with_materialization(node.materialization),
            )
        }
        _ => {
            let mut changed = false;
            let mut children = HashMap::new();
            for child in node.children() {
                let new = expand(child, memo)?;
                changed |= !Rc::ptr_eq(&new, child);
                children.insert(Rc::as_ptr(child), new);
            }
            if changed {
                Rc::new(
                    OperatorNode::with_schema(
                        node.operator
                            .map_children(|child| Rc::clone(&children[&Rc::as_ptr(child)])),
                        node.schema.clone(),
                    )
                    .with_guarantee(node.guarantee.clone())
                    .with_timing(node.timing)
                    .with_materialization(node.materialization),
                )
            } else {
                Rc::clone(node)
            }
        }
    };
    memo.nodes.insert(Rc::as_ptr(node), Rc::clone(&out));
    memo.expanded.push((Rc::clone(node), Rc::clone(&out)));
    Ok(out)
}
