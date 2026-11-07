//! $\boldsymbol\theta^{\min}_{c,i}$ from an input's error cap
//! (concepts doc, sections 2 and 4): the weakest parameters whose
//! $\varepsilon$, for the statistic read, is within the cap.

use asap_accuracy::estimators::local_guarantee;
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::stages::parameter_list;

/// The parameter floor of reading `statistic` from a state of `family`
/// within error `cap`: the per-parameter minimum ([`weakest`]) of the
/// listed configurations ($\boldsymbol\theta$ in `parameter_list`) whose
/// $\varepsilon$ is at most `cap` (or not a constant, which only Stage 3
/// can bind). A listed configuration that dominates it is admitted even if
/// its own $\varepsilon$ is larger, so the floor never excludes one that
/// meets the cap: pruning stays safe. `None` (no floor) when every listed
/// configuration meets the cap, none does (Stage 3 rejects the plans), or
/// `cap` is infinite.
pub(crate) fn floor_for_cap(
    family: &FieldDataType,
    algorithm: SketchAlgorithm,
    statistic: &SketchStatistic,
    cap: f64,
) -> Option<SketchParams> {
    if !cap.is_finite() {
        return None;
    }
    let list = parameter_list(algorithm);
    let meets: Vec<&SketchParams> = list
        .iter()
        .filter(|parameters| {
            local_guarantee(family, Some(parameters), statistic)
                .and_then(|guarantee| guarantee.spec.error.evaluate())
                .is_none_or(|error| error <= cap)
        })
        .collect();
    if meets.is_empty() || meets.len() == list.len() {
        return None;
    }
    weakest_of(meets.into_iter().cloned())
}

/// The [`weakest`] of `floors`; `None` when there is none or two do not
/// combine.
pub(crate) fn weakest_of(floors: impl IntoIterator<Item = SketchParams>) -> Option<SketchParams> {
    floors
        .into_iter()
        .map(Some)
        .reduce(|left, right| weakest(left.as_ref()?, right.as_ref()?))
        .flatten()
}

/// The per-parameter minimum of two configurations of one family (the
/// larger DDSketch `alpha`): a floor both satisfy. `None` for two families
/// or a Hydra configuration, whose parameters no list gives.
pub(crate) fn weakest(left: &SketchParams, right: &SketchParams) -> Option<SketchParams> {
    combine(left, right, false)
}

/// The per-parameter maximum of two floors of one family (the smaller
/// DDSketch `alpha`): what satisfies both. `None` for two families or a
/// Hydra configuration.
pub(crate) fn strongest(left: &SketchParams, right: &SketchParams) -> Option<SketchParams> {
    combine(left, right, true)
}

/// [`weakest`] (`stronger` false) or [`strongest`] (true). Each parameter
/// that sets $\varepsilon$ or holds items is combined; the recorded error
/// and failure probability are the left one's (every listed configuration
/// of a family records the same failure probability).
fn combine(left: &SketchParams, right: &SketchParams, stronger: bool) -> Option<SketchParams> {
    use SketchParams as P;
    let pick = |a: u32, b: u32| if stronger { a.max(b) } else { a.min(b) };
    Some(match (left, right) {
        (
            P::Kll {
                k,
                failure_probability,
            },
            P::Kll { k: other, .. },
        ) => P::Kll {
            k: pick(*k, *other),
            failure_probability: *failure_probability,
        },
        (P::Kmv { k, relative_error }, P::Kmv { k: other, .. }) => P::Kmv {
            k: pick(*k, *other),
            relative_error: *relative_error,
        },
        (P::DDSketch { alpha }, P::DDSketch { alpha: other }) => P::DDSketch {
            alpha: if stronger {
                alpha.min(*other)
            } else {
                alpha.max(*other)
            },
        },
        (
            P::Hll {
                precision,
                relative_error,
            },
            P::Hll {
                precision: other, ..
            },
        ) => P::Hll {
            precision: if stronger {
                (*precision).max(*other)
            } else {
                (*precision).min(*other)
            },
            relative_error: *relative_error,
        },
        (P::Cms { width, depth }, P::Cms { width: w, depth: d }) => P::Cms {
            width: pick(*width, *w),
            depth: pick(*depth, *d),
        },
        (P::CountSketch { width, depth }, P::CountSketch { width: w, depth: d }) => {
            P::CountSketch {
                width: pick(*width, *w),
                depth: pick(*depth, *d),
            }
        }
        (
            P::CmsWithHeap {
                width,
                depth,
                heap_size,
            },
            P::CmsWithHeap {
                width: w,
                depth: d,
                heap_size: h,
            },
        ) => P::CmsWithHeap {
            width: pick(*width, *w),
            depth: pick(*depth, *d),
            heap_size: pick(*heap_size, *h),
        },
        (
            P::CountSketchWithHeap {
                width,
                depth,
                heap_size,
            },
            P::CountSketchWithHeap {
                width: w,
                depth: d,
                heap_size: h,
            },
        ) => P::CountSketchWithHeap {
            width: pick(*width, *w),
            depth: pick(*depth, *d),
            heap_size: pick(*heap_size, *h),
        },
        (
            P::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
                distinct_failure_probability,
            },
            P::UnivMon {
                heap_size: h,
                sketch_rows: r,
                sketch_cols: c,
                layers: l,
                ..
            },
        ) => P::UnivMon {
            heap_size: pick(*heap_size, *h),
            sketch_rows: pick(*sketch_rows, *r),
            sketch_cols: pick(*sketch_cols, *c),
            layers: if stronger {
                (*layers).max(*l)
            } else {
                (*layers).min(*l)
            },
            distinct_failure_probability: *distinct_failure_probability,
        },
        _ => return None,
    })
}
