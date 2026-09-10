//! Structural sharing for a selected workload in one execution/data scope.
//!
//! This is not candidate selection or a cross-request cache. Callers opt into
//! common producer execution only after agreeing on lifecycle and data scope.
//! Typed equality includes schemas, guarantees and complete source expressions.

use std::collections::HashMap;
use std::rc::Rc;

use super::{SummaryExpr, SummaryNode};

/// Numeric PartialEq alone conflates signed zeros. The serialized check is
/// additional evidence, never a replacement for typed equality (JSON maps
/// nonfinite floats to null). Keep this rule local to structural sharing.
fn same_value<T: PartialEq + serde::Serialize>(left: &T, right: &T) -> bool {
    left == right
        && match (serde_json::to_string(left), serde_json::to_string(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

/// Children have already been interned. Comparing their identities avoids
/// recursively expanding a shared DAG once for every path to each descendant.
fn same_node(left: &SummaryNode, right: &SummaryNode) -> bool {
    use SummaryExpr::*;
    let expression_equal = match (&left.expr, &right.expr) {
        (KeepPreAsap(a), KeepPreAsap(b)) => Rc::ptr_eq(a, b) || same_value(a, b),
        (
            BinaryOp {
                lhs: al,
                rhs: ar,
                operator: ao,
            },
            BinaryOp {
                lhs: bl,
                rhs: br,
                operator: bo,
            },
        ) => Rc::ptr_eq(al, bl) && Rc::ptr_eq(ar, br) && ao == bo,
        (
            CandidateTopK {
                candidates: ac,
                values: av,
                k: ak,
                grouping: ag,
                completeness: ax,
            },
            CandidateTopK {
                candidates: bc,
                values: bv,
                k: bk,
                grouping: bg,
                completeness: bx,
            },
        ) => Rc::ptr_eq(ac, bc) && Rc::ptr_eq(av, bv) && ak == bk && ag == bg && same_value(ax, bx),
        (
            ValueOperation {
                child: ac,
                operation: ao,
                timing: at,
            },
            ValueOperation {
                child: bc,
                operation: bo,
                timing: bt,
            },
        ) => Rc::ptr_eq(ac, bc) && same_value(ao, bo) && at == bt,
        (
            SummaryAgg {
                child: ac,
                family: af,
                input: ai,
                reduction: ar,
                grouping: ag,
            },
            SummaryAgg {
                child: bc,
                family: bf,
                input: bi,
                reduction: br,
                grouping: bg,
            },
        ) => Rc::ptr_eq(ac, bc) && af == bf && same_value(ai, bi) && ar == br && ag == bg,
        (
            SummaryJoin {
                outer: ao,
                inner: ai,
                key: ak,
                family: af,
            },
            SummaryJoin {
                outer: bo,
                inner: bi,
                key: bk,
                family: bf,
            },
        ) => Rc::ptr_eq(ao, bo) && Rc::ptr_eq(ai, bi) && ak == bk && af == bf,
        (
            SummarySubtract {
                left: al,
                right: ar,
            },
            SummarySubtract {
                left: bl,
                right: br,
            },
        ) => Rc::ptr_eq(al, bl) && Rc::ptr_eq(ar, br),
        (
            SummaryEstimate {
                summary_input: ai,
                query: aq,
            },
            SummaryEstimate {
                summary_input: bi,
                query: bq,
            },
        ) => Rc::ptr_eq(ai, bi) && same_value(aq, bq),
        (
            SummaryDelete {
                summary_input: ai,
                key: ak,
            },
            SummaryDelete {
                summary_input: bi,
                key: bk,
            },
        ) => Rc::ptr_eq(ai, bi) && ak == bk,
        (SummaryMerge { children: a }, SummaryMerge { children: b }) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| Rc::ptr_eq(a, b))
        }
        // Keep this exhaustive on the left: new variants require a sharing rule.
        (
            KeepPreAsap(_)
            | BinaryOp { .. }
            | CandidateTopK { .. }
            | ValueOperation { .. }
            | SummaryAgg { .. }
            | SummaryJoin { .. }
            | SummarySubtract { .. }
            | SummaryEstimate { .. }
            | SummaryDelete { .. }
            | SummaryMerge { .. },
            _,
        ) => false,
    };
    expression_equal && left.schema == right.schema && same_value(&left.guarantee, &right.guarantee)
}

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
            SummaryExpr::CandidateTopK {
                candidates, values, ..
            } => {
                *candidates = visit(candidates, seen, pool);
                *values = visit(values, seen, pool);
            }
            SummaryExpr::ValueOperation { child, .. } => *child = visit(child, seen, pool),
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
        let result = match pool.iter().find(|existing| same_node(existing, &result)) {
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

    // Sharing must preserve IEEE signed zero, including inside exact expressions.
    #[test]
    fn signed_zero_is_not_coalesced() {
        for values in [[0.0, -0.0], [-0.0, 0.0]] {
            let roots =
                share_common_summary_subtrees(vec![(0, leaf(values[0])), (1, leaf(values[1]))]);
            assert!(!Rc::ptr_eq(&roots[0].1, &roots[1].1));
            for ((_, root), expected) in roots.iter().zip(values) {
                let SummaryExpr::KeepPreAsap(expr) = &root.expr else {
                    panic!()
                };
                let QueryExpr::Literal(ScalarValue::Float64(actual)) = expr.as_ref() else {
                    panic!()
                };
                assert_eq!(actual.to_bits(), expected.to_bits());
                assert_eq!(1.0 / actual, 1.0 / expected);
            }
        }
    }

    // Exact expression wrappers must retain signed zero too; JSON's null
    // encoding of nonfinite floats must never become the equality decision.
    #[test]
    fn nested_values_and_nonfinite_values_remain_distinct() {
        let wrapped = |value| {
            Rc::new(SummaryNode {
                expr: SummaryExpr::KeepPreAsap(Rc::new(QueryExpr::promql_scalar(value))),
                ..leaf(1.0).as_ref().clone()
            })
        };
        for (a, b) in [
            (0.0, -0.0),
            (f64::INFINITY, f64::NEG_INFINITY),
            (f64::NAN, f64::NAN),
        ] {
            let roots = share_common_summary_subtrees(vec![(0, wrapped(a)), (1, wrapped(b))]);
            assert!(!Rc::ptr_eq(&roots[0].1, &roots[1].1));
        }
        let roots = share_common_summary_subtrees(vec![
            (0, wrapped(f64::INFINITY)),
            (1, wrapped(f64::INFINITY)),
        ]);
        assert!(Rc::ptr_eq(&roots[0].1, &roots[1].1));
    }

    // Distinct quantile readouts share only a compatible typed sketch producer.
    #[test]
    fn quantile_roots_share_producer_but_not_readout_or_parameters() {
        use crate::post_asap::{
            GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SketchQuery,
            SummaryFamilyType, SummaryUpdate,
        };
        use crate::pre_asap::{ColumnRef, Reduction};
        fn readout(q: f64, alpha: f64) -> Rc<SummaryNode> {
            let producer = Rc::new(SummaryNode {
                expr: SummaryExpr::SummaryAgg {
                    child: leaf(1.0),
                    family: SummaryFamilyType::Sketch(
                        SketchKind::new(
                            SketchAlgorithm::DDSketch,
                            SketchParams::DDSketch { alpha },
                        ),
                        GroupingStrategy::default(),
                    ),
                    input: SummaryUpdate::column(ColumnRef::SampleValue),
                    reduction: Reduction::PerEntity,
                    grouping: GroupingStrategy::default(),
                },
                schema: SummarySchema {
                    fields: vec![],
                    time_index: None,
                },
                guarantee: None,
            });
            Rc::new(SummaryNode {
                expr: SummaryExpr::SummaryEstimate {
                    summary_input: producer,
                    query: SketchQuery::Quantile { q },
                },
                schema: SummarySchema {
                    fields: vec![],
                    time_index: None,
                },
                guarantee: None,
            })
        }
        let roots = share_common_summary_subtrees(vec![
            ("p95", readout(0.95, 0.01)),
            ("p99", readout(0.99, 0.01)),
            ("strict", readout(0.95, 0.001)),
        ]);
        let producer = |root: &Rc<SummaryNode>| match &root.expr {
            SummaryExpr::SummaryEstimate { summary_input, .. } => Rc::clone(summary_input),
            _ => panic!(),
        };
        assert!(!Rc::ptr_eq(&roots[0].1, &roots[1].1));
        assert!(Rc::ptr_eq(&producer(&roots[0].1), &producer(&roots[1].1)));
        assert!(!Rc::ptr_eq(&producer(&roots[0].1), &producer(&roots[2].1)));
    }

    // Fifty unique input nodes must not require walking an expanded 2^24 tree.
    // The timeout is a coarse runaway guard, not a performance SLA.
    #[test]
    fn shared_diamond_does_not_expand_during_comparison() {
        let (done, completion) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            fn diamond() -> Rc<SummaryNode> {
                let mut current = leaf(1.0);
                for _ in 0..24 {
                    current = Rc::new(SummaryNode {
                        expr: SummaryExpr::BinaryOp {
                            lhs: Rc::clone(&current),
                            rhs: current,
                            operator: super::super::BinaryOperator {
                                kind: crate::pre_asap::BinaryOpKind::Arithmetic(
                                    crate::pre_asap::ArithmeticOpKind::Add,
                                ),
                                vector_match: None,
                            },
                        },
                        schema: super::super::SummarySchema {
                            fields: vec![],
                            time_index: None,
                        },
                        guarantee: None,
                    });
                }
                current
            }
            let roots = share_common_summary_subtrees(vec![(0, diamond()), (1, diamond())]);
            assert!(Rc::ptr_eq(&roots[0].1, &roots[1].1));
            done.send(()).unwrap();
        });
        completion
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("comparison expanded the shared DAG");
        worker.join().unwrap();
    }
}
