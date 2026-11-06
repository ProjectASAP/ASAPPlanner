//! Tumbling-window summaries: what an [`ASAPOp::SummaryTumblingWindowChain`] computes,
//! as the plain `SummaryAgg` nodes it stands for.
//!
//! A chain of `n` tumbling windows of width `w` over `child` is tumbling window `i` (newest first,
//! `0 ≤ i < n`): the chain's `SummaryAgg` over a range `TimeRange(w)` over
//! `child` shifted back by `i·w`. A `SummaryMerge` over the chain is the
//! state of one build over `TimeRange(n·w)` (`whole_window_build`) when
//! the family merges (the family-capability table), and the executor runs
//! the chain as its tumbling windows (`expand_tumbling_window_chains`). A merge of only the
//! newest `m < n` tumbling windows (`SummaryMerge::newest_tumbling_windows`) is the state of one
//! build over `TimeRange(m·w)` (`merged_window_build`): one chain materialized for
//! the longest window serves every query whose window, on the same tumbling windows,
//! ends at the same time and is shorter.
#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use super::asap::{summary_agg_schema, ASAPOp};
use super::input::node_of_build;
use super::input::{InputOrTemporaryBuild, OperatorInput};
use super::node::{Operator, OperatorNode};
use super::non_asap::{NonASAPOp, TimeRangeKind};
#[cfg(any(test, feature = "test-support"))]
use super::operator_properties::TimeShift;
use crate::ir::schema::Schema;
use crate::ir::SchemaDerivationError;

fn invalid(message: &str) -> SchemaDerivationError {
    SchemaDerivationError::InvalidScalarSignature(message.into())
}

/// The `SummaryTumblingWindowChain` a summary state carries merged: the chain itself,
/// or the one chain a `SummaryMerge` that keeps its groups reads, in full
/// or its newest tumbling windows (`merged_tumbling_window_count`). `None` for any other input,
/// a rollup (regrouping merge) included.
pub fn merged_chain<I: OperatorInput>(state: &I) -> Option<I> {
    match state.operator() {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => Some(state.clone()),
        Operator::ASAP(ASAPOp::SummaryMerge {
            children,
            group_by: None,
            ..
        }) => match children.as_slice() {
            [child]
                if matches!(
                    child.operator(),
                    Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
                ) =>
            {
                Some(child.clone())
            }
            _ => None,
        },
        _ => None,
    }
}

/// How many of its chain's tumbling windows, newest first, a summary state with
/// a [`merged_chain`] merges: the merge's `newest_tumbling_windows`, otherwise the
/// chain's `tumbling_window_count`. `None` for any other input.
fn merged_tumbling_window_count<I: OperatorInput>(state: &I) -> Option<u32> {
    let Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        tumbling_window_count,
        ..
    }) = merged_chain(state)?.operator()
    else {
        return None;
    };
    match state.operator() {
        Operator::ASAP(ASAPOp::SummaryMerge {
            newest_tumbling_windows: Some(newest),
            ..
        }) => Some(newest),
        _ => Some(tumbling_window_count),
    }
}

/// A build over the inputs of a chain over `I`, with its schema.
pub(crate) type BuildWithSchema<I> = (ASAPOp<InputOrTemporaryBuild<I>>, Schema);

/// The `SummaryAgg` over `TimeRange(tumbling_window_count · tumbling_window_width_ms)` over the
/// chain's input whose state the merged tumbling windows of `chain` equal.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn whole_window_build<I: OperatorInput>(
    chain: &I,
) -> Result<BuildWithSchema<I>, SchemaDerivationError> {
    let Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        tumbling_window_count,
        ..
    }) = chain.operator()
    else {
        return Err(invalid("expected a tumbling-window chain"));
    };
    newest_tumbling_windows_build(chain, tumbling_window_count)
}

/// The `SummaryAgg` whose state a summary state with a
/// [`merged_chain`] carries: one build over the window of the tumbling windows it
/// merges (`merged_tumbling_window_count`).
pub(crate) fn merged_window_build<I: OperatorInput>(
    state: &I,
) -> Result<BuildWithSchema<I>, SchemaDerivationError> {
    let (Some(chain), Some(tumbling_window_count)) =
        (merged_chain(state), merged_tumbling_window_count(state))
    else {
        return Err(invalid(
            "expected merged tumbling windows of one tumbling-window chain",
        ));
    };
    newest_tumbling_windows_build(&chain, tumbling_window_count)
}

