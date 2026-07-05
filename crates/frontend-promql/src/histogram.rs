//! Sample-type metadata for the `histogram_quantile` discrimination (issue #79).
//!
//! `histogram_quantile(φ, m)` has two lowerings: exact interpolation over
//! classic cumulative `le` buckets (`AggIntent::HistogramQuantile`, **not**
//! sketch-able) versus the generic sketch-able `Quantile` (native histograms /
//! raw samples, which L4 can approximate to an accuracy target). The true
//! signal is the argument's **sample type**, which query structure only
//! *proxies* — see the structural `is_classic_bucket_arg` heuristic, whose
//! false-positive (`…_bucket`-named non-histogram) and false-negative
//! (suffix-less classic histogram) cases this metadata fixes.
//!
//! A client that knows its sample types supplies a [`HistogramCatalog`]; it is
//! consulted first, and the structural heuristic remains the fallback when a
//! metric is undeclared.

use std::cell::RefCell;
use std::collections::HashMap;

/// The physical sample type behind a histogram metric — the true signal for
/// whether `histogram_quantile` over it can be re-sketched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistogramKind {
    /// Classic cumulative `le` buckets — pre-aggregated counts. The
    /// distribution can't be reconstructed from them, so it is **not**
    /// sketch-able: `histogram_quantile` is exact bucket interpolation.
    ClassicBucket,
    /// Native (exponential) histogram — sketch-able to an accuracy target.
    Native,
    /// Raw float samples the client retains — sketch-able. This is the case the
    /// generic `Quantile` lowering exists for (a client holding raw samples can
    /// build a quantile sketch even though the user wrote `histogram_quantile`).
    RawSamples,
}

impl HistogramKind {
    /// Whether `histogram_quantile` over this kind lowers to the sketch-able
    /// generic `Quantile` (`true`) rather than exact bucket interpolation.
    pub fn is_sketchable(self) -> bool {
        !matches!(self, HistogramKind::ClassicBucket)
    }
}

/// Metric-name → declared [`HistogramKind`]. Supplied by a client that knows its
/// sample types, to drive the `histogram_quantile` discrimination from metadata
/// instead of query structure (issue #79).
#[derive(Debug, Clone, Default)]
pub struct HistogramCatalog(HashMap<String, HistogramKind>);

impl HistogramCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare `metric`'s sample type (builder style).
    pub fn with(mut self, metric: impl Into<String>, kind: HistogramKind) -> Self {
        self.0.insert(metric.into(), kind);
        self
    }

    /// The declared kind for `metric`, if any.
    pub fn kind_of(&self, metric: &str) -> Option<HistogramKind> {
        self.0.get(metric).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

thread_local! {
    static CURRENT: RefCell<Option<HistogramCatalog>> = const { RefCell::new(None) };
}

/// RAII guard installing `catalog` as the ambient histogram catalog for the
/// current thread, restoring the prior value on drop.
///
/// Lowering is synchronous and processes one query at a time, so a thread-local
/// ambient catalog cleanly injects this read-only metadata into the deep,
/// free-function `walk` recursion without threading a parameter through every
/// signature (the discrimination is consulted in exactly one place,
/// `walk_histogram`).
pub(crate) struct CatalogGuard(Option<HistogramCatalog>);

impl CatalogGuard {
    pub(crate) fn install(catalog: HistogramCatalog) -> Self {
        let prev = CURRENT.with(|c| c.borrow_mut().replace(catalog));
        CatalogGuard(prev)
    }
}

impl Drop for CatalogGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.0.take());
    }
}

/// The ambient catalog's declared kind for `metric`, or `None` when no catalog
/// is installed or the metric is undeclared (→ fall back to the heuristic).
pub(crate) fn current_kind_of(metric: &str) -> Option<HistogramKind> {
    CURRENT.with(|c| c.borrow().as_ref().and_then(|cat| cat.kind_of(metric)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_classic_buckets_are_not_sketchable() {
        assert!(!HistogramKind::ClassicBucket.is_sketchable());
        assert!(HistogramKind::Native.is_sketchable());
        assert!(HistogramKind::RawSamples.is_sketchable());
    }

    #[test]
    fn catalog_lookup() {
        let cat = HistogramCatalog::new()
            .with("classic", HistogramKind::ClassicBucket)
            .with("raw", HistogramKind::RawSamples);
        assert_eq!(cat.kind_of("classic"), Some(HistogramKind::ClassicBucket));
        assert_eq!(cat.kind_of("raw"), Some(HistogramKind::RawSamples));
        assert_eq!(cat.kind_of("unknown"), None);
    }

    #[test]
    fn guard_installs_and_restores_the_ambient_catalog() {
        assert_eq!(current_kind_of("m"), None);
        {
            let _g = CatalogGuard::install(
                HistogramCatalog::new().with("m", HistogramKind::ClassicBucket),
            );
            assert_eq!(current_kind_of("m"), Some(HistogramKind::ClassicBucket));
        }
        // Restored to empty after the guard drops.
        assert_eq!(current_kind_of("m"), None);
    }
}
