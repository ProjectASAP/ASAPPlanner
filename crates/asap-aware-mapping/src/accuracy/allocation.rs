//! Allocate end-to-end error and failure budgets across approximate layers.
use super::*;

/// The shape of a composition an allocator splits a budget across.
#[derive(Debug, Clone, PartialEq)]
pub struct CompositionShape {
    /// The metric the composed guarantee will carry — decides whether the
    /// budget composes additively (`Σ ε_i ≤ ε`) or multiplicatively
    /// (`Π(1+ε_i) ≤ 1+ε`).
    pub metric: ErrorMetric,
    /// How many approximate layers share the budget (≥ 1).
    pub approximate_layer_count: usize,
}

/// One way of splitting an end-to-end target across a composition's
/// approximate layers. `layers[0]` is the outermost layer's local target;
/// the remainder are the inner layers', outermost first.
#[derive(Debug, Clone, PartialEq)]
pub struct AccuracyAllocation {
    pub allocator: &'static str,
    pub layers: Vec<AccuracyTarget>,
}

impl AccuracyAllocation {
    /// The end-to-end budget left for everything below `layers[0]` — what
    /// the inner subtree must satisfy as a whole (it re-splits internally).
    /// `None` for a single-layer allocation.
    pub fn inner_target(&self, shape: &CompositionShape) -> Option<AccuracyTarget> {
        let inner = &self.layers[1..];
        if inner.is_empty() {
            return None;
        }
        let (eps, delta): (Vec<f64>, Vec<Option<f64>>) = inner
            .iter()
            .map(|t| match t {
                AccuracyTarget::Exact => (0.0, Some(0.0)),
                AccuracyTarget::Epsilon(e) => (*e, None),
                AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, Some(*delta)),
            })
            .unzip();
        let epsilon = match shape.metric {
            ErrorMetric::RelativeValue => eps.iter().map(|e| 1.0 + e).product::<f64>() - 1.0,
            _ => eps.iter().sum(),
        };
        Some(match delta.iter().copied().sum::<Option<f64>>() {
            Some(delta) => AccuracyTarget::EpsilonDelta { epsilon, delta },
            None => AccuracyTarget::Epsilon(epsilon),
        })
    }
}

/// Enumerates the finite set of budget splits the search tries for one
/// composition. Exposed as its own hook because equal splitting is rarely
/// cost-optimal; a deployment can return several candidate splits and let
/// cost ranking pick among the legal ones.
pub trait AccuracyBudgetAllocator {
    fn allocations(
        &self,
        target: &AccuracyTarget,
        composition: &CompositionShape,
    ) -> Vec<AccuracyAllocation>;
}

/// The initial deterministic allocator: every approximate layer gets an
/// equal share — `ε_i = ε / n`, `δ_i = δ / n` for an additively composed
/// metric, and `ε_i = (1 + ε)^{1/n} − 1` for a multiplicatively composed
/// one — so the composed bound meets the target exactly with no slack.
/// `AccuracyTarget::Exact` yields no allocation: no approximate layer can
/// meet it.
#[derive(Debug, Default, Clone, Copy)]
pub struct EqualSplitAllocator;

impl AccuracyBudgetAllocator for EqualSplitAllocator {
    fn allocations(
        &self,
        target: &AccuracyTarget,
        composition: &CompositionShape,
    ) -> Vec<AccuracyAllocation> {
        let n = composition.approximate_layer_count.max(1);
        let (epsilon, delta) = match target {
            AccuracyTarget::Exact => return Vec::new(),
            AccuracyTarget::Epsilon(e) => (*e, None),
            AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, Some(*delta)),
        };
        if !(epsilon.is_finite() && epsilon > 0.0) {
            return Vec::new();
        }
        let local_epsilon = match composition.metric {
            ErrorMetric::RelativeValue => (1.0 + epsilon).powf(1.0 / n as f64) - 1.0,
            _ => epsilon / n as f64,
        };
        let layer = match delta {
            Some(delta) => AccuracyTarget::EpsilonDelta {
                epsilon: local_epsilon,
                delta: delta / n as f64,
            },
            None => AccuracyTarget::Epsilon(local_epsilon),
        };
        vec![AccuracyAllocation {
            allocator: "EqualSplitAllocator",
            layers: vec![layer; n],
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn equal_split_respects_the_root_epsilon_and_delta() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.1,
            delta: 0.02,
        };
        let shape = CompositionShape {
            metric: ErrorMetric::AbsoluteValue,
            approximate_layer_count: 2,
        };
        let allocations = EqualSplitAllocator.allocations(&target, &shape);
        assert_eq!(allocations.len(), 1);
        let layers = &allocations[0].layers;
        assert_eq!(layers.len(), 2);
        let (eps, deltas): (Vec<f64>, Vec<f64>) = layers
            .iter()
            .map(|t| match t {
                AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, *delta),
                other => panic!("unexpected {other:?}"),
            })
            .unzip();
        assert!((eps.iter().sum::<f64>() - 0.1).abs() < 1e-12);
        assert!((deltas.iter().sum::<f64>() - 0.02).abs() < 1e-12);
        assert_eq!(
            allocations[0].inner_target(&shape),
            Some(AccuracyTarget::EpsilonDelta {
                epsilon: 0.05,
                delta: 0.01
            })
        );

        // Multiplicative composition: (1+ε_i)^2 = 1+ε, not 2ε_i = ε.
        let rel_shape = CompositionShape {
            metric: ErrorMetric::RelativeValue,
            approximate_layer_count: 2,
        };
        let allocations =
            EqualSplitAllocator.allocations(&AccuracyTarget::Epsilon(0.21), &rel_shape);
        let AccuracyTarget::Epsilon(e) = allocations[0].layers[0] else {
            panic!()
        };
        assert!((e - 0.1).abs() < 1e-12);

        assert!(EqualSplitAllocator
            .allocations(&AccuracyTarget::Exact, &shape)
            .is_empty());
    }
}
