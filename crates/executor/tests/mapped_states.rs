//! States read where a deployment stores them (`StateColumn::from_mapped`):
//! a merge folds mapped states into its accumulators and answers as it
//! does over owned states, out-of-order counter windows included.
use asap_executor::arrow::array::{DictionaryArray, UInt32Array};
use asap_executor::arrow::buffer::{Buffer, ScalarBuffer};
use asap_executor::arrow::datatypes::UInt32Type;
use asap_executor::operators::Operator;
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_executor::planner::summary_type::{
    ExactKind, ExactParams, SketchAlgorithm, SketchParams, SketchStatistic,
};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{state_view_bytes, Batch, BatchColumn, StateColumn};
use asap_executor::{Error, StateColumnUpdater};
use futures::{executor::block_on, StreamExt};
use planner_types::stages::Lifetime;
use std::sync::Arc;

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

/// Window `window` of `series` states of `family`, `samples` samples each
/// (timestamps in the window, so counter windows are disjoint in time).
fn window(
    family: &FieldDataType,
    params: Option<&SketchParams>,
    window: u32,
    series: u32,
    samples: u32,
) -> Batch {
    let mut updater = StateColumnUpdater::new(family, params).unwrap();
    updater.ensure_groups(series as usize).unwrap();
    for s in 0..series {
        for sample in 0..samples {
            let t = i64::from(window * samples + sample) * 1_000;
            let value = f64::from(s * 7 + window * samples + sample) * 0.5 + 1.0;
            updater.update(s, value, t).unwrap();
        }
    }
    let names: Vec<String> = (0..series).map(|s| format!("s{s}")).collect();
    let names: DictionaryArray<UInt32Type> = names.iter().map(String::as_str).collect();
    Batch::try_from_columns(
        schema(family),
        vec![
            BatchColumn::Plain(Arc::new(names)),
            BatchColumn::States {
                indices: UInt32Array::from_iter_values(0..series),
                states: Arc::new(updater.seal()),
            },
        ],
    )
    .unwrap()
}

/// `batch` with its states written in their mapped layout and read back in
/// place: the bytes a deployment would map.
fn mapped(batch: &Batch) -> Batch {
    let states = batch.state_column(1).unwrap();
    let mut bytes = Vec::<u8>::new();
    let mut offsets = vec![0u32];
    for index in 0..states.len() {
        bytes.extend(state_view_bytes(states.state(index).as_ref()).expect("a mapped form"));
        bytes.resize(bytes.len().div_ceil(8) * 8, 0);
        offsets.push(bytes.len() as u32);
    }
    let mut words = vec![0u64; bytes.len() / 8];
    for (word, chunk) in words.iter_mut().zip(bytes.chunks(8)) {
        *word = u64::from_le_bytes(chunk.try_into().unwrap());
    }
    let len = bytes.len();
    let column = StateColumn::from_mapped(
        batch.schema().fields[1].dtype.clone(),
        Buffer::from(words).slice_with_length(0, len),
        ScalarBuffer::from(offsets),
    )
    .unwrap();
    assert!(column.mapped_state_address(0).is_some());
    Batch::try_from_columns(
        batch.schema().clone(),
        vec![
            BatchColumn::Plain(batch.column(0).clone()),
            BatchColumn::States {
                indices: batch
                    .column(1)
                    .as_any()
                    .downcast_ref::<UInt32Array>()
                    .unwrap()
                    .clone(),
                states: Arc::new(column),
            },
        ],
    )
    .unwrap()
}

/// `SummaryMerge` by series over `batches`, under `max_bytes`: its output
/// batches.
fn merge(batches: Vec<Batch>, max_bytes: usize) -> Result<Vec<Batch>, Error> {
    merge_with_peak(batches, max_bytes).map(|(out, _)| out)
}

/// [`merge`], and the most bytes the run held at once.
fn merge_with_peak(batches: Vec<Batch>, max_bytes: usize) -> Result<(Vec<Batch>, usize), Error> {
    let schema = batches[0].schema().clone();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema.clone(), batches).unwrap(),
        Lifetime::None,
    )
    .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::summary_merge(schema, 1, vec![0]).unwrap(),
        Lifetime::None,
    )
    .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits { max_bytes },
    )
    .unwrap();
    block_on(async {
        let mut output = dag.execute(&[1], context.clone()).unwrap().remove(0);
        let mut out = Vec::new();
        while let Some(batch) = output.next().await {
            let batch = batch?;
            out.push(Batch::clone(&batch));
        }
        Ok((out, context.peak_bytes()))
    })
}

