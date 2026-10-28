//! One scan of a source shared by reads with different predicates, columns,
//! windows and shifts (subsumption derivations, *Efficient and Extensible
//! Algorithms for Multi Query Optimization*, SIGMOD 2000, §2.1). The shared
//! `Scan` reads every column any of them projects and the rows any of them
//! keeps. Each read gets a reader of it: its own `TimeShift`s and
//! `TimeRange` rebuilt over the shared `Scan`, then its own rows (`Filter`)
//! and columns (`Project`) above the window. The original reads stay, so
//! plan selection decides.
//!
//! The window stays directly over the shifts and the `Scan`, so
//! `PhysicalASAPDAG::scan_window` sees each reader's window: the executor
//! requests, and Stage 3 keeps and prices, the union of the readers'
//! windows. A `Filter` below the window would make the scan's window
//! unbounded. Reads with an `@` anchor do not share: their window is not
//! relative to the evaluation time.
use super::reuse::eligible;
use crate::memo::candidates::{CandidateMemo, CandidateOrigin, ImplementationCandidate};
use crate::memo::check_generation_deadline;
use crate::memo::state_groups::{boundary_groups, StateGroupExpansionError};
use crate::memo::{MemoGroup, MemoGroupId};
use asap_types::ir::operator::Source;
use asap_types::ir::schema::{Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, Predicate, ProjectItem, ScalarExpr};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

pub(in crate::memo) fn add_shared_scan_candidates(
    memo: &mut CandidateMemo,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    // Windowed reads keyed by their source.
    let mut reads: Vec<(Source, Vec<usize>)> = Vec::new();
    for (group, entry) in memo.groups.iter().enumerate() {
        let Some(key) = windowed_read(&entry.node) else {
            continue;
        };
        if !memo.candidates[group]
            .iter()
            .any(|c| is_own(c, &entry.node))
        {
            continue;
        }
        match reads.iter_mut().find(|(known, _)| known == key) {
            Some((_, groups)) => groups.push(group),
            None => reads.push((key.clone(), vec![group])),
        }
    }
    let mut added = 0;
    for (_, groups) in reads {
        check_generation_deadline(deadline)?;
        if groups.len() > 1 {
            added += share(memo, &groups)?;
        }
    }
    Ok(added)
}

/// The source of a `TimeRange` over `TimeShift`s without an `@` anchor
/// over a `Scan`.
fn windowed_read(node: &OperatorNode) -> Option<&Source> {
    let Some(NonASAPOp::TimeRange { child, .. }) = node.non_asap() else {
        return None;
    };
    let mut node = child;
    loop {
        match node.non_asap()? {
            NonASAPOp::TimeShift { shift, child } if shift.at.is_none() => node = child,
            NonASAPOp::Scan { source, .. } => return Some(source),
            _ => return None,
        }
    }
}

/// A group's own Pass 1 implementation: its logical node.
fn is_own(candidate: &ImplementationCandidate, node: &Rc<OperatorNode>) -> bool {
    candidate.origin == CandidateOrigin::Pass1
        && eligible(candidate)
        && Rc::ptr_eq(&candidate.node, node)
}

/// The scan at the bottom of a windowed read.
fn scan_of(node: &Rc<OperatorNode>) -> &Rc<OperatorNode> {
    match node.non_asap() {
        Some(NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. }) => {
            scan_of(child)
        }
        _ => node,
    }
}

/// `node`'s chain rebuilt over `scan` in place of its own scan.
fn over_scan(
    node: &Rc<OperatorNode>,
    scan: &Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, StateGroupExpansionError> {
    if matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })) {
        return Ok(Rc::clone(scan));
    }
    let rebuilt = node
        .with_new_children(|child| over_scan(child, scan).expect("a chain over one scan"))
        .map_err(|error| StateGroupExpansionError::ScanLabelWidening(error.to_string()))?;
    Ok(Rc::new(rebuilt))
}

