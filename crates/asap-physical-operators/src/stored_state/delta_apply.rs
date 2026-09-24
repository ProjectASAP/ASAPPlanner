//! Shared sketch state reconstruction and decoding.
use asap_sketchlib::CountMinSketch;
use asap_sketchlib::CountMinSketchWithHeap;
use asap_sketchlib::CountSketch;
use asap_sketchlib::CountSketchWithHeap;
use asap_sketchlib::DdSketch;
use asap_sketchlib::HllSketch;
use asap_sketchlib::HllVariant;
use asap_sketchlib::KllSketch;
use asap_sketchlib::MessagePackCodec;

use super::decoders::{
    decode_cms_from_msgpack, decode_cms_from_proto, decode_cms_from_proto_delta,
    decode_cms_with_heap_from_msgpack, decode_cms_with_heap_from_msgpack_delta,
    decode_cs_from_msgpack, decode_cs_from_proto, decode_cs_from_proto_delta,
    decode_cs_with_heap_from_msgpack, decode_cs_with_heap_from_msgpack_delta,
};
use super::{SketchEncoding, SketchSampleState};

/// Which sketch family a candidate is, and the parameters needed to
/// *bootstrap an empty state* — required by the per-window-reset (PWR)
/// delta model where a window's FIRST frame is a delta-from-empty (no
/// carry-in Full). Most families' deltas embed their own params in the
/// wire fragment (decoded independently, then merged in — see
/// `SummaryState::apply_delta_bytes`); HLL register deltas and DD's
/// bucket-index deltas are applied onto a pre-sized structure instead,
/// so those two need the params known up front to allocate it.
#[derive(Debug, Clone, Copy)]
pub enum DeltaSketchKind {
    UnivMon {
        heap_size: u32,
        sketch_rows: u32,
        sketch_cols: u32,
        layers: u8,
    },
    DDSketch {
        alpha: f64,
    },
    Hll {
        precision: u32,
    },
    Kll {
        k: u32,
    },
    Cms {
        rows: usize,
        cols: usize,
    },
    CountSketch {
        rows: usize,
        cols: usize,
    },
    /// `CmsWithHeap` wraps `asap_sketchlib::CountMinSketchWithHeap`
    /// (min-over-rows estimator) and `CountSketchWithHeap` wraps the
    /// distinct `asap_sketchlib::CountSketchWithHeap` (median-of-signed-rows
    /// estimator) -- different algorithms that happen to share a storage
    /// shape. Kept as two variants (not one shared `Heap`) so
    /// `merge_same_family` rejects merging one into the other the same
    /// way it already rejects e.g. merging a `Cms` into a `Kll`; now the
    /// type system enforces it too, since the two variants hold different
    /// Rust types.
    CmsWithHeap {
        rows: usize,
        cols: usize,
        heap_size: usize,
    },
    CountSketchWithHeap {
        rows: usize,
        cols: usize,
        heap_size: usize,
    },
}

impl DeltaSketchKind {
    /// Construct an EMPTY state for this kind, used to seed a new window
    /// when its first frame is a delta-from-empty (PWR). A delta applied
    /// onto this empty base reconstructs exactly that window's state
    /// (delta-from-empty ⊕ empty = window state).
    fn bootstrap_empty(&self) -> SummaryState {
        match self {
            Self::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
            } => SummaryState::UnivMon(
                crate::summary_operators::univmon_accumulator::UnivMonAccumulator::new(
                    *heap_size as usize,
                    *sketch_rows as usize,
                    *sketch_cols as usize,
                    *layers as usize,
                )
                .expect("validated UnivMon catalog dimensions"),
            ),
            DeltaSketchKind::DDSketch { alpha } => SummaryState::Dd(DdSketch::new(*alpha)),
            DeltaSketchKind::Kll { k } => SummaryState::Kll(KllSketch::new(*k as u16)),
            DeltaSketchKind::Hll { precision } => {
                SummaryState::Hll(HllSketch::new(HllVariant::Regular, *precision))
            }
            DeltaSketchKind::Cms { rows, cols } => {
                SummaryState::Cms(CountMinSketch::new(*rows, *cols))
            }
            DeltaSketchKind::CountSketch { rows, cols } => {
                SummaryState::CountSketch(CountSketch::new(*rows, *cols))
            }
            DeltaSketchKind::CmsWithHeap {
                rows,
                cols,
                heap_size,
            } => SummaryState::CmsWithHeap(CountMinSketchWithHeap::new(*rows, *cols, *heap_size)),
            DeltaSketchKind::CountSketchWithHeap {
                rows,
                cols,
                heap_size,
            } => SummaryState::CountSketchWithHeap(CountSketchWithHeap::new(
                *rows, *cols, *heap_size,
            )),
        }
    }
}

