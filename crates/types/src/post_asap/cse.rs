//! Structural sharing for a selected workload in one execution/data scope.
//!
//! This is not candidate selection or a cross-request cache. Callers opt into
//! common producer execution only after agreeing on lifecycle and data scope.
//! Typed equality includes schemas, guarantees and complete source expressions.

use std::collections::HashMap;
use std::rc::Rc;

use super::{SummaryExpr, SummaryNode};

/// Intern equal selected subtrees across roots while preserving every root ID.
///
/// Only structural equality is used: no grouping, parameter, accuracy or source
/// coercions are performed. All roots must belong to the same data snapshot or
/// maintenance scope. Downstream realization must still check physical
/// implementation compatibility. Use separate calls for independent executions.
pub fn share_common_summary_subtrees<Id>(
    roots: Vec<(Id, Rc<SummaryNode>)>,
) -> Vec<(Id, Rc<SummaryNode>)> {
    fn visit(
        node: &Rc<SummaryNode>,
        seen: &mut HashMap<usize, Rc<SummaryNode>>,
        pool: &mut Vec<Rc<SummaryNode>>,
    ) -> Rc<SummaryNode> {
        let identity = Rc::as_ptr(node) as usize;
        if let Some(node) = seen.get(&identity) {
            return Rc::clone(node);
        }
        let mut result = node.as_ref().clone();
        match &mut result.expr {
            SummaryExpr::KeepPreAsap(_) => {}
            SummaryExpr::SummaryAgg { child, .. } => *child = visit(child, seen, pool),
            SummaryExpr::BinaryOp { lhs, rhs, .. } => {
                *lhs = visit(lhs, seen, pool);
                *rhs = visit(rhs, seen, pool);
            }
            SummaryExpr::SummaryJoin { outer, inner, .. } => {
                *outer = visit(outer, seen, pool);
                *inner = visit(inner, seen, pool);
            }
            SummaryExpr::SummarySubtract { left, right } => {
                *left = visit(left, seen, pool);
                *right = visit(right, seen, pool);
            }
            SummaryExpr::SummaryEstimate { summary_input, .. }
            | SummaryExpr::SummaryDelete { summary_input, .. } => {
                *summary_input = visit(summary_input, seen, pool);
            }
            SummaryExpr::SummaryMerge { children } => {
                for child in children {
                    *child = visit(child, seen, pool);
                }
            }
        }
        let result = match pool.iter().find(|existing| existing.as_ref() == &result) {
            Some(existing) => Rc::clone(existing),
            None => {
                let result = Rc::new(result);
                pool.push(Rc::clone(&result));
                result
            }
        };
        seen.insert(identity, Rc::clone(&result));
        result
    }
    let mut seen = HashMap::new();
    let mut pool = Vec::new();
    roots
        .into_iter()
        .map(|(id, root)| (id, visit(&root, &mut seen, &mut pool)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_asap::{ResultGuarantee, SummarySchema};
    use crate::pre_asap::{QueryExpr, ScalarValue};

    fn leaf(value: f64) -> Rc<SummaryNode> {
        Rc::new(SummaryNode {
            expr: SummaryExpr::KeepPreAsap(Rc::new(QueryExpr::Literal(ScalarValue::Float64(
                value,
            )))),
            schema: SummarySchema {
                fields: vec![],
                time_index: None,
            },
            guarantee: Some(ResultGuarantee::exact("fixture")),
        })
    }

    // Equal separately constructed roots preserve both IDs but share identity.
    #[test]
    fn shares_equal_roots_and_preserves_ids() {
        let roots = share_common_summary_subtrees(vec![("a", leaf(1.0)), ("b", leaf(1.0))]);
        assert_eq!(roots[0].0, "a");
        assert_eq!(roots[1].0, "b");
        assert!(Rc::ptr_eq(&roots[0].1, &roots[1].1));
    }

    // A diamond is retained across the returned roots, not copied per consumer.
    #[test]
    fn shares_children_across_distinct_roots() {
        let merge = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryMerge {
                children: vec![leaf(1.0), leaf(2.0)],
            },
            schema: SummarySchema {
                fields: vec![],
                time_index: None,
            },
            guarantee: None,
        });
        let roots = share_common_summary_subtrees(vec![(0, leaf(1.0)), (1, merge)]);
        let SummaryExpr::SummaryMerge { children } = &roots[1].1.expr else {
            panic!()
        };
        assert!(Rc::ptr_eq(&roots[0].1, &children[0]));
        assert!(!Rc::ptr_eq(&children[0], &children[1]));
    }

    // Unknown guarantees must not be replaced by an equal expression's exact guarantee.
    #[test]
    fn distinct_guarantees_and_values_are_not_shared() {
        let mut unknown = leaf(1.0).as_ref().clone();
        unknown.guarantee = None;
        let roots = share_common_summary_subtrees(vec![
            (0, leaf(1.0)),
            (1, Rc::new(unknown)),
            (2, leaf(2.0)),
        ]);
        assert!(!Rc::ptr_eq(&roots[0].1, &roots[1].1));
        assert!(!Rc::ptr_eq(&roots[0].1, &roots[2].1));
        assert!(roots[1].1.guarantee.is_none());
    }
}