/// Adds the shared scan of the windowed reads `groups` (one source) and a
/// reader of it to each; the number of readers added.
fn share(memo: &mut CandidateMemo, groups: &[usize]) -> Result<usize, StateGroupExpansionError> {
    // The union schema; a read whose columns conflict with it keeps its
    // own scan, and the others still share.
    let mut union: Option<Schema> = None;
    let mut members = Vec::new();
    for &group in groups {
        let schema = &scan_of(&memo.groups[group].node).schema;
        match &union {
            None => union = Some(schema.clone()),
            Some(known) => match widened(known, schema) {
                Some(wider) => union = Some(wider),
                None => continue,
            },
        }
        members.push(group);
    }
    let (Some(union), [_, _, ..]) = (union, members.as_slice()) else {
        return Ok(0);
    };
    // Each member's conjuncts, over the union's columns.
    let conjuncts: Vec<Vec<ScalarExpr>> = members
        .iter()
        .map(|&group| {
            let scan = scan_of(&memo.groups[group].node);
            let Some(NonASAPOp::Scan { predicates, .. }) = scan.non_asap() else {
                unreachable!("a windowed read ends in a scan")
            };
            let mut out = Vec::new();
            for predicate in predicates {
                split_and(&predicate.0, &mut out);
            }
            out.iter()
                .map(|conjunct| {
                    conjunct.map_columns(&mut |column| column_in(&union, &scan.schema, column))
                })
                .collect()
        })
        .collect();
    // Reads without predicates are the scan-label rule's (`scan_labels.rs`).
    if conjuncts.iter().all(Vec::is_empty) {
        return Ok(0);
    }
    let common: Vec<ScalarExpr> = conjuncts[0]
        .iter()
        .filter(|conjunct| conjuncts.iter().all(|other| other.contains(conjunct)))
        .cloned()
        .collect();
    let residuals: Vec<Vec<ScalarExpr>> = conjuncts
        .iter()
        .map(|own| {
            own.iter()
                .filter(|conjunct| !common.contains(conjunct))
                .cloned()
                .collect()
        })
        .collect();
    // The rows any member keeps: the common conjuncts, and the OR of the
    // members' other conjuncts when each compares a column with literals;
    // otherwise the common ones only.
    let mut predicates: Vec<Predicate> = common.iter().cloned().map(Predicate).collect();
    if residuals.iter().all(|own| !own.is_empty())
        && residuals.iter().flatten().all(compares_column_with_literal)
    {
        let mut disjuncts: Vec<ScalarExpr> = Vec::new();
        for own in &residuals {
            let disjunct = and(own.clone());
            if !disjuncts.contains(&disjunct) {
                disjuncts.push(disjunct);
            }
        }
        predicates.push(Predicate(match disjuncts.len() {
            1 => disjuncts.remove(0),
            _ => ScalarExpr::BoolOr(disjuncts),
        }));
    }
    let first = Rc::clone(&memo.groups[members[0]].node);
    let Some(NonASAPOp::Scan { source, .. }) = scan_of(&first).non_asap() else {
        unreachable!("a windowed read ends in a scan")
    };
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: source.clone(),
        predicates,
        schema: union,
    }))
    .map_err(|error| StateGroupExpansionError::ScanLabelWidening(error.to_string()))?;
    let all_queries = queries_of(memo, members.iter().copied());
    let scan = intern(memo, &scan, &all_queries)?;
    // Each member's own shifts and window over the shared scan; members
    // with equal windows share that window's group.
    let windows = members
        .iter()
        .map(|&group| over_scan(&memo.groups[group].node, &scan))
        .collect::<Result<Vec<_>, _>>()?;
    let mut added = 0;
    for ((&group, residual), window) in members.iter().zip(residuals).zip(&windows) {
        let readers = members
            .iter()
            .zip(&windows)
            .filter(|(_, other)| **other == *window)
            .map(|(&member, _)| member);
        let queries = queries_of(memo, readers);
        let window = intern(memo, window, &queries)?;
        let window_group = memo
            .groups
            .iter()
            .position(|group| Rc::ptr_eq(&group.node, &window))
            .expect("interned");
        if group == window_group {
            continue;
        }
        let original = Rc::clone(&memo.groups[group].node);
        let mut reader = Rc::clone(&window);
        if !residual.is_empty() {
            reader = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(and(residual)),
                child: reader,
            }))
            .map_err(|error| StateGroupExpansionError::ScanLabelWidening(error.to_string()))?;
        }
        if reader.schema.fields != original.schema.fields {
            reader = projected(&reader, &original.schema)?;
        }
        if !same_output_contract(&original, &reader) {
            continue;
        }
        let Some(own) = memo.candidates[group]
            .iter()
            .find(|c| is_own(c, &original))
            .cloned()
        else {
            continue;
        };
        let boundaries = HashMap::from([(Rc::as_ptr(&window), MemoGroupId(window_group as u32))]);
        let children = boundary_groups(&reader, &boundaries);
        if memo.candidates[group].iter().any(|known| {
            known.origin == CandidateOrigin::WidenedScans
                && known.children == children
                && *known.node == *reader
        }) {
            continue;
        }
        memo.candidates[group].push(ImplementationCandidate {
            node: reader,
            children,
            origin: CandidateOrigin::WidenedScans,
            ..own
        });
        added += 1;
    }
    Ok(added)
}

/// The queries any of `groups` serves, ascending.
fn queries_of(memo: &CandidateMemo, groups: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut queries: Vec<usize> = groups
        .flat_map(|group| memo.groups[group].queries.iter().copied())
        .collect();
    queries.sort_unstable();
    queries.dedup();
    queries
}

