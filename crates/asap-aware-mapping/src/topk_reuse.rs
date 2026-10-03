//! Workload-aware reuse between compatible ordered limits.
//!
//! If two queries rank the same input and request top-k results with
//! `small_k < large_k`, the smaller result is exactly the first `small_k`
//! rows of the larger result.  This strategy therefore replaces
//! `Limit(small_k, Sort(X))` with `Limit(small_k, Limit(large_k, Sort(X)))`.

use std::rc::Rc;

use asap_types::ir::{NonASAPOp, OperatorNode};

use crate::replacement::{
    Replacement, ReplacementProvenance, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG,
};

/// Derives a smaller top-k result from a compatible larger top-k sibling.
pub struct TopKLimitReuseStrategy {
    limits: Vec<Rc<OperatorNode>>,
}

impl TopKLimitReuseStrategy {
    pub fn new(limits: &[Rc<OperatorNode>]) -> Self {
        Self {
            limits: limits.to_vec(),
        }
    }

    fn larger_sources<'a>(&'a self, target: &TargetSubDAG<'_>) -> Vec<&'a Rc<OperatorNode>> {
        let Some(NonASAPOp::Limit {
            n: Some(target_n),
            offset: 0,
            child: target_child,
            ..
        }) = target.root.non_asap()
        else {
            return Vec::new();
        };

        let mut sources: Vec<_> = self
            .limits
            .iter()
            .filter(|candidate| {
                if Rc::ptr_eq(candidate, target.root) {
                    return false;
                }
                let Some(NonASAPOp::Limit {
                    n: Some(n),
                    offset: 0,
                    child,
                    ..
                }) = candidate.non_asap()
                else {
                    return false;
                };
                n > target_n
                    && (Rc::ptr_eq(child, target_child) || child.as_ref() == target_child.as_ref())
            })
            .collect();
        // Prefer the smallest sufficient materialized top-k when several
        // larger siblings are available.
        sources.sort_by_key(|source| match source.non_asap() {
            Some(NonASAPOp::Limit { n: Some(n), .. }) => *n,
            _ => unreachable!(),
        });
        sources
    }
}

impl ReplacementStrategy for TopKLimitReuseStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        !self.larger_sources(target).is_empty()
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        let Some(NonASAPOp::Limit {
            n: Some(target_n),
            offset: 0,
            partition_by,
            ..
        }) = target.root.non_asap()
        else {
            return Vec::new();
        };

        self.larger_sources(target)
            .into_iter()
            .filter_map(|source| {
                let source_n = match source.non_asap() {
                    Some(NonASAPOp::Limit { n: Some(n), .. }) => *n,
                    _ => unreachable!(),
                };
                let rewritten = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Limit {
                    n: Some(*target_n),
                    offset: 0,
                    partition_by: partition_by.clone(),
                    child: Rc::clone(source),
                }))
                .ok()?;
                Some(ReplacementSubDAG {
                    strategy: "TopKLimitReuseStrategy",
                    replacement: Replacement::SubDAG(rewritten),
                    provenance: ReplacementProvenance::LogicalRewrite,
                    rationale: format!(
                        "derives top-{target_n} from the compatible shared top-{source_n} result; both rank the identical input with the same ordering"
                    ),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scan;
    use asap_types::ir::operator::operator_properties::GroupKeys;
    use asap_types::ir::schema::Schema;

    fn scan_named(metric: &str) -> Rc<OperatorNode> {
        scan(metric, Schema::with_time_index(vec![], 0, vec![]))
    }

    fn limit(n: usize, offset: usize, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Limit {
            n: Some(n),
            offset,
            partition_by: GroupKeys::none(),
            child,
        }))
        .unwrap()
    }

    #[test]
    fn smaller_limit_reuses_larger_compatible_limit() {
        let child = scan_named("m");
        let small = limit(5, 0, Rc::clone(&child));
        let large = limit(10, 0, child);
        let strategy = TopKLimitReuseStrategy::new(&[Rc::clone(&small), Rc::clone(&large)]);
        let replacements = strategy.replacements(&TargetSubDAG::new(&small));
        assert_eq!(replacements.len(), 1);
        let Replacement::SubDAG(rewrite) = &replacements[0].replacement else {
            panic!()
        };
        let Some(NonASAPOp::Limit {
            n: Some(5), child, ..
        }) = rewrite.non_asap()
        else {
            panic!()
        };
        assert!(Rc::ptr_eq(child, &large));
    }

    #[test]
    fn offset_or_different_input_is_not_reused() {
        let a = scan_named("a");
        let b = scan_named("b");
        let small = limit(5, 0, a);
        let large = limit(10, 0, b);
        let offset = limit(20, 1, scan_named("a"));
        let strategy = TopKLimitReuseStrategy::new(&[Rc::clone(&small), large, offset]);
        assert!(!strategy.matches(&TargetSubDAG::new(&small)));
    }
}
