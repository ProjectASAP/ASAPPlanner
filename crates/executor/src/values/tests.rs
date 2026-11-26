use super::*;
use crate::summary_kernels::weighted_frequency::{FrequencyAlgorithm, WeightedFrequency};
use planner_types::summary_type::SketchAlgorithm;

// A state cannot acquire a different family merely by relabeling its
// batch; states of different shapes (θ) do not merge.
#[test]
fn weighted_state_family_must_match_and_shapes_must_merge() {
    let cms = SummaryFamilyType::Sketch(SketchAlgorithm::CountMinSketchWithHeap);
    let cs = SummaryFamilyType::Sketch(SketchAlgorithm::CountSketchWithHeap);
    let state = WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 32, 5, 8).unwrap();
    assert!(validate_state(&cs, &state).is_ok());
    assert!(validate_state(&cms, &state).is_err());
    // A Count Sketch without a heap is not a heap family's state.
    let no_heap = WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 32, 5, 0).unwrap();
    assert!(validate_state(&cs, &no_heap).is_err());
    let wrong_shape = WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 64, 5, 8).unwrap();
    assert!(validate_state(&cs, &wrong_shape).is_ok());
    assert!(state.merge(&wrong_shape).is_err());
}

/// The sealing rule (`StateColumn::sealed`) at its two sites: a chain's own
/// window (`StateColumnUpdater::seal`) and a chain level merged from
/// narrower windows (a `SummaryMerge` with a sealed output).
mod sealing {
    use super::super::{state_view_bytes, Batch, BatchColumn, MappedSketchKernel};
    use crate::operators::Operator;
    use crate::plan::PhysicalDAG;
    use crate::runtime::{Limits, RunContext, Scope};
    use crate::values::{StateColumn, StateStorage, Value};
    use crate::StateColumnUpdater;
    use arrow::array::{DictionaryArray, UInt32Array};
    use arrow::datatypes::UInt32Type;
    use futures::{executor::block_on, StreamExt};
    use planner_types::ir::schema::{DataType, Field, FieldDataType, Schema};
    use planner_types::stages::Lifetime;
    use planner_types::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic};
    use std::sync::Arc;

    const SERIES: u32 = 50;
    const WINDOWS: u32 = 12;

    /// Every sketch family a chain seals, with small θ: those a per-group
    /// updater builds, and Count-Min, which no updater builds (its states
    /// arrive stored) but a level merges.
    fn families() -> Vec<(FieldDataType, SketchParams)> {
        use SketchAlgorithm as A;
        vec![
            (
                FieldDataType::Sketch(A::DDSketch),
                SketchParams::DDSketch { alpha: 0.01 },
            ),
            (FieldDataType::Sketch(A::KLL), SketchParams::Kll { k: 200 }),
            (
                FieldDataType::Sketch(A::CountMinSketch),
                SketchParams::Cms {
                    width: 64,
                    depth: 3,
                },
            ),
            (
                FieldDataType::Sketch(A::HLL),
                SketchParams::Hll { precision: 10 },
            ),
            (FieldDataType::Sketch(A::KMV), SketchParams::Kmv { k: 64 }),
            (
                FieldDataType::Sketch(A::UnivMon),
                SketchParams::UnivMon {
                    heap_size: 8,
                    sketch_rows: 3,
                    sketch_cols: 64,
                    layers: 4,
                },
            ),
            (
                FieldDataType::Sketch(A::CountMinSketchWithHeap),
                SketchParams::CmsWithHeap {
                    width: 64,
                    depth: 3,
                    heap_size: 8,
                },
            ),
            (
                FieldDataType::Sketch(A::CountSketchWithHeap),
                SketchParams::CountSketchWithHeap {
                    width: 64,
                    depth: 3,
                    heap_size: 8,
                },
            ),
        ]
    }

    fn schema(family: &FieldDataType) -> Arc<Schema> {
        Arc::new(Schema {
            fields: vec![
                Field {
                    name: "series".into(),
                    dtype: FieldDataType::Plain(DataType::Utf8),
                    nullable: false,
                },
                Field {
                    name: "state".into(),
                    dtype: family.clone(),
                    nullable: false,
                },
            ],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        })
    }

    /// Window `window` of a chain: one state of `family` per series over
    /// `samples` samples each, sealed by the updater (the chain's own
    /// sealing site).
    fn window(family: &FieldDataType, params: &SketchParams, window: u32, samples: u32) -> Batch {
        if *family == FieldDataType::Sketch(SketchAlgorithm::CountMinSketch) {
            return count_min_window(params, window, samples);
        }
        let keyed = matches!(
            family,
            FieldDataType::Sketch(
                SketchAlgorithm::CountMinSketchWithHeap | SketchAlgorithm::CountSketchWithHeap
            )
        );
        let mut updater = StateColumnUpdater::new(family, Some(params)).unwrap();
        updater.ensure_groups(SERIES as usize).unwrap();
        for s in 0..SERIES {
            for sample in 0..samples {
                let n = s * 7 + window * samples + sample;
                if keyed {
                    let item = Value::Utf8(format!("k{}", n % 17).into());
                    updater.update_item(s, &[item], 1.0).unwrap();
                } else {
                    let t = i64::from(window * samples + sample) * 1_000;
                    updater.update(s, f64::from(n) * 0.5 + 1.0, t).unwrap();
                }
            }
        }
        let names: Vec<String> = (0..SERIES).map(|s| format!("s{s}")).collect();
        let names: DictionaryArray<UInt32Type> = names.iter().map(String::as_str).collect();
        Batch::try_from_columns(
            schema(family),
            vec![
                BatchColumn::Plain(Arc::new(names)),
                BatchColumn::States {
                    indices: UInt32Array::from_iter_values(0..SERIES),
                    states: Arc::new(updater.seal()),
                },
            ],
        )
        .unwrap()
    }

    /// A window of Count-Min states, one per series, counting `samples`
    /// keys each, built as objects: no updater builds a Count-Min state, so
    /// this is not a chain's sealing site.
    fn count_min_window(params: &SketchParams, window: u32, samples: u32) -> Batch {
        use crate::summary_kernels::CountMinSketchAccumulator;
        let SketchParams::Cms { width, depth } = params else {
            unreachable!("Count-Min θ")
        };
        let family = FieldDataType::Sketch(SketchAlgorithm::CountMinSketch);
        let rows = (0..SERIES)
            .map(|s| {
                let mut state =
                    CountMinSketchAccumulator::new(*depth as usize, *width as usize).unwrap();
                for sample in 0..samples {
                    state.update(&format!("k{}", (s + window + sample) % 17), 1.0);
                }
                vec![
                    Value::Utf8(format!("s{s}").into()),
                    Value::Summary {
                        family: family.clone(),
                        state: Arc::new(state),
                    },
                ]
            })
            .collect();
        Batch::try_new(schema(&family), rows).unwrap()
    }

    fn windows(family: &FieldDataType, params: &SketchParams, samples: u32) -> Vec<Batch> {
        (0..WINDOWS)
            .map(|w| window(family, params, w, samples))
            .collect()
    }

    /// `SummaryMerge` by series over `batches`; with `sealed`, a chain
    /// level (`SummaryMergeWindows`, the level's sealing site).
    fn merge(batches: Vec<Batch>, sealed: bool) -> Vec<Batch> {
        let schema = batches[0].schema().clone();
        let mut dag = PhysicalDAG::default();
        dag.add(
            0,
            vec![],
            Operator::source(schema.clone(), batches).unwrap(),
            Lifetime::None,
        )
        .unwrap();
        let merge = Operator::summary_merge(schema, 1, vec![0]).unwrap();
        let merge = if sealed {
            merge.with_sealed_output()
        } else {
            merge
        };
        dag.add(1, vec![0], merge, Lifetime::None).unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
            },
            Limits { max_bytes: 1 << 30 },
        )
        .unwrap();
        block_on(async {
            let mut output = dag.execute(&[1], context).unwrap().remove(0);
            let mut out = Vec::new();
            while let Some(batch) = output.next().await {
                out.push(Batch::clone(&batch.unwrap()));
            }
            out
        })
    }

    fn level(family: &FieldDataType, params: &SketchParams, samples: u32) -> Batch {
        let mut level = merge(windows(family, params, samples), true);
        assert_eq!(level.len(), 1, "{family:?}: one level batch");
        level.remove(0)
    }

    fn states(batch: &Batch) -> &StateColumn {
        batch.state_column(1).unwrap()
    }

    /// What a column of `states` sealed into views holds and is charged:
    /// each state's view padded to 8 bytes, and 4-byte offsets, one per
    /// state plus one.
    fn view_column_bytes(states: &StateColumn) -> usize {
        let kernel = MappedSketchKernel::of(states.family()).unwrap();
        (0..states.len())
            .map(|i| {
                state_view_bytes(kernel, states.state(i).as_ref())
                    .unwrap()
                    .len()
                    .div_ceil(8)
                    * 8
            })
            .sum::<usize>()
            + 4 * (states.len() + 1)
    }

    /// S1. One rule at both sites: every family with a view layout
    /// (`MappedSketchKernel::of`: DDSketch, KLL, Count-Min) seals into
    /// views, held and charged as views; every other family seals as owned
    /// sketches. Count-Min is checked at the level only (no updater builds
    /// it). Collects every family and site that breaks it.
    #[test]
    fn every_family_with_a_view_layout_seals_into_views_at_both_sites() {
        let mut broken = Vec::new();
        for (family, params) in families() {
            let window = window(&family, &params, 0, 15);
            let level = level(&family, &params, 15);
            let mut sites = vec![("merged level", states(&level))];
            if family != FieldDataType::Sketch(SketchAlgorithm::CountMinSketch) {
                sites.push(("chain window", states(&window)));
            }
            for (site, column) in sites {
                let views = matches!(column.storage(), StateStorage::Mapped(_));
                let has_view_layout = MappedSketchKernel::of(&family).is_some();
                if views != has_view_layout {
                    broken.push(format!(
                        "{family:?} {site}: sealed into views {views}, has a view layout {has_view_layout}"
                    ));
                } else if views && column.state_memory_bytes() != view_column_bytes(column) {
                    broken.push(format!(
                        "{family:?} {site}: charged {} B, its views {} B",
                        column.state_memory_bytes(),
                        view_column_bytes(column)
                    ));
                }
            }
        }
        assert!(broken.is_empty(), "{broken:#?}");
    }

    /// S2. The bytes a sealed KLL (k = 200) and Count-Min (3 x 64) column
    /// holds, from their view layouts (`asap_sketchlib` `to_view_bytes`):
    /// a KLL view is a 40-byte header (magic, version, k, m, levels,
    /// items), its level boundaries (`levels + 1` u32s padded to 8) and 8
    /// bytes per item; a Count-Min view a 32-byte header (magic, version,
    /// rows, cols, admission rate) and 8 bytes per counter. Each state
    /// adds a 4-byte offset, the column one more. Below k = 200 items a
    /// KLL holds one level and every item: 15 per chain window, 12 x 15 =
    /// 180 per merged level. (No updater builds a Count-Min state, so only
    /// its merged level is a sealing site.)
    #[test]
    fn sealed_kll_and_count_min_hold_their_view_layout_bytes() {
        let kll = (
            FieldDataType::Sketch(SketchAlgorithm::KLL),
            SketchParams::Kll { k: 200 },
        );
        let cms = (
            FieldDataType::Sketch(SketchAlgorithm::CountMinSketch),
            SketchParams::Cms {
                width: 64,
                depth: 3,
            },
        );
        let per_state = |view_bytes: usize| 50 * (view_bytes + 4) + 4;
        let kll_view = |items: usize| 40 + 8 + 8 * items;
        let cms_view = 32 + 8 * 3 * 64;
        let cases = [
            (
                "KLL chain window",
                window(&kll.0, &kll.1, 0, 15),
                per_state(kll_view(15)),
            ),
            (
                "KLL merged level",
                level(&kll.0, &kll.1, 15),
                per_state(kll_view(180)),
            ),
            (
                "Count-Min merged level",
                level(&cms.0, &cms.1, 15),
                per_state(cms_view),
            ),
        ];
        // 8,604 B, 74,604 B and 78,604 B.
        let held: Vec<_> = cases
            .iter()
            .map(|(case, batch, _)| (*case, states(batch).state_memory_bytes()))
            .collect();
        let expected: Vec<_> = cases
            .iter()
            .map(|(case, _, bytes)| (*case, *bytes))
            .collect();
        assert_eq!(held, expected);
    }

    /// S3 (guard). Sealing changes how states are held, not what they
    /// answer: a merged level's KLL quantiles and Count-Min totals, read
    /// from its sealed column, equal those of the same merge unsealed.
    #[test]
    fn sealed_kll_and_count_min_levels_answer_as_unsealed() {
        let cases = [
            (
                FieldDataType::Sketch(SketchAlgorithm::KLL),
                SketchParams::Kll { k: 200 },
                vec![
                    SketchStatistic::Quantile { q: 0.0 },
                    SketchStatistic::Quantile { q: 0.5 },
                    SketchStatistic::Quantile { q: 0.99 },
                ],
                1_100,
            ),
            (
                FieldDataType::Sketch(SketchAlgorithm::CountMinSketch),
                SketchParams::Cms {
                    width: 64,
                    depth: 3,
                },
                vec![SketchStatistic::PointCount {
                    key: planner_types::ir::scalar::ColumnRef::SampleValue,
                    value: None,
                }],
                15,
            ),
        ];
        for (family, params, statistics, samples) in cases {
            let answers = |batches: &[Batch]| {
                let mut out = Vec::new();
                for batch in batches {
                    let names = batch.to_rows();
                    let column = states(batch);
                    let indices = batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<UInt32Array>()
                        .unwrap();
                    for (row, name) in names.iter().enumerate() {
                        let state = column.state(indices.value(row) as usize);
                        let values: Vec<f64> = statistics
                            .iter()
                            .map(|statistic| state.estimate(statistic).unwrap())
                            .collect();
                        out.push((format!("{:?}", name[0]), values));
                    }
                }
                out.sort_by(|a, b| a.0.cmp(&b.0));
                out
            };
            // The same windows (a KLL window's compactions are random).
            let windows = windows(&family, &params, samples);
            let unsealed = merge(windows.clone(), false);
            let sealed = merge(windows, true);
            assert_eq!(answers(&sealed), answers(&unsealed), "{family:?}");
        }
    }

    /// S4. A merge's output states hold no room for further merges: a
    /// KLL (k = 200) merged from 12 windows of 15 samples holds its struct,
    /// its 2 level boundaries and its 180 items, 248 + 16 + 1,440 = 1,704
    /// bytes, not the doubled item buffer and 62 level slots the merge
    /// grew (2,664 bytes).
    #[test]
    fn a_merge_shrinks_the_kll_states_it_builds() {
        let family = FieldDataType::Sketch(SketchAlgorithm::KLL);
        let params = SketchParams::Kll { k: 200 };
        let merged = merge(windows(&family, &params, 15), false);
        let held: Vec<usize> = merged
            .iter()
            .flat_map(|batch| {
                let column = states(batch);
                (0..column.len()).map(|i| column.state(i).approx_memory_bytes())
            })
            .collect();
        assert_eq!(held, vec![248 + 16 + 8 * 180; SERIES as usize]);
    }
}
