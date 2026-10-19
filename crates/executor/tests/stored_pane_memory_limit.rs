//! Stored panes are immutable states the store holds and shares: a query
//! that reads them is not charged for those states again, only for what it
//! allocates. A merge over them still charges the states it creates.
use asap_executor::arrow::array::{DictionaryArray, UInt32Array};
use asap_executor::arrow::datatypes::UInt32Type;
use asap_executor::operators::Operator;
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::schema::{
    DataType, Field, FieldDataType, Schema, SketchAlgorithm, SketchKind, SketchParams,
};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, BatchColumn};
use asap_executor::{Error, StateColumnUpdater};
use futures::{executor::block_on, StreamExt};
use std::sync::Arc;

const SERIES: u32 = 1_000;
const PANES: usize = 12;

/// One pane of `SERIES` KLL states (k = 200, about 11 kB charged each, so
/// about 11 MB per pane), keyed by series.
fn pane() -> (Arc<Schema>, Batch) {
    let family = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: 200,
                failure_probability: None,
            },
        ),
        Default::default(),
    );
    let schema = Arc::new(Schema {
        fields: vec![
            Field {
                name: "series".into(),
                dtype: FieldDataType::Plain(DataType::Utf8),
                nullable: false,
                table: None,
            },
            Field {
                name: "state".into(),
                dtype: family.clone(),
                nullable: false,
                table: None,
            },
        ],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let mut updater = StateColumnUpdater::new(&family).unwrap();
    updater.ensure_groups(SERIES as usize).unwrap();
    for series in 0..SERIES {
        updater.update(series, f64::from(series), 0).unwrap();
    }
    let names = (0..SERIES)
        .map(|series| format!("s{series}"))
        .collect::<Vec<_>>();
    let series: DictionaryArray<UInt32Type> = names.iter().map(String::as_str).collect();
    let batch = Batch::try_from_columns(
        schema.clone(),
        vec![
            BatchColumn::Plain(Arc::new(series)),
            BatchColumn::States {
                indices: UInt32Array::from_iter_values(0..SERIES),
                states: Arc::new(updater.seal()),
            },
        ],
    )
    .unwrap();
    (schema, batch)
}

/// Merges `PANES` copies of one stored pane per series under `max_bytes`.
fn merge_stored_panes(max_bytes: usize) -> Result<usize, Error> {
    let (schema, pane) = pane();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema.clone(), vec![pane; PANES]).unwrap(),
    )
    .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::summary_merge(schema, 1, vec![0]).unwrap(),
    )
    .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits {
            max_bytes,
            ..Limits::default()
        },
    )
    .unwrap();
    block_on(async {
        let mut output = dag.execute(&[1], context).unwrap().remove(0);
        let mut rows = 0;
        while let Some(batch) = output.next().await {
            rows += batch?.num_rows();
        }
        Ok(rows)
    })
}

// Twelve stored panes hold about 130 MB of states, which charging them per
// read would put far past a 32 MiB limit. The merge allocates one state per
// series (about 11 MB), so it fits.
#[test]
fn reading_stored_panes_does_not_charge_their_states() {
    assert_eq!(merge_stored_panes(32 << 20).unwrap(), SERIES as usize);
}

// The merged states themselves (about 11 MB) do not fit in 4 MiB.
#[test]
fn a_merge_that_allocates_past_the_limit_still_fails() {
    assert!(matches!(
        merge_stored_panes(4 << 20),
        Err(Error::MemoryLimit)
    ));
}