/// The group of `node`, a chain whose every node becomes a group of its
/// own (or the existing group of an equal node), children first.
fn intern(
    memo: &mut CandidateMemo,
    node: &Rc<OperatorNode>,
    queries: &[usize],
) -> Result<Rc<OperatorNode>, StateGroupExpansionError> {
    let children = node
        .children()
        .into_iter()
        .map(|child| intern(memo, child, queries))
        .collect::<Result<Vec<_>, _>>()?;
    let node = match children.as_slice() {
        [] => Rc::clone(node),
        _ => {
            let mut next = children.iter();
            Rc::new(
                node.with_new_children(|_| Rc::clone(next.next().expect("one per child")))
                    .map_err(|error| {
                        StateGroupExpansionError::ScanLabelWidening(error.to_string())
                    })?,
            )
        }
    };
    if let Some(existing) = memo
        .groups
        .iter()
        .find(|group| *group.node == *node && group.node.schema == node.schema)
    {
        return Ok(Rc::clone(&existing.node));
    }
    let id = MemoGroupId(
        u32::try_from(memo.groups.len()).map_err(|_| StateGroupExpansionError::TooManyGroups)?,
    );
    let boundaries = memo
        .groups
        .iter()
        .enumerate()
        .map(|(id, group)| (Rc::as_ptr(&group.node), MemoGroupId(id as u32)))
        .collect();
    let inputs = boundary_groups(&node, &boundaries)
        .into_iter()
        .filter(|&input| input != id)
        .collect::<Vec<_>>();
    memo.groups.push(MemoGroup {
        node: Rc::clone(&node),
        children: inputs.clone(),
        parents: Vec::new(),
        queries: queries.to_vec(),
        roots_of: Vec::new(),
    });
    memo.candidates.push(vec![ImplementationCandidate {
        implements: vec![id],
        node: Rc::clone(&node),
        children: inputs,
        origin: CandidateOrigin::WidenedScans,
        budget_share: 1,
    }]);
    Ok(node)
}

/// `known` widened by `schema`'s columns (by name and table): a PromQL
/// label goes before the series identity, as the scan-label rule places it.
/// `None` when the two give one column different types, or one is closed
/// (known to hold every label) and the other not.
fn widened(known: &Schema, schema: &Schema) -> Option<Schema> {
    if known.closed != schema.closed {
        return None;
    }
    let mut union = known.clone();
    for field in &schema.fields {
        match union
            .fields
            .iter()
            .find(|f| f.name == field.name && f.table == field.table)
        {
            Some(existing) if existing == field => {}
            Some(_) => return None,
            None => {
                let at = union
                    .fields
                    .iter()
                    .position(|f| f.name == PROMQL_SERIES_IDENTITY)
                    .unwrap_or(union.fields.len());
                union.fields.insert(at, field.clone());
                // Unique keys and the time column name columns by position.
                for column in union
                    .unique_keys
                    .iter_mut()
                    .flatten()
                    .chain(union.time_index.as_mut())
                {
                    if *column >= at {
                        *column += 1;
                    }
                }
            }
        }
    }
    Some(union)
}

/// The position in `union` of `schema`'s column `column`.
fn column_in(union: &Schema, schema: &Schema, column: usize) -> usize {
    let field = &schema.fields[column];
    union
        .fields
        .iter()
        .position(|f| f.name == field.name && f.table == field.table)
        .expect("the union holds every member column")
}

/// `wide` with exactly `schema`'s columns, by name and table.
fn projected(
    wide: &Rc<OperatorNode>,
    schema: &Schema,
) -> Result<Rc<OperatorNode>, StateGroupExpansionError> {
    let cols = schema
        .fields
        .iter()
        .enumerate()
        .map(|(column, field)| ProjectItem {
            alias: Some(field.name.clone()),
            expr: ScalarExpr::Column(column_in(&wide.schema, schema, column)),
        })
        .collect();
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols,
        qualifier: None,
        child: Rc::clone(wide),
    }))
    .map_err(|error| StateGroupExpansionError::ScanLabelWidening(error.to_string()))
}

fn same_output_contract(expected: &OperatorNode, actual: &OperatorNode) -> bool {
    expected.result_kind == actual.result_kind
        && expected.schema.fields == actual.schema.fields
        && expected.schema.time_index == actual.schema.time_index
        && expected.schema.unique_keys == actual.schema.unique_keys
        && (!expected.schema.closed || actual.schema.closed)
}

fn split_and(expr: &ScalarExpr, out: &mut Vec<ScalarExpr>) {
    match expr {
        ScalarExpr::BoolAnd(items) => items.iter().for_each(|item| split_and(item, out)),
        other => out.push(other.clone()),
    }
}

fn and(mut conjuncts: Vec<ScalarExpr>) -> ScalarExpr {
    match conjuncts.len() {
        1 => conjuncts.remove(0),
        _ => ScalarExpr::BoolAnd(conjuncts),
    }
}

/// Whether `conjunct` compares one column with literals (`c = v`, `c < v`,
/// a label matcher, `c IN (v, …)`).
fn compares_column_with_literal(conjunct: &ScalarExpr) -> bool {
    match conjunct {
        ScalarExpr::Compare { left, right, .. } => matches!(
            (&**left, &**right),
            (ScalarExpr::Column(_), ScalarExpr::Literal(_))
                | (ScalarExpr::Literal(_), ScalarExpr::Column(_))
        ),
        ScalarExpr::InList { expr, list, .. } => {
            matches!(&**expr, ScalarExpr::Column(_))
                && list
                    .iter()
                    .all(|item| matches!(item, ScalarExpr::Literal(_)))
        }
        _ => false,
    }
}