/// Try to decode a "full" sketch from the bytes (used by both
/// per-window and cumulative modes when the encoding is `*Full`).
fn decode_full(
    kind: &DeltaSketchKind,
    bytes: &[u8],
    encoding: SketchEncoding,
) -> Result<SummaryState, String> {
    match (kind, encoding) {
        (
            DeltaSketchKind::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
            },
            SketchEncoding::MsgpackFull,
        ) => {
            let state =
                crate::summary_operators::univmon_accumulator::UnivMonAccumulator::from_bytes(
                    bytes,
                )
                .map_err(|e| e.to_string())?;
            if state.dimensions()
                != (
                    *heap_size as usize,
                    *sketch_rows as usize,
                    *sketch_cols as usize,
                    *layers as usize,
                )
            {
                return Err("UnivMon payload dimensions differ from installed catalog".into());
            }
            Ok(SummaryState::UnivMon(state))
        }
        (DeltaSketchKind::DDSketch { .. }, SketchEncoding::ProtoFull) => {
            let sk = dd_from_proto(bytes)?;
            Ok(SummaryState::Dd(sk))
        }
        (DeltaSketchKind::DDSketch { .. }, SketchEncoding::MsgpackFull) => {
            let sk = DdSketch::from_msgpack(bytes)
                .map_err(|e| format!("deserialize DDSketch msgpack: {e}"))?;
            Ok(SummaryState::Dd(sk))
        }
        (DeltaSketchKind::Hll { .. }, SketchEncoding::ProtoFull) => {
            let sk = hll_from_proto(bytes)?;
            Ok(SummaryState::Hll(sk))
        }
        (DeltaSketchKind::Hll { .. }, SketchEncoding::MsgpackFull) => {
            let sk = HllSketch::from_msgpack(bytes)
                .map_err(|e| format!("deserialize HllSketch msgpack: {e}"))?;
            Ok(SummaryState::Hll(sk))
        }
        (DeltaSketchKind::Kll { .. }, SketchEncoding::ProtoFull) => {
            let sk = kll_from_proto(bytes)?;
            Ok(SummaryState::Kll(sk))
        }
        (DeltaSketchKind::Kll { .. }, SketchEncoding::MsgpackFull) => {
            let sk = KllSketch::from_msgpack(bytes)
                .map_err(|e| format!("deserialize KllSketch msgpack: {e}"))?;
            Ok(SummaryState::Kll(sk))
        }
        (DeltaSketchKind::Cms { .. }, SketchEncoding::ProtoFull) => {
            Ok(SummaryState::Cms(decode_cms_from_proto(bytes)?))
        }
        (DeltaSketchKind::Cms { .. }, SketchEncoding::MsgpackFull) => {
            Ok(SummaryState::Cms(decode_cms_from_msgpack(bytes)?))
        }
        (DeltaSketchKind::CountSketch { .. }, SketchEncoding::ProtoFull) => {
            Ok(SummaryState::CountSketch(decode_cs_from_proto(bytes)?))
        }
        (DeltaSketchKind::CountSketch { .. }, SketchEncoding::MsgpackFull) => {
            Ok(SummaryState::CountSketch(decode_cs_from_msgpack(bytes)?))
        }
        // The heap-bearing wire format is msgpack-only in this
        // deployment; `decode_cms_with_heap_from_msgpack` is the same
        // "Full" decoder the reducer's existing per-frame dispatch falls
        // through to for any non-MsgpackDelta encoding.
        (
            DeltaSketchKind::CmsWithHeap { .. },
            SketchEncoding::ProtoFull | SketchEncoding::MsgpackFull,
        ) => Ok(SummaryState::CmsWithHeap(
            decode_cms_with_heap_from_msgpack(bytes)?,
        )),
        (
            DeltaSketchKind::CountSketchWithHeap { .. },
            SketchEncoding::ProtoFull | SketchEncoding::MsgpackFull,
        ) => Ok(SummaryState::CountSketchWithHeap(
            decode_cs_with_heap_from_msgpack(bytes)?,
        )),
        (_, e) => Err(format!("decode_full called with non-Full encoding {e:?}")),
    }
}

/// The reconstructed state one candidate sid contributes — either
/// folded across a window (or several) via delta application, or merged
/// in from another sid's own reconstruction.
pub enum SummaryState {
    UnivMon(crate::summary_operators::univmon_accumulator::UnivMonAccumulator),
    Dd(DdSketch),
    Hll(HllSketch),
    Kll(KllSketch),
    Cms(CountMinSketch),
    CountSketch(CountSketch),
    /// See `DeltaSketchKind::CmsWithHeap`/`CountSketchWithHeap` for why
    /// these are two variants holding two different sketchlib types.
    CmsWithHeap(CountMinSketchWithHeap),
    CountSketchWithHeap(CountSketchWithHeap),
}