/// The `SummaryAgg` over `TimeRange(tumbling_window_count · tumbling_window_width_ms)` over the
/// chain's input: the state of the chain's newest `tumbling_window_count` tumbling windows.
fn newest_tumbling_windows_build<I: OperatorInput>(
    chain: &I,
    tumbling_window_count: u32,
) -> Result<BuildWithSchema<I>, SchemaDerivationError> {
    let Operator::ASAP(chain_op) = chain.operator() else {
        return Err(invalid("expected a tumbling-window chain"));
    };
    named_as_chain(
        window_build(&chain_op, tumbling_window_count)?,
        chain.schema(),
    )
}

/// [`newest_tumbling_windows_build`] of the chain operator `chain`, without
/// its schema.
pub(crate) fn window_build<I: OperatorInput>(
    chain: &ASAPOp<I>,
    tumbling_window_count: u32,
) -> Result<ASAPOp<InputOrTemporaryBuild<I>>, SchemaDerivationError> {
    let ASAPOp::SummaryTumblingWindowChain {
        child,
        tumbling_window_width_ms,
        ..
    } = chain
    else {
        return Err(invalid("expected a tumbling-window chain"));
    };
    let window = tumbling_window_width_ms
        .checked_mul(u64::from(tumbling_window_count))
        .ok_or_else(|| invalid("tumbling-window chain window overflows"))?;
    build_over(
        chain,
        range(InputOrTemporaryBuild::Input(child.clone()), window)?,
    )
}

#[cfg(any(test, feature = "test-support"))]
/// Tumbling window `index` (0 = newest) of `chain`: its `SummaryAgg` over the range
/// `TimeRange(tumbling_window_width_ms)` ending `index · tumbling_window_width_ms` before the
/// chain's input.
pub(crate) fn tumbling_window<I: OperatorInput>(
    chain: &I,
    index: u32,
) -> Result<BuildWithSchema<I>, SchemaDerivationError> {
    let Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child,
        tumbling_window_width_ms,
        ..
    }) = chain.operator()
    else {
        return Err(invalid("expected a tumbling-window chain"));
    };
    let back = i64::try_from(tumbling_window_width_ms.saturating_mul(u64::from(index)))
        .map_err(|_| invalid("tumbling window offset overflows"))?;
    let Operator::ASAP(chain_op) = chain.operator() else {
        return Err(invalid("expected a tumbling-window chain"));
    };
    named_as_chain(
        build_over(
            &chain_op,
            range(shifted(child, back)?, tumbling_window_width_ms)?,
        )?,
        chain.schema(),
    )
}