/// Each output series' state, as its mapped bytes, or (`readout`) as
/// quantiles.
fn by_series(batches: &[Batch], readout: bool) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for batch in batches {
        let names = batch.to_rows();
        let states = batch.state_column(1).unwrap();
        let indices = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        for (row, name) in names.iter().enumerate() {
            let state = states.state(indices.value(row) as usize);
            let bytes = if readout {
                [0.0, 0.1, 0.5, 0.9, 0.99, 1.0]
                    .iter()
                    .flat_map(|&q| {
                        state
                            .estimate(&SketchStatistic::Quantile { q })
                            .unwrap()
                            .to_le_bytes()
                    })
                    .collect()
            } else {
                state_view_bytes(state.as_ref()).unwrap()
            };
            out.push((format!("{:?}", name[0]), bytes));
        }
    }
    out.sort();
    out
}

const LIMIT: usize = 1 << 30;

/// V2/V21: every mapped family merges to the same answer as owned states:
/// DDSketch, Count-Min and exact records by their bytes, KLL by its
/// quantiles (12 windows of 15 samples stay below KLL's first compaction, so
/// they are exact too); over mapped windows, owned windows, and a mix.
#[test]
fn mapped_states_merge_as_owned_states() {
    let cases: Vec<(FieldDataType, Option<SketchParams>, bool)> = vec![
        (
            FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            Some(SketchParams::DDSketch { alpha: 0.01 }),
            false,
        ),
        (
            FieldDataType::Sketch(SketchAlgorithm::Kll),
            Some(SketchParams::Kll { k: 200 }),
            true,
        ),
        (
            FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            None,
            false,
        ),
        (
            FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count),
            None,
            false,
        ),
        (
            FieldDataType::ExactAggregate(ExactKind::Min, ExactParams::Min),
            None,
            false,
        ),
        (
            FieldDataType::ExactAggregate(ExactKind::Max, ExactParams::Max),
            None,
            false,
        ),
        (
            FieldDataType::ExactAggregate(ExactKind::Counter, ExactParams::Counter),
            None,
            false,
        ),
    ];
    for (family, params, readout) in cases {
        let owned: Vec<Batch> = (0..12)
            .map(|w| window(&family, params.as_ref(), w, 50, 15))
            .collect();
        let maps: Vec<Batch> = owned.iter().map(mapped).collect();
        let mixed: Vec<Batch> = owned
            .iter()
            .zip(&maps)
            .enumerate()
            .map(|(w, (o, m))| if w % 2 == 0 { m.clone() } else { o.clone() })
            .collect();
        let expected = by_series(&merge(owned, LIMIT).unwrap(), readout);
        assert_eq!(expected.len(), 50, "{family:?}");
        assert_eq!(
            by_series(&merge(maps, LIMIT).unwrap(), readout),
            expected,
            "{family:?} mapped"
        );
        assert_eq!(
            by_series(&merge(mixed, LIMIT).unwrap(), readout),
            expected,
            "{family:?} mixed"
        );
    }
}

/// V21: counter windows are joined in time order even when the mapped
/// windows arrive out of order.
#[test]
fn mapped_counters_merge_in_time_order() {
    let family = FieldDataType::ExactAggregate(ExactKind::Counter, ExactParams::Counter);
    let owned: Vec<Batch> = (0..6).map(|w| window(&family, None, w, 20, 10)).collect();
    let expected = by_series(&merge(owned.clone(), LIMIT).unwrap(), false);
    let reversed: Vec<Batch> = owned.iter().rev().map(mapped).collect();
    assert_eq!(by_series(&merge(reversed, LIMIT).unwrap(), false), expected);
}

/// Malformed mapped bytes are refused when the column is built.
#[test]
fn malformed_mapped_states_are_refused() {
    let family = FieldDataType::Sketch(SketchAlgorithm::Kll);
    let words = vec![u64::MAX; 8];
    assert!(StateColumn::from_mapped(
        family.clone(),
        Buffer::from(words.clone()),
        ScalarBuffer::from(vec![0u32, 64])
    )
    .is_err());
    // Offsets past the bytes.
    assert!(StateColumn::from_mapped(
        family,
        Buffer::from(words),
        ScalarBuffer::from(vec![0u32, 128])
    )
    .is_err());
    // A family with no mapped form.
    assert!(StateColumn::from_mapped(
        FieldDataType::Sketch(SketchAlgorithm::UnivMon),
        Buffer::from(vec![0u64; 1]),
        ScalarBuffer::from(vec![0u32, 8])
    )
    .is_err());
}
