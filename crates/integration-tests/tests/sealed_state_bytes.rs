//! The price of a sealed summary state (`sketch_state_bytes`) against the
//! bytes the executor holds for it: a chain's own sealed window
//! (`StateColumnUpdater::seal`) and a chain level merged from narrower
//! windows (a `SummaryMerge` with a sealed output). A state sealed into its
//! view layout is held as its view padded to 8 bytes and a 4-byte offset in
//! its column (plus one per column); the offset is a byte of the state's row
//! beside the state, so the price compared is the padded view.
use asap_cost::node::state_bytes::{sketch_state_bytes, StateInputRows};
use asap_executor::arrow::array::{DictionaryArray, UInt32Array};
use asap_executor::arrow::datatypes::UInt32Type;
use asap_executor::operators::Operator;
use asap_executor::plan::PhysicalDAG;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, BatchColumn, StateColumn};
use asap_executor::StateColumnUpdater;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::stages::Lifetime;
use asap_types::summary_type::{SketchAlgorithm, SketchParams};
use futures::{executor::block_on, StreamExt};
use std::sync::Arc;

const SERIES: u32 = 50;
const WINDOWS: u32 = 12;

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

/// Window `window` of a chain: one sealed state of `family` per series over
/// `samples` distinct values each.
fn window(family: &FieldDataType, params: &SketchParams, window: u32, samples: u32) -> Batch {
    if let SketchParams::Cms { width, depth } = params {
        return count_min_window(*width, *depth, window, samples);
    }
    let mut updater = StateColumnUpdater::new(family, Some(params)).unwrap();
    updater.ensure_groups(SERIES as usize).unwrap();
    for s in 0..SERIES {
        for sample in 0..samples {
            let n = s * 7 + window * samples + sample;
            let t = i64::from(window * samples + sample) * 1_000;
            updater.update(s, f64::from(n) * 0.5 + 1.0, t).unwrap();
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

/// A window of Count-Min states, one per series, counting `samples` keys
/// each, built as objects: no updater builds a Count-Min state (they arrive
/// stored), so only a level merged from such windows is sealed.
fn count_min_window(width: u32, depth: u32, window: u32, samples: u32) -> Batch {
    use asap_executor::summary_kernels::CountMinSketchAccumulator;
    use asap_executor::values::Value;
    let family = FieldDataType::Sketch(SketchAlgorithm::CountMinSketch);
    let rows = (0..SERIES)
        .map(|s| {
            let mut state = CountMinSketchAccumulator::new(depth as usize, width as usize).unwrap();
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

/// The chain level merged from [`WINDOWS`] windows of `samples` each.
fn level(family: &FieldDataType, params: &SketchParams, samples: u32) -> Batch {
    let windows: Vec<Batch> = (0..WINDOWS)
        .map(|w| window(family, params, w, samples))
        .collect();
    let schema = windows[0].schema().clone();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema.clone(), windows).unwrap(),
        Lifetime::None,
    )
    .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::summary_merge(schema, 1, vec![0])
            .unwrap()
            .with_sealed_output(),
        Lifetime::None,
    )
    .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits { max_bytes: 1 << 30 },
    )
    .unwrap();
    let mut out = block_on(async {
        let mut output = dag.execute(&[1], context).unwrap().remove(0);
        let mut out = Vec::new();
        while let Some(batch) = output.next().await {
            out.push(Batch::clone(&batch.unwrap()));
        }
        out
    });
    assert_eq!(out.len(), 1, "{family:?}: one level batch");
    out.remove(0)
}

/// The bytes one state of `states` is held at beside its offset: the
/// column's charge less one offset per state and one for the column, over
/// its states. Equals the mean padded view when the column holds views.
fn held_per_state(states: &StateColumn) -> f64 {
    let offsets = 4 * (states.len() + 1);
    (states.state_memory_bytes() as f64 - offsets as f64) / states.len() as f64
}

fn rows(rows: u32) -> StateInputRows {
    StateInputRows {
        folded_rows_per_state: Some(f64::from(rows)),
        non_negative: true,
        value_magnitude_range: None,
        distinct_values: None,
    }
}

/// P1. A sealed KLL's and Count-Min's price is the view the executor holds
/// it in, exactly, while every item is retained (KLL up to k rows):
/// KLL (k = 200) 40 + 8 + 8 · n bytes over n ≤ 200 rows, Count-Min (3 x 64)
/// 32 + 8 · 192 = 1,568 bytes; for a chain's own window and for a merged
/// level (12 windows of 15 rows, priced at their 180; Count-Min at its
/// level only, no updater building it). Past k a KLL holds the items its
/// compactions retain, which depend on its coin tosses and, for a level,
/// on the merge order: the price bounds the column's mean state (what the
/// column is charged, over its states) from above, within 1.25 times it.
/// Collects every case that breaks it.
#[test]
fn a_sealed_states_price_is_the_view_the_executor_holds() {
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
    let wide_cms = (
        FieldDataType::Sketch(SketchAlgorithm::CountMinSketch),
        SketchParams::Cms {
            width: 2_048,
            depth: 5,
        },
    );
    // (case, batch, the price's input rows per state, exact).
    let cases = [
        (
            "KLL window, 1 row",
            window(&kll.0, &kll.1, 0, 1),
            &kll.1,
            1,
            true,
        ),
        (
            "KLL window, 15 rows",
            window(&kll.0, &kll.1, 0, 15),
            &kll.1,
            15,
            true,
        ),
        (
            "KLL window, 200 rows",
            window(&kll.0, &kll.1, 0, 200),
            &kll.1,
            200,
            true,
        ),
        (
            "KLL level, 12 x 15 rows",
            level(&kll.0, &kll.1, 15),
            &kll.1,
            180,
            true,
        ),
        (
            "KLL window, 1,100 rows",
            window(&kll.0, &kll.1, 0, 1_100),
            &kll.1,
            1_100,
            false,
        ),
        (
            "KLL level, 12 x 1,100 rows",
            level(&kll.0, &kll.1, 1_100),
            &kll.1,
            13_200,
            false,
        ),
        (
            "Count-Min 3 x 64 level",
            level(&cms.0, &cms.1, 15),
            &cms.1,
            180,
            true,
        ),
        (
            "Count-Min 5 x 2,048 level",
            level(&wide_cms.0, &wide_cms.1, 15),
            &wide_cms.1,
            180,
            true,
        ),
    ];
    let mut broken = Vec::new();
    for (case, batch, params, input_rows, exact) in &cases {
        let states = batch.state_column(1).unwrap();
        let held = held_per_state(states);
        let price = sketch_state_bytes(params, rows(*input_rows)) as f64;
        let fits = match exact {
            true => held == price,
            false => held <= price && price <= 1.25 * held,
        };
        if !fits {
            broken.push(format!("{case}: priced {price} B, held {held} B per state"));
        }
    }
    assert!(broken.is_empty(), "{broken:#?}");
}

/// P2 (guard). A sealed DDSketch's price bounds what its column holds
/// per state from above (`ddsketch_state_bytes`; its bands are pinned in
/// `state_bytes::tests`).
#[test]
fn a_sealed_ddsketchs_price_bounds_its_view() {
    let family = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
    let params = SketchParams::DDSketch { alpha: 0.01 };
    for (batch, input_rows) in [
        (window(&family, &params, 0, 15), 15),
        (level(&family, &params, 15), 180),
    ] {
        let states = batch.state_column(1).unwrap();
        let price = sketch_state_bytes(&params, rows(input_rows));
        let held = held_per_state(states);
        assert!(
            held <= price as f64,
            "{input_rows} rows: held {held} B per state, price {price} B"
        );
    }
}