impl SummaryState {
    /// Apply a delta-encoded payload from a window sample. For DD / KLL,
    /// the delta is interpreted as a "mergeable fragment" decoded
    /// through the same full-state decoder and merged into the
    /// rolling state. For HLL, the wire delta is a sparse register
    /// update applied via the sketch's `apply_delta`.
    ///
    /// On encoding mismatch (e.g. trying to apply an HllDelta to a
    /// DDSketch rolling state) returns Err.
    pub fn apply_delta_bytes(
        &mut self,
        bytes: &[u8],
        encoding: SketchEncoding,
    ) -> Result<(), String> {
        if !matches!(
            encoding,
            SketchEncoding::ProtoDelta | SketchEncoding::MsgpackDelta
        ) {
            return Err(format!(
                "apply_delta_bytes called with non-Delta encoding {encoding:?}"
            ));
        }
        match self {
            SummaryState::UnivMon(_) => Err("UnivMon requires full pane snapshots".into()),
            SummaryState::Dd(sk) => {
                match encoding {
                    // PROTO_DELTA: dispatch on the payload SHAPE, mirroring the
                    // supported DDSketch frame decoder, which tries the
                    // full-envelope decode first, then falls back to
                    // the bucket-delta proto. Two wire shapes can arrive on the
                    // ProtoDelta channel:
                    //
                    //   1. `SketchEnvelope{DdSketchState}` — a full-state
                    //      fragment, mergeable via `DdSketch::merge`. (The edge
                    //      sends this when `compute_delta_against` hits the
                    //      empty-current / undecodable-prior fallback and ships
                    //      a full snapshot tagged as a delta.)
                    //   2. `DDSketchDelta { buckets: [{index, d_count}] }` — a
                    //      bucket-index delta proto, applied additively. This is
                    //      the COMMON delta_transmission frame the edge emits
                    //      under per-window-reset (`compute_delta(&empty)`).
                    //
                    // Before this fix the reducer decoded ONLY shape (1) via
                    // `decode_full`. A real shape-(2) frame failed with a wire-
                    // type mismatch on field 1 (delta field 1 = repeated
                    // submessage; state field 1 = `double alpha`) → the whole
                    // `quantile_over_time` returned `No result` for every
                    // delta_transmission DDSketch stream. We wrap the rolling
                    // `DdSketch` in a transient accumulator so the bucket-delta
                    // apply lands on `sk` in place.
                    SketchEncoding::ProtoDelta => {
                        // Shape (1): full envelope fragment → merge. Try this
                        // first (cheap decode attempt; a bucket-delta proto
                        // fails it on the field-1 wire-type mismatch).
                        if let Ok(SummaryState::Dd(other)) = decode_full(
                            &DeltaSketchKind::DDSketch { alpha: 0.0 },
                            bytes,
                            SketchEncoding::ProtoFull,
                        ) {
                            sk.merge(&other)
                                .map_err(|e| format!("merge DDSketch delta envelope: {e}"))?;
                            return Ok(());
                        }
                        // Shape (2): bucket-delta proto → additive apply via the
                        // SAME decoder the ingest delta path uses.
                        use crate::summary_operators::dd_sketch_accumulator::DDSketchAccumulator;
                        let mut acc = DDSketchAccumulator {
                            inner: std::mem::replace(sk, DdSketch::new(sk.alpha)),
                            sample_p: 1.0,
                        };
                        let res = acc.apply_proto_delta_bytes(bytes);
                        *sk = acc.inner;
                        res.map_err(|e| format!("apply DDSketch proto bucket-delta: {e}"))?;
                        Ok(())
                    }
                    // MSGPACK_DELTA: a serialized full-sketch fragment, mergeable
                    // via the full-state decoder. Kept for completeness — the
                    // edge wires PROTO_DELTA for DDSketch today.
                    SketchEncoding::MsgpackDelta => {
                        let other = match decode_full(
                            &DeltaSketchKind::DDSketch { alpha: 0.0 },
                            bytes,
                            SketchEncoding::MsgpackFull,
                        ) {
                            Ok(SummaryState::Dd(s)) => s,
                            Ok(_) => {
                                return Err(
                                    "decode_full(DDSketch) returned non-DDSketch state".to_string()
                                )
                            }
                            Err(e) => return Err(e),
                        };
                        sk.merge(&other)
                            .map_err(|e| format!("merge DDSketch delta: {e}"))?;
                        Ok(())
                    }
                    _ => unreachable!(),
                }
            }
            SummaryState::Hll(sk) => {
                // HLL has a true sparse register delta in the proto
                // wire format. Use the same path the precompute
                // accumulator uses (`apply_proto_delta_bytes`-style).
                if encoding == SketchEncoding::ProtoDelta {
                    apply_hll_proto_delta(sk, bytes)
                } else {
                    // MsgpackDelta for HLL isn't a sparse encoding;
                    // it's a serialized HllSketch fragment, mergeable
                    // via `HllSketch::merge`.
                    let other = HllSketch::from_msgpack(bytes)
                        .map_err(|e| format!("deserialize HllSketch (delta-as-msgpack): {e}"))?;
                    sk.merge(&other)
                        .map_err(|e| format!("merge HLL delta: {e}"))?;
                    Ok(())
                }
            }
            SummaryState::Kll(sk) => {
                let full_enc = match encoding {
                    SketchEncoding::ProtoDelta => SketchEncoding::ProtoFull,
                    SketchEncoding::MsgpackDelta => SketchEncoding::MsgpackFull,
                    _ => unreachable!(),
                };
                let other = match decode_full(&DeltaSketchKind::Kll { k: 0 }, bytes, full_enc) {
                    Ok(SummaryState::Kll(s)) => s,
                    Ok(_) => return Err("decode_full(Kll) returned non-Kll state".to_string()),
                    Err(e) => return Err(e),
                };
                sk.merge(&other)
                    .map_err(|e| format!("merge KLL delta: {e}"))?;
                Ok(())
            }
            // CMS/CountSketch/Heap have no true sparse in-place delta
            // (unlike DD's bucket-index proto or HLL's register proto,
            // above) — every delta frame already decodes into a
            // complete, standalone state on its own (the PWR wire
            // contract resets to empty at the source), so applying one
            // is always "decode independently, then merge".
            SummaryState::Cms(sk) => {
                if encoding != SketchEncoding::ProtoDelta {
                    return Err(
                        "CountMin (heap-less) MSGPACK_DELTA is not a valid producer encoding \
                         (msgpack-delta is the heap-bearing form)"
                            .to_string(),
                    );
                }
                let other = decode_cms_from_proto_delta(bytes)?;
                sk.merge(&other)
                    .map_err(|e| format!("merge CountMinSketch delta: {e}"))
            }
            SummaryState::CountSketch(sk) => {
                if encoding != SketchEncoding::ProtoDelta {
                    return Err(
                        "CountSketch (heap-less) MSGPACK_DELTA is not a valid producer encoding \
                         (msgpack-delta is the heap-bearing form)"
                            .to_string(),
                    );
                }
                let other = decode_cs_from_proto_delta(bytes)?;
                sk.merge(&other)
                    .map_err(|e| format!("merge CountSketch delta: {e}"))
            }
            SummaryState::CmsWithHeap(sk) => {
                // Matches the existing per-frame reducer dispatch: only
                // MsgpackDelta gets true delta treatment; ProtoDelta (not
                // produced for this family in this deployment) falls
                // through to the full-msgpack decoder, same as `decode_full`.
                let other = if encoding == SketchEncoding::MsgpackDelta {
                    decode_cms_with_heap_from_msgpack_delta(bytes)?
                } else {
                    decode_cms_with_heap_from_msgpack(bytes)?
                };
                sk.merge(&other)
                    .map_err(|e| format!("merge CmsWithHeap delta: {e}"))
            }
            SummaryState::CountSketchWithHeap(sk) => {
                let other = if encoding == SketchEncoding::MsgpackDelta {
                    decode_cs_with_heap_from_msgpack_delta(bytes)?
                } else {
                    decode_cs_with_heap_from_msgpack(bytes)?
                };
                sk.merge(&other)
                    .map_err(|e| format!("merge CountSketchWithHeap delta: {e}"))
            }
        }
    }

    pub fn quantile(&self, q: f64) -> f64 {
        match self {
            SummaryState::Dd(sk) => sk.quantile(q).unwrap_or(0.0),
            SummaryState::Kll(sk) => sk.quantile(q),
            _ => 0.0,
        }
    }

    pub fn cardinality(&self) -> f64 {
        match self {
            SummaryState::Hll(sk) => sk.estimate(),
            _ => 0.0,
        }
    }

    /// The bucket TOTAL — sum of row 0 of the underlying matrix. What a
    /// bare `count_over_time`/`sum by (item) (rate(...))`-shaped query
    /// (no specific item key) reads out. `0.0` for non-Frequency-family
    /// states.
    pub fn total(&self) -> f64 {
        let matrix = match self {
            SummaryState::Cms(c) => c.sketch(),
            SummaryState::CountSketch(c) => c.sketch().clone(),
            SummaryState::CmsWithHeap(h) => h.sketch_matrix(),
            SummaryState::CountSketchWithHeap(h) => h.sketch_matrix(),
            _ => return 0.0,
        };
        matrix
            .first()
            .map(|row| row.iter().copied().sum::<f64>())
            .unwrap_or(0.0)
    }

    /// Per-key point estimate — `count(metric{item="x"})`-shaped queries.
    /// Unlike [`Self::topk_items`], no heap is needed: all four Frequency
    /// variants (heap-bearing or not) already carry a keyed `estimate`
    /// over their matrix. `None` for the quantile/cardinality states,
    /// which have no item universe at all.
    pub fn estimate(&self, key: &str) -> Option<f64> {
        match self {
            SummaryState::Cms(c) => Some(c.estimate(key)),
            SummaryState::CountSketch(c) => Some(c.estimate(key)),
            SummaryState::CmsWithHeap(h) => Some(h.estimate(key)),
            SummaryState::CountSketchWithHeap(h) => Some(h.estimate(key)),
            _ => None,
        }
    }

