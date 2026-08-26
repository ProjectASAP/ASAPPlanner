//! Workload-aware reuse between compatible ordered limits.
//!
//! If two queries rank the same input and request top-k results with
//! `small_k < large_k`, the smaller result is exactly the first `small_k`
//! rows of the larger result.  This strategy therefore replaces
//! `Limit(small_k, Sort(X))` with `Limit(small_k, Limit(large_k, Sort(X)))`.

use std::rc::Rc;

use asap_types::pre_asap::QueryExpr;

use crate::replacement::{
    Replacement, ReplacementProvenance, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG,
};

/// Derives a smaller top-k result from a compatible larger top-k sibling.
pub struct TopKLimitReuseStrategy {
    limits: Vec<Rc<QueryExpr>>,
}

impl TopKLimitReuseStrategy {
    pub fn new(limits: &[Rc<QueryExpr>]) -> Self {
        Self {
            limits: limits.to_vec(),
        }
    }

    fn larger_sources<'a>(&'a self, target: &TargetSubDAG<'_>) -> Vec<&'a Rc<QueryExpr>> {
        let QueryExpr::Limit {
            n: target_n,
            offset: 0,
            child: target_child,
        } = target.root.as_ref()
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
                let QueryExpr::Limit {
                    n,
                    offset: 0,
                    child,
                } = candidate.as_ref()
                else {
                    return false;
                };
                n > target_n
                    && (Rc::ptr_eq(child, target_child) || child.as_ref() == target_child.as_ref())
            })
            .collect();
        // Prefer the smallest sufficient materialized top-k when several
        // larger siblings are available.
        sources.sort_by_key(|source| match source.as_ref() {
            QueryExpr::Limit { n, .. } => *n,
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
        let QueryExpr::Limit {
            n: target_n,
            offset: 0,
            ..
        } = target.root.as_ref()
        else {
            return Vec::new();
        };

        self.larger_sources(target)
            .into_iter()
            .map(|source| {
                let source_n = match source.as_ref() {
                    QueryExpr::Limit { n, .. } => *n,
                    _ => unreachable!(),
                };
                ReplacementSubDAG {
                    strategy: "TopKLimitReuseStrategy",
                    replacement: Replacement::Rewrite(Rc::new(QueryExpr::Limit {
                        n: *target_n,
                        offset: 0,
                        child: Rc::clone(source),
                    })),
                    provenance: ReplacementProvenance::LogicalRewrite,
                    rationale: format!(
                        "derives top-{target_n} from the compatible shared top-{source_n} result; both rank the identical input with the same ordering"
                    ),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::pre_asap::{Schema, Source};

    fn scan_named(metric: &str) -> Rc<QueryExpr> {
        Rc::new(QueryExpr::Scan {
            source: Source::TimeSeries {
                metric: metric.into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(vec![], 0, vec![]),
        })
    }

    #[test]
    fn smaller_limit_reuses_larger_compatible_limit() {
        let child = scan_named("m");
        let small = Rc::new(QueryExpr::Limit {
            n: 5,
            offset: 0,
            child: Rc::clone(&child),
        });
        let large = Rc::new(QueryExpr::Limit {
            n: 10,
            offset: 0,
            child,
        });
        let strategy = TopKLimitReuseStrategy::new(&[Rc::clone(&small), Rc::clone(&large)]);
        let replacements = strategy.replacements(&TargetSubDAG::new(&small));
        assert_eq!(replacements.len(), 1);
        let Replacement::Rewrite(rewrite) = &replacements[0].replacement else {
            panic!()
        };
        let QueryExpr::Limit { n: 5, child, .. } = rewrite.as_ref() else {
            panic!()
        };
        assert!(Rc::ptr_eq(child, &large));
    }

    #[test]
    fn offset_or_different_input_is_not_reused() {
        let a = scan_named("a");
        let b = scan_named("b");
        let small = Rc::new(QueryExpr::Limit {
            n: 5,
            offset: 0,
            child: a,
        });
        let large = Rc::new(QueryExpr::Limit {
            n: 10,
            offset: 0,
            child: b,
        });
        let offset = Rc::new(QueryExpr::Limit {
            n: 20,
            offset: 1,
            child: scan_named("a"),
        });
        let strategy = TopKLimitReuseStrategy::new(&[Rc::clone(&small), large, offset]);
        assert!(!strategy.matches(&TargetSubDAG::new(&small)));
    }
}