#[cfg(any(test, feature = "test-support"))]
/// `child` evaluated `back_ms` earlier: one `TimeShift`, folded into a
/// `TimeShift` directly below.
fn shifted<I: OperatorInput>(
    child: I,
    back_ms: i64,
) -> Result<InputOrTemporaryBuild<I>, SchemaDerivationError> {
    if back_ms == 0 {
        return Ok(InputOrTemporaryBuild::Input(child));
    }
    let (shift, input) = match child.operator() {
        Operator::NonASAP(NonASAPOp::TimeShift {
            shift,
            child: input,
        }) => (
            TimeShift {
                offset_ms: shift
                    .offset_ms
                    .checked_add(back_ms)
                    .ok_or_else(|| invalid("tumbling window offset overflows"))?,
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
    InputOrTemporaryBuild::build(NonASAPOp::TimeShift {
        shift,
        child: InputOrTemporaryBuild::Input(input),
    })
}

/// A range `TimeRange(width_ms)` over `child`.
fn range<I: OperatorInput>(
    child: InputOrTemporaryBuild<I>,
    width_ms: u64,
) -> Result<InputOrTemporaryBuild<I>, SchemaDerivationError> {
    InputOrTemporaryBuild::build(NonASAPOp::TimeRange {
        range: Duration::from_millis(width_ms),
        kind: TimeRangeKind::Range,
        child,
    })
}

/// The chain's `SummaryAgg` over `input`.
fn build_over<I: OperatorInput>(
    chain: &ASAPOp<I>,
    input: InputOrTemporaryBuild<I>,
) -> Result<ASAPOp<InputOrTemporaryBuild<I>>, SchemaDerivationError> {
    let ASAPOp::SummaryTumblingWindowChain {
        family,
        input: update,
        reduction,
        grouping,
        filter,
        ..
    } = chain.map_children(|child| InputOrTemporaryBuild::Input(child.clone()))
    else {
        return Err(invalid("expected a tumbling-window chain"));
    };
    Ok(ASAPOp::SummaryAgg {
        child: input,
        family,
        input: update,
        reduction,
        grouping,
        filter,
    })
}

/// `build` with its schema, named as the chain of schema `chain`.
fn named_as_chain<I: OperatorInput>(
    build: ASAPOp<InputOrTemporaryBuild<I>>,
    chain: &Schema,
) -> Result<BuildWithSchema<I>, SchemaDerivationError> {
    let ASAPOp::SummaryAgg {
        child,
        family,
        reduction,
        ..
    } = &build
    else {
        return Err(invalid("expected a summary build"));
    };
    let mut schema = summary_agg_schema(child.schema(), family, reduction)?;
    for (field, kept) in schema.fields.iter_mut().zip(&chain.fields) {
        field.name = kept.name.clone();
    }
    Ok((build, schema))
}

/// `root` with every `SummaryTumblingWindowChain` expanded into its tumbling windows: a `SummaryMerge`
/// reads each tumbling window of each chain it read, newest first, as plain
/// `SummaryAgg` nodes the executor runs; a merge of the newest `n` tumbling windows
/// reads the first `n`, the same nodes a merge of the whole chain reads. Every other node keeps its
/// guarantee. A chain read by anything but a `SummaryMerge` is an error.
#[cfg(any(test, feature = "test-support"))]
pub fn expand_tumbling_window_chains(
    root: &Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    expand(root, &mut NodeToExpandedNodeCache::default())
}

#[cfg(any(test, feature = "test-support"))]
/// What one expansion has built: each expanded node by its input's
/// address, and each tumbling window by its chain's address and index, so a chain two
/// merges read expands into one set of tumbling windows.
#[derive(Default)]
struct NodeToExpandedNodeCache {
    expanded_node_of: HashMap<*const OperatorNode, Rc<OperatorNode>>,
    tumbling_window_of: HashMap<(*const OperatorNode, u32), Rc<OperatorNode>>,
    /// Each chain expanded, in the order first reached.
    chains: Vec<Rc<OperatorNode>>,
    /// Each expanded node with its input, in the order expanded.
    expanded: Vec<(Rc<OperatorNode>, Rc<OperatorNode>)>,
}

#[cfg(any(test, feature = "test-support"))]
fn expand(
    node: &Rc<OperatorNode>,
    cache: &mut NodeToExpandedNodeCache,
) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    if let Some(done) = cache.expanded_node_of.get(&Rc::as_ptr(node)) {
        return Ok(Rc::clone(done));
    }
    let out = match &node.operator {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => {
            return Err(invalid(
                "a tumbling-window chain is read only through a summary merge",
            ))
        }
        Operator::ASAP(ASAPOp::SummaryMerge {
            children,
            newest_tumbling_windows,
            group_by,
        }) => {
            let mut inputs = Vec::new();
            for child in children {
                match child.asap() {
                    Some(ASAPOp::SummaryTumblingWindowChain {
                        tumbling_window_count,
                        ..
                    }) => {
                        for index in 0..newest_tumbling_windows.unwrap_or(*tumbling_window_count) {
                            let key = (Rc::as_ptr(child), index);
                            if let Some(done) = cache.tumbling_window_of.get(&key) {
                                inputs.push(Rc::clone(done));
                                continue;
                            }
                            let tumbling_window =
                                Rc::new(node_of_build(tumbling_window(child, index)?));
                            if index == 0 {
                                cache.chains.push(Rc::clone(child));
                            }
                            cache
                                .tumbling_window_of
                                .insert(key, Rc::clone(&tumbling_window));
                            inputs.push(tumbling_window);
                        }
                    }
                    _ => inputs.push(expand(child, cache)?),
                }
            }
            let merge = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
                children: inputs,
                newest_tumbling_windows: None,
                group_by: group_by.clone(),
            }))?;
            Rc::new(
                OperatorNode::with_schema(merge.operator, node.schema.clone())
                    .with_guarantee(node.guarantee.clone()),
            )
        }
        _ => {
            let mut changed = false;
            let mut children = HashMap::new();
            for child in node.children() {
                let new = expand(child, cache)?;
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
                    .with_guarantee(node.guarantee.clone()),
                )
            } else {
                Rc::clone(node)
            }
        }
    };
    cache
        .expanded_node_of
        .insert(Rc::as_ptr(node), Rc::clone(&out));
    cache.expanded.push((Rc::clone(node), Rc::clone(&out)));
    Ok(out)
}