    /// Top-k `(key, value)` pairs from the heap, descending by value.
    /// `None` for anything other than a heap-bearing state — the
    /// heap-less Frequency states (`Cms`/`CountSketch`) carry no item
    /// universe to enumerate, and the quantile/cardinality states have
    /// no heap at all.
    pub fn topk_items(&self) -> Option<Vec<(String, f64)>> {
        match self {
            SummaryState::CmsWithHeap(h) => Some(
                h.topk_heap_items()
                    .into_iter()
                    .map(|item| (item.key, item.value))
                    .collect(),
            ),
            SummaryState::CountSketchWithHeap(h) => Some(
                h.topk_heap_items()
                    .into_iter()
                    .map(|item| (item.key, item.value))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Merge `other` into `self` in place — both must be the same sketch
    /// family. Used to combine several sids' reconstructed states
    /// (`cumulative_summary_state`/`per_window_summary_states`) into one
    /// cross-sid answer. `CmsWithHeap`/`CountSketchWithHeap` fall through
    /// to the catch-all mismatch arm below like any other mixed pair —
    /// and since the two variants now hold distinct sketchlib types
    /// (`CountMinSketchWithHeap` vs `CountSketchWithHeap`), there is no
    /// arm that could accidentally match them together — see their doc
    /// on `DeltaSketchKind`.
    pub fn merge_same_family(&mut self, other: &SummaryState) -> Result<(), String> {
        match (self, other) {
            (SummaryState::UnivMon(a), SummaryState::UnivMon(b)) => {
                a.merge_in_place(b).map_err(|e| e.to_string())
            }
            (SummaryState::Dd(a), SummaryState::Dd(b)) => {
                a.merge(b).map_err(|e| format!("merge DDSketch: {e}"))
            }
            (SummaryState::Hll(a), SummaryState::Hll(b)) => {
                a.merge(b).map_err(|e| format!("merge HLL: {e}"))
            }
            (SummaryState::Kll(a), SummaryState::Kll(b)) => {
                a.merge(b).map_err(|e| format!("merge KLL: {e}"))
            }
            (SummaryState::Cms(a), SummaryState::Cms(b)) => {
                a.merge(b).map_err(|e| format!("merge CountMinSketch: {e}"))
            }
            (SummaryState::CountSketch(a), SummaryState::CountSketch(b)) => {
                a.merge(b).map_err(|e| format!("merge CountSketch: {e}"))
            }
            (SummaryState::CmsWithHeap(a), SummaryState::CmsWithHeap(b)) => {
                a.merge(b).map_err(|e| format!("merge CmsWithHeap: {e}"))
            }
            (SummaryState::CountSketchWithHeap(a), SummaryState::CountSketchWithHeap(b)) => a
                .merge(b)
                .map_err(|e| format!("merge CountSketchWithHeap: {e}")),
            (a, _) => Err(format!(
                "SummaryState family mismatch in merge_same_family (self is {})",
                a.family_name()
            )),
        }
    }

    /// Diagnostic family name for error messages — not used for dispatch.
    fn family_name(&self) -> &'static str {
        match self {
            SummaryState::UnivMon(_) => "UnivMon",
            SummaryState::Dd(_) => "DDSketch",
            SummaryState::Hll(_) => "Hll",
            SummaryState::Kll(_) => "Kll",
            SummaryState::Cms(_) => "Cms",
            SummaryState::CountSketch(_) => "CountSketch",
            SummaryState::CmsWithHeap(_) => "CmsWithHeap",
            SummaryState::CountSketchWithHeap(_) => "CountSketchWithHeap",
        }
    }
}

/// Fold every in-range window's frames for ONE series into a single
/// merged `SummaryState` (cumulative over `[t0, t1]`), returning `None`
/// if no Full frame ever landed (every sample was a leading delta). The
/// per-sid building block for a cross-sid answer: reconstruct each
/// candidate sid's state this way, then merge them (`merge_same_family`)
/// before reading out a quantile/cardinality over the combined data.
pub fn cumulative_summary_state(
    samples: &[(i64, &SketchSampleState)],
    kind: DeltaSketchKind,
) -> Result<Option<SummaryState>, String> {
    let mut rolling: Option<SummaryState> = None;
    for (_window_end, state) in samples {
        match state.encoding {
            SketchEncoding::ProtoFull | SketchEncoding::MsgpackFull => {
                let new_state = decode_full(&kind, &state.bytes, state.encoding)?;
                rolling = Some(match rolling.take() {
                    None => new_state,
                    Some(mut prev) => {
                        prev.merge_same_family(&new_state)?;
                        prev
                    }
                });
            }
            SketchEncoding::ProtoDelta | SketchEncoding::MsgpackDelta => {
                if rolling.is_none() {
                    rolling = Some(kind.bootstrap_empty());
                }
                if let Some(rs) = rolling.as_mut() {
                    rs.apply_delta_bytes(&state.bytes, state.encoding)?;
                }
            }
        }
    }
    Ok(rolling)
}

#[cfg(test)]
/// Walk a sorted-by-window-end slice of samples in time order and
/// produce ONE per-window scalar `(window_end_ms, scalar)`.
///
/// ## Per-window-reset (PWR) delta model
///
/// The edge emits frames grouped by window (all frames of one window
/// share the same `window_end` key; the key changes across windows).
/// The edge RESETS its snapshot base at each window boundary, so each
/// window's state is built *from empty*:
///
/// * Within a window, frames accumulate to the window total. The first
///   frame may be a `Full` (window 1, or a periodic re-snapshot) or a
///   `Delta`-from-empty (windows 2+ under PWR); subsequent frames are
///   `Delta` INCREMENTS applied onto the window's running base.
/// * Across windows, the base MUST reset — a new `window_end` discards
///   the previous window's rolling state and starts from empty. Never
///   carry one window's state into the next (that would inflate via
///   cross-window accumulation).
///
/// Concretely this fixes two bugs in the old "single rolling Option that
/// only ever resets on a Full" walk:
///   1. A query range whose Full lives only in window 1 (or out of
///      range) left windows 2+ as deltas with `rolling=None`, all
///      skipped → empty result.
///   2. A window 2+ delta applied onto window 1's leftover rolling state
///      → cross-window inflation.
///
/// For a `Delta` that is the window's FIRST frame (the PWR delta-from-
/// empty case), we bootstrap an EMPTY rolling state of `kind` and apply
/// the delta onto it (delta-from-empty ⊕ empty = that window's state).
///
/// The delta-OFF path (exactly one `Full` per window) still produces one
/// correct value per window: the window opens with a Full, has no
/// further frames, and emits that Full's scalar.
///
/// `eval` reads a scalar from the rolling state (`quantile(q)` /
/// `cardinality()`). `skipped` counts frames that could not contribute
/// (a delta we genuinely couldn't bootstrap from — should be rare).
///
/// Returns `Ok(per_window_samples, skipped)`.
pub fn per_window_evaluate<E>(
    samples: &[(i64, &SketchSampleState)],
    kind: DeltaSketchKind,
    eval: E,
) -> Result<(Vec<(i64, f64)>, usize), String>
where
    E: Fn(&SummaryState) -> f64,
{
    let (states, skipped) = per_window_summary_states(samples, kind)?;
    Ok((
        states.into_iter().map(|(w, rs)| (w, eval(&rs))).collect(),
        skipped,
    ))
}

/// Walk a sorted-by-window-end slice of samples in time order and
/// reconstruct ONE sid's per-window `SummaryState` (same per-window-reset
/// walk as [`per_window_evaluate`], generalized to return the
/// reconstructed state itself instead of an already-evaluated scalar).
/// The per-sid building block for cross-sid per-window merging (unlike
/// [`cumulative_summary_state`], which folds a whole `[t0, t1]` range
/// into one answer, this keeps each window separate so a caller can
/// merge same-window states across several sids before evaluating --
/// needed for a matrix/range-query answer, where each output point is
/// itself a cross-sid merge for that one window).
///
/// Returns `Ok((per_window_states, skipped))`.
pub fn per_window_summary_states(
    samples: &[(i64, &SketchSampleState)],
    kind: DeltaSketchKind,
) -> Result<(Vec<(i64, SummaryState)>, usize), String> {
    let mut out: Vec<(i64, SummaryState)> = Vec::new();
    let mut skipped = 0usize;

    // Rolling state for the CURRENT window only. Reset to None whenever
    // `window_end` changes (a new window establishes its own base from
    // empty). `cur_end` tracks which window `rolling` belongs to.
    let mut rolling: Option<SummaryState> = None;
    let mut cur_end: Option<i64> = None;

    for (window_end, state) in samples {
        // Window boundary: flush the previous window's final accumulated
        // state, then reset the base so this window starts from empty.
        if cur_end != Some(*window_end) {
            if let (Some(prev_end), Some(rs)) = (cur_end, rolling.take()) {
                out.push((prev_end, rs));
            }
            cur_end = Some(*window_end);
        }

        match state.encoding {
            SketchEncoding::ProtoFull | SketchEncoding::MsgpackFull => {
                // A Full (re)sets this window's base.
                rolling = Some(decode_full(&kind, &state.bytes, state.encoding)?);
            }
            SketchEncoding::ProtoDelta | SketchEncoding::MsgpackDelta => {
                // Apply onto this window's running base. If this is the
                // window's first frame (PWR delta-from-empty), bootstrap
                // an empty base and apply onto it.
                if rolling.is_none() {
                    rolling = Some(kind.bootstrap_empty());
                }
                match rolling.as_mut() {
                    Some(rs) => rs.apply_delta_bytes(&state.bytes, state.encoding)?,
                    None => skipped += 1,
                }
            }
        }
    }

    // Flush the final window.
    if let (Some(prev_end), Some(rs)) = (cur_end, rolling.take()) {
        out.push((prev_end, rs));
    }

    Ok((out, skipped))
}

// ---------------------------------------------------------------------------
// Proto-envelope decoders — P2-4: ONE decoder per family.
//
// These delegate to the precompute-side accumulators'
// `from_sketchlib_proto_bytes`, which are the single source of truth for
// the modified-OTLP proto wire format (envelope unwrapping, alpha/k/
// precision validation, and — critically for HLL — SPARSE
// `registers_sparse` expansion). Folding the warm read path onto the
// same decoder the ingest path uses means the sparse-register fix (and
// any future format change) can never drift between the two copies again
// — the bug class P2-3 / P2-4 closed. We extract the accumulator's
// public `inner` sketch for the rolling-state merge.
// ---------------------------------------------------------------------------

fn dd_from_proto(buffer: &[u8]) -> Result<DdSketch, String> {
    use crate::summary_operators::dd_sketch_accumulator::DDSketchAccumulator;
    DDSketchAccumulator::from_sketchlib_proto_bytes(buffer)
        .map(|acc| acc.inner)
        .map_err(|e| e.to_string())
}

fn kll_from_proto(buffer: &[u8]) -> Result<KllSketch, String> {
    use crate::summary_operators::datasketches_kll_accumulator::DatasketchesKLLAccumulator;
    DatasketchesKLLAccumulator::from_sketchlib_proto_bytes(buffer)
        .map(|acc| acc.inner)
        .map_err(|e| e.to_string())
}

fn hll_from_proto(buffer: &[u8]) -> Result<HllSketch, String> {
    use crate::summary_operators::hll_sketch_accumulator::HllSketchAccumulator;
    HllSketchAccumulator::from_sketchlib_proto_bytes(buffer)
        .map(|acc| acc.inner)
        .map_err(|e| e.to_string())
}

/// Apply a proto-encoded `HllDelta` frame onto the HLL register vector — the
/// delta is a varint-packed (index_delta, value) blob; decode + apply
/// (register-wise max) via the shared sketch library so the unpacking stays a
/// single source of truth.
fn apply_hll_proto_delta(sk: &mut HllSketch, buffer: &[u8]) -> Result<(), String> {
    sk.apply_delta_bytes(buffer)
        .map_err(|e| format!("apply HLLDelta: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! P2-3 / P2-4 regression tests for the consolidated single-decoder
    //! path. These exercise the family proto decoders that now delegate
    //! to the precompute accumulators (the single source of truth), so a
    //! divergence between the warm read path and the ingest path —
    //! notably the SPARSE-register HLL handling the deleted dead decoder
    //! got wrong — fails the build.
    use super::*;
    use asap_sketchlib::HllVariant;

    fn encode_dd(sk: &DdSketch) -> Vec<u8> {
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, DdSketchState, SketchEnvelope};
        use prost::Message;
        let state = DdSketchState {
            alpha: sk.alpha,
            store_counts: sk.store_counts.clone(),
            store_offset: sk.store_offset,
        };
        SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Ddsketch(state)),
            ..Default::default()
        }
        .encode_to_vec()
    }

    fn encode_kll(k: u16, items: &[f64]) -> Vec<u8> {
        use asap_sketchlib::proto::sketchlib::{sketch_envelope, KllState, SketchEnvelope};
        use prost::Message;
        let state = KllState {
            k: k as u32,
            items: items.to_vec(),
            levels: vec![],
            num_levels: 0,
            ..Default::default()
        };
        SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Kll(state)),
            ..Default::default()
        }
        .encode_to_vec()
    }

    fn encode_hll_dense(sk: &HllSketch) -> Vec<u8> {
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, HllVariant as ProtoVariant, HyperLogLogState, SketchEnvelope,
        };
        use prost::Message;
        let state = HyperLogLogState {
            variant: ProtoVariant::Regular as i32,
            precision: sk.precision,
            registers: sk.registers.clone(),
            hip_kxq0: sk.hip_kxq0,
            hip_kxq1: sk.hip_kxq1,
            hip_est: sk.hip_est,
            registers_sparse: None,
        };
        SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Hll(state)),
            ..Default::default()
        }
        .encode_to_vec()
    }

    /// Build a SPARSE HLL proto frame: dense `registers` left empty,
    /// `registers_sparse.packed` = varint (index_delta, value) pairs.
    /// This is exactly the wire form a low-cardinality producer emits
    /// (sketchlib-go below its dense/sparse crossover) — the frame the
    /// DELETED `HllSketch_from_sketchlib_proto_bytes` hard-rejected with
    /// "registers has 0 bytes".
    fn encode_hll_sparse(precision: u32, nonzero: &[(u64, u8)]) -> Vec<u8> {
        use asap_sketchlib::proto::sketchlib::{
            sketch_envelope, HllSparseRegisters, HllVariant as ProtoVariant, HyperLogLogState,
            SketchEnvelope,
        };
        use prost::Message;
        // Varint-pack (index_delta, value), ascending index order.
        let mut packed: Vec<u8> = Vec::new();
        let mut prev: u64 = 0;
        let put_uvarint = |buf: &mut Vec<u8>, mut v: u64| loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                buf.push(b | 0x80);
            } else {
                buf.push(b);
                break;
            }
        };
        let mut sorted = nonzero.to_vec();
        sorted.sort_by_key(|(i, _)| *i);
        for (idx, val) in &sorted {
            put_uvarint(&mut packed, idx - prev);
            put_uvarint(&mut packed, *val as u64);
            prev = *idx;
        }
        let state = HyperLogLogState {
            variant: ProtoVariant::Regular as i32,
            precision,
            registers: Vec::new(), // dense field empty → sparse path
            hip_kxq0: 0.0,
            hip_kxq1: 0.0,
            hip_est: 0.0,
            // `num_registers` is informational — the decoder expands
            // against `expected_len` from precision, not this field.
            registers_sparse: Some(HllSparseRegisters {
                num_registers: 1u32 << precision,
                packed,
            }),
        };
        SketchEnvelope {
            sketch_state: Some(sketch_envelope::SketchState::Hll(state)),
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn hll_from_proto_accepts_sparse_frame() {
        // The consolidated decoder must accept the sparse wire form (the
        // deleted dead decoder rejected it). Build a sparse frame setting
        // a handful of registers, decode it, and confirm those register
        // slots came back set in the dense array.
        let precision = 12u32;
        let nonzero = [(3u64, 5u8), (100, 2), (4000, 7)];
        let bytes = encode_hll_sparse(precision, &nonzero);
        let sk = hll_from_proto(&bytes).expect("sparse HLL frame must decode (P2-3 regression)");
        assert_eq!(sk.registers.len(), 1usize << precision);
        for (idx, val) in nonzero {
            assert_eq!(
                sk.registers[idx as usize], val,
                "sparse register {idx} expanded to wrong value"
            );
        }
    }

    #[test]
    fn hll_from_proto_matches_accumulator_decoder() {
        // P2-4: the warm read path and the ingest accumulator must decode
        // the SAME bytes to the SAME sketch (one source of truth).
        use crate::summary_operators::hll_sketch_accumulator::HllSketchAccumulator;
        let mut sk = HllSketch::new(HllVariant::Regular, 12);
        for i in 0..500u64 {
            sk.update(format!("item-{i}").as_bytes());
        }
        let bytes = encode_hll_dense(&sk);
        let via_delta = hll_from_proto(&bytes).expect("delta_apply hll decode");
        let via_acc = HllSketchAccumulator::from_sketchlib_proto_bytes(&bytes)
            .expect("accumulator hll decode")
            .inner;
        assert_eq!(
            via_delta.registers, via_acc.registers,
            "delta_apply and accumulator must produce identical HLL registers"
        );
        assert!((via_delta.estimate() - via_acc.estimate()).abs() < 1e-9);
    }

    #[test]
    fn dd_from_proto_matches_accumulator_decoder() {
        use crate::summary_operators::dd_sketch_accumulator::DDSketchAccumulator;
        let mut sk = DdSketch::new(0.01);
        for v in [1.0, 2.0, 5.0, 5.0, 9.0, 42.0] {
            sk.update(v);
        }
        let bytes = encode_dd(&sk);
        let via_delta = dd_from_proto(&bytes).expect("delta_apply dd decode");
        let via_acc = DDSketchAccumulator::from_sketchlib_proto_bytes(&bytes)
            .expect("accumulator dd decode")
            .inner;
        // Same quantile answers from the same bytes through both paths.
        assert_eq!(via_delta.quantile(0.5), via_acc.quantile(0.5));
        assert_eq!(via_delta.quantile(0.99), via_acc.quantile(0.99));
    }

    #[test]
    fn kll_from_proto_matches_accumulator_decoder() {
        use crate::summary_operators::datasketches_kll_accumulator::DatasketchesKLLAccumulator;
        let items: Vec<f64> = (0..200).map(|i| i as f64).collect();
        let bytes = encode_kll(256, &items);
        let via_delta = kll_from_proto(&bytes).expect("delta_apply kll decode");
        let via_acc = DatasketchesKLLAccumulator::from_sketchlib_proto_bytes(&bytes)
            .expect("accumulator kll decode")
            .inner;
        assert_eq!(via_delta.quantile(0.5), via_acc.quantile(0.5));
    }

    // -----------------------------------------------------------------
    // Per-window-reset (PWR) delta-apply regression tests.
    //
    // The edge resets its snapshot base at every window boundary, so a
    // window's first frame is either a Full (window 1 / re-snapshot) or
    // a Delta-from-empty (windows 2+). The query-side walk must:
    //   * reset the rolling base when `window_end` changes,
    //   * bootstrap an empty base for a window's leading Delta,
    //   * emit ONE value per window (the window's final accumulated
    //     state), never per-frame and never cross-window-accumulated.
    // -----------------------------------------------------------------

    fn full(bytes: Vec<u8>) -> SketchSampleState {
        SketchSampleState {
            bytes,
            encoding: SketchEncoding::ProtoFull,
        }
    }
    fn delta(bytes: Vec<u8>) -> SketchSampleState {
        SketchSampleState {
            bytes,
            encoding: SketchEncoding::ProtoDelta,
        }
    }

    fn dd_over(alpha: f64, vals: &[f64]) -> DdSketch {
        let mut sk = DdSketch::new(alpha);
        for &v in vals {
            sk.update(v);
        }
        sk
    }

    /// PWR across 3 windows: window 1 is `[Full]`, windows 2 & 3 are
    /// `[Delta-from-empty]` (NO Full carry-in). Each window must
    /// reconstruct its OWN distribution's median — not empty (the old
    /// "skip delta with no base" bug) and not cross-window-inflated.
    #[test]
    fn pwr_ddsketch_three_windows_delta_from_empty() {
        let alpha = 0.01;
        let w1 = dd_over(alpha, &[1.0, 2.0, 3.0, 4.0, 5.0]);
        let w2 = dd_over(alpha, &[10.0, 20.0, 30.0, 40.0, 50.0]);
        let w3 = dd_over(alpha, &[100.0, 200.0, 300.0, 400.0, 500.0]);

        // window 1 ships a Full; windows 2+ ship a delta-from-empty.
        let s1 = full(encode_dd(&w1));
        let s2 = delta(encode_dd(&w2));
        let s3 = delta(encode_dd(&w3));
        let samples = vec![(1000_i64, &s1), (2000, &s2), (3000, &s3)];

        let kind = DeltaSketchKind::DDSketch { alpha };
        let (out, skipped) =
            per_window_evaluate(&samples, kind, |rs| rs.quantile(0.5)).expect("pwr eval");
        assert_eq!(skipped, 0, "PWR must not skip delta-from-empty frames");
        assert_eq!(out.len(), 3, "one value per window");

        // Each window's median ≈ that window's own distribution median,
        // independent of the others (no carry-in inflation).
        let truth = [
            w1.quantile(0.5).unwrap(),
            w2.quantile(0.5).unwrap(),
            w3.quantile(0.5).unwrap(),
        ];
        for (i, (w_end, est)) in out.iter().enumerate() {
            assert_eq!(*w_end, (i as i64 + 1) * 1000);
            let rel = (est - truth[i]).abs() / truth[i].max(1e-9);
            assert!(
                rel < 0.05,
                "window {i}: est={est} truth={} rel={rel}",
                truth[i]
            );
        }
        // Cross-window-inflation guard: window 2's median must NOT have
        // absorbed window 1 (would pull it well below 30).
        assert!(
            out[1].1 > 20.0,
            "window 2 median {} suggests cross-window accumulation",
            out[1].1
        );
    }

    /// Sub-window producer: a SINGLE window carries multiple frames
    /// `[Full, Delta, Delta]`, where each later delta is an increment
    /// since the previous emit in that window. The walk must COLLAPSE
    /// them to ONE value = the window's running total, not emit three.
    #[test]
    fn pwr_ddsketch_subwindow_frames_collapse_to_window_total() {
        let alpha = 0.01;
        // Three sub-window increments that together cover 1..=15.
        let a = dd_over(alpha, &[1.0, 2.0, 3.0, 4.0, 5.0]);
        let b = dd_over(alpha, &[6.0, 7.0, 8.0, 9.0, 10.0]);
        let c = dd_over(alpha, &[11.0, 12.0, 13.0, 14.0, 15.0]);
        let s_a = full(encode_dd(&a));
        let s_b = delta(encode_dd(&b));
        let s_c = delta(encode_dd(&c));
        // All three share the same window_end (one window, sub-window frames).
        let samples = vec![(5000_i64, &s_a), (5000, &s_b), (5000, &s_c)];

        let kind = DeltaSketchKind::DDSketch { alpha };
        let (out, skipped) =
            per_window_evaluate(&samples, kind, |rs| rs.quantile(0.5)).expect("subwindow eval");
        assert_eq!(skipped, 0);
        assert_eq!(out.len(), 1, "sub-window frames collapse to ONE value");
        assert_eq!(out[0].0, 5000);

        let truth = dd_over(alpha, &(1..=15).map(|v| v as f64).collect::<Vec<_>>())
            .quantile(0.5)
            .unwrap();
        let rel = (out[0].1 - truth).abs() / truth.max(1e-9);
        assert!(rel < 0.05, "window total est={} truth={truth}", out[0].1);
    }

    /// Same sub-window collapse, but the window's FIRST frame is a
    /// Delta-from-empty (PWR window 2+ with sub-window frames):
    /// `[Delta-from-empty, Delta, Delta]`.
    #[test]
    fn pwr_ddsketch_subwindow_first_frame_delta_from_empty() {
        let alpha = 0.01;
        let a = dd_over(alpha, &[1.0, 2.0, 3.0, 4.0, 5.0]);
        let b = dd_over(alpha, &[6.0, 7.0, 8.0, 9.0, 10.0]);
        let c = dd_over(alpha, &[11.0, 12.0, 13.0, 14.0, 15.0]);
        let s_a = delta(encode_dd(&a)); // first frame is delta-from-empty
        let s_b = delta(encode_dd(&b));
        let s_c = delta(encode_dd(&c));
        let samples = vec![(9000_i64, &s_a), (9000, &s_b), (9000, &s_c)];

        let kind = DeltaSketchKind::DDSketch { alpha };
        let (out, skipped) =
            per_window_evaluate(&samples, kind, |rs| rs.quantile(0.5)).expect("eval");
        assert_eq!(skipped, 0);
        assert_eq!(out.len(), 1);
        let truth = dd_over(alpha, &(1..=15).map(|v| v as f64).collect::<Vec<_>>())
            .quantile(0.5)
            .unwrap();
        let rel = (out[0].1 - truth).abs() / truth.max(1e-9);
        assert!(rel < 0.05, "est={} truth={truth}", out[0].1);
    }

    /// PWR for HLL across 3 windows, each a Delta-from-empty (sparse
    /// register delta). Bootstrapping an EMPTY HLL of the right precision
    /// is required (register deltas index into a pre-sized array). Each
    /// window's cardinality must reflect its OWN item set.
    #[test]
    fn pwr_hll_three_windows_delta_from_empty() {
        let precision = 12u32;
        // Build per-window HLLs, then encode each as a register-delta
        // against an EMPTY sketch (= that window's full register state,
        // the PWR delta-from-empty wire form).
        let empty = HllSketch::new(HllVariant::Regular, precision);
        let mut frames = Vec::new();
        let truths = [200usize, 800, 1500];
        for (w, &n) in truths.iter().enumerate() {
            let mut sk = HllSketch::new(HllVariant::Regular, precision);
            let base = (w as u64) * 100_000; // disjoint item sets per window
            for i in 0..n as u64 {
                sk.update(format!("u-{}", base + i).as_bytes());
            }
            let bytes = sk.compute_delta(&empty, 0);
            frames.push((((w as u64) + 1) * 1000, delta(bytes)));
        }
        let samples: Vec<(i64, &SketchSampleState)> =
            frames.iter().map(|(t, s)| (*t as i64, s)).collect();

        let kind = DeltaSketchKind::Hll { precision };
        let (out, skipped) =
            per_window_evaluate(&samples, kind, |rs| rs.cardinality()).expect("hll pwr eval");
        assert_eq!(skipped, 0, "HLL delta-from-empty must bootstrap, not skip");
        assert_eq!(out.len(), 3);
        for (i, (_w_end, est)) in out.iter().enumerate() {
            let n = truths[i] as f64;
            let rel = (est - n).abs() / n;
            assert!(
                rel < 0.15,
                "window {i}: HLL est={est} truth={n} rel={rel} (each window independent)"
            );
        }
    }

    /// `CmsWithHeap` (min-over-rows estimator, `CountMinSketchWithHeap`)
    /// and `CountSketchWithHeap` (median-of-signed-rows estimator, the
    /// distinct `CountSketchWithHeap` type) are different sketch
    /// algorithms that merely happen to share a storage shape — merging
    /// one into the other must be rejected as a family mismatch, the
    /// same as merging a `Cms` into a `Kll` would be. Since the two
    /// `SummaryState` variants now hold genuinely different Rust types,
    /// this is also enforced at compile time — there is no arm in
    /// `merge_same_family` that type-checks a mixed pair together.
    #[test]
    fn cms_with_heap_and_count_sketch_with_heap_are_not_the_same_family() {
        use asap_sketchlib::{CountMinSketchWithHeap, CountSketchWithHeap, MessagePackCodec};

        let mut cms_heap = CountMinSketchWithHeap::new(4, 256, 10);
        cms_heap.update("a", 1.0);
        let mut cs_heap = CountSketchWithHeap::new(4, 256, 10);
        cs_heap.update("b", 1.0);

        let mut a = SummaryState::CmsWithHeap(
            CountMinSketchWithHeap::from_msgpack(&cms_heap.to_msgpack().unwrap()).unwrap(),
        );
        let b = SummaryState::CountSketchWithHeap(
            CountSketchWithHeap::from_msgpack(&cs_heap.to_msgpack().unwrap()).unwrap(),
        );

        match a.merge_same_family(&b) {
            Err(msg) => assert!(
                msg.contains("family mismatch"),
                "expected a family-mismatch error, got: {msg}"
            ),
            Ok(()) => panic!(
                "CmsWithHeap must not merge with CountSketchWithHeap -- \
                 different algorithms sharing only a storage shape"
            ),
        }
    }

    fn encode_delta_heap(
        rows: u32,
        cols: u32,
        cells: &[(u32, u32, i64)],
        heap: &[(&str, f64)],
        heap_size: u64,
    ) -> Vec<u8> {
        #[derive(serde::Serialize)]
        struct W<'a>(
            bool,
            (u32, u32, &'a [(u32, u32, i64)]),
            Vec<(String, f64)>,
            u64,
        );
        let heap_owned: Vec<(String, f64)> =
            heap.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let w = W(true, (rows, cols, cells), heap_owned, heap_size);
        rmp_serde::to_vec(&w).expect("encode delta-heap")
    }

    /// `SummaryState::CountSketchWithHeap` must decode both FULL and
    /// DELTA-HEAP msgpack frames through the genuine
    /// `asap_sketchlib::CountSketchWithHeap` (median-of-signed-rows
    /// estimator) rather than the CMS-family `CountMinSketchWithHeap`
    /// (min-over-rows estimator) it used to alias — the bug this split
    /// fixed. Built via real `update()` calls (not a hand-crafted matrix)
    /// so the sign-hashed row semantics are genuinely exercised, then
    /// checks both decode paths reproduce the same matrix and the same
    /// `estimate()` as the in-memory sketch they were encoded from.
    #[test]
    fn count_sketch_with_heap_full_and_delta_decode_via_new_asap_sketchlib_type() {
        use asap_sketchlib::{CountSketchWithHeap, MessagePackCodec};

        let mut built = CountSketchWithHeap::new(4, 64, 10);
        for _ in 0..50 {
            built.update("k", 1.0);
        }
        let expected_matrix = built.sketch_matrix();
        let expected_estimate = built.estimate("k");

        // FULL path.
        let full_bytes = built.to_msgpack().expect("encode full CountSketchWithHeap");
        let full_state = decode_full(
            &DeltaSketchKind::CountSketchWithHeap {
                rows: 4,
                cols: 64,
                heap_size: 10,
            },
            &full_bytes,
            SketchEncoding::MsgpackFull,
        )
        .expect("decode_full CountSketchWithHeap");
        match full_state {
            SummaryState::CountSketchWithHeap(inner) => {
                assert_eq!(inner.sketch_matrix(), expected_matrix);
                assert_eq!(inner.estimate("k"), expected_estimate);
            }
            other => panic!(
                "expected CountSketchWithHeap state, got {}",
                other.family_name()
            ),
        }

        // DELTA-HEAP path: same cells + heap against an empty base (PWR
        // contract), encoded the way the Go producer does.
        let cells: Vec<(u32, u32, i64)> = expected_matrix
            .iter()
            .enumerate()
            .flat_map(|(r, row)| {
                row.iter().enumerate().filter_map(move |(c, v)| {
                    if *v != 0.0 {
                        Some((r as u32, c as u32, *v as i64))
                    } else {
                        None
                    }
                })
            })
            .collect();
        let heap_pairs: Vec<(String, f64)> = built
            .topk_heap_items()
            .into_iter()
            .map(|item| (item.key, item.value))
            .collect();
        assert!(!heap_pairs.is_empty(), "expected \"k\" in the top-k heap");
        let heap_refs: Vec<(&str, f64)> =
            heap_pairs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let delta_bytes = encode_delta_heap(4, 64, &cells, &heap_refs, 10);

        let mut rolling = DeltaSketchKind::CountSketchWithHeap {
            rows: 4,
            cols: 64,
            heap_size: 10,
        }
        .bootstrap_empty();
        rolling
            .apply_delta_bytes(&delta_bytes, SketchEncoding::MsgpackDelta)
            .expect("apply CountSketchWithHeap delta");
        match rolling {
            SummaryState::CountSketchWithHeap(inner) => {
                assert_eq!(
                    inner.sketch_matrix(),
                    expected_matrix,
                    "delta path must reconstruct the identical matrix"
                );
                assert_eq!(inner.estimate("k"), expected_estimate);
            }
            other => panic!(
                "expected CountSketchWithHeap state, got {}",
                other.family_name()
            ),
        }
    }
}
