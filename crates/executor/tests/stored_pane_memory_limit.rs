//! Stored panes are immutable states the store holds and shares: a query
//! that reads them is not charged for those states again, only for what it
//! allocates. A merge over them still charges the states it creates.
use asap_executor::arrow::array::{DictionaryArray, UInt32Array};
use asap_executor::arrow::datatypes::UInt32Type;
use asap_executor::operators::{Operator, SortKey};
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
/// Samples per state: past 1,024 a k = 200 KLL holds its whole item buffer.
const SAMPLES: u32 = 1_100;

/// One pane of `SERIES` KLL states (k = 200, about 11 kB charged each, so
/// about 11 MB per pane), keyed by series.
fn pane() -> (Arc<Schema>, Batch) {
    pane_of(SERIES)
}

/// One pane of `series` KLL states (k = 200), keyed by series.
fn pane_of(series_count: u32) -> (Arc<Schema>, Batch) {
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
    updater.ensure_groups(series_count as usize).unwrap();
    for series in 0..series_count {
        for sample in 0..SAMPLES {
            updater
                .update(series, f64::from(series) + f64::from(sample), 0)
                .unwrap();
        }
    }
    let names = (0..series_count)
        .map(|series| format!("s{series}"))
        .collect::<Vec<_>>();
    let series: DictionaryArray<UInt32Type> = names.iter().map(String::as_str).collect();
    let batch = Batch::try_from_columns(
        schema.clone(),
        vec![
            BatchColumn::Plain(Arc::new(series)),
            BatchColumn::States {
                indices: UInt32Array::from_iter_values(0..series_count),
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

// A Limit keeping 49% of a stored pane of 2,000 KLLs (about 21 MB of
// states) compacts its state column, and the compacted column shares the
// stored sketches by pointer: under an 8 MiB limit it is charged for the
// pointers, not the sketches. Keeping 51% (no compaction) fits too.
#[test]
fn compacting_a_stored_pane_does_not_charge_its_shared_sketches() {
    for kept in [980, 1_020] {
        let (schema, pane) = pane_of(2_000);
        let mut dag = PhysicalDAG::default();
        dag.add(
            0,
            vec![],
            Operator::source(schema.clone(), vec![pane]).unwrap(),
        )
        .unwrap();
        dag.add(
            1,
            vec![0],
            Operator::limit(schema, kept, 0, vec![]).unwrap(),
        )
        .unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
            },
            Limits {
                max_bytes: 8 << 20,
                ..Limits::default()
            },
        )
        .unwrap();
        let rows = block_on(async {
            let mut output = dag.execute(&[1], context).unwrap().remove(0);
            let mut rows = 0;
            while let Some(batch) = output.next().await {
                rows += batch?.num_rows();
            }
            Ok::<_, Error>(rows)
        });
        assert_eq!(rows.unwrap(), kept as usize, "keeping {kept} rows");
    }
}

// topk over several panes: a Sort concatenates two stored panes of 1,000
// KLLs (about 21 MB of states), then a Limit keeps 49% and compacts the
// concatenated column. Both derivations share the stored sketches by
// pointer, so the run fits in 8 MiB.
#[test]
fn sorting_then_limiting_stored_panes_does_not_charge_their_sketches() {
    let (schema, first) = pane_of(SERIES);
    let (_, second) = pane_of(SERIES);
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema.clone(), vec![first, second]).unwrap(),
    )
    .unwrap();
    let key = SortKey {
        column: 0,
        descending: false,
        nulls_first: false,
    };
    dag.add(
        1,
        vec![0],
        Operator::sort(schema.clone(), vec![key], vec![]).unwrap(),
    )
    .unwrap();
    dag.add(2, vec![1], Operator::limit(schema, 980, 0, vec![]).unwrap())
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits {
            max_bytes: 8 << 20,
            ..Limits::default()
        },
    )
    .unwrap();
    let rows = block_on(async {
        let mut output = dag.execute(&[2], context).unwrap().remove(0);
        let mut rows = 0;
        while let Some(batch) = output.next().await {
            rows += batch?.num_rows();
        }
        Ok::<_, Error>(rows)
    });
    assert_eq!(rows.unwrap(), 980);
}

// A merge over one stored pane of 2,000 KLLs (about 21 MB of states), one
// state per series: every state passes through unchanged, shared by
// pointer, so the merge fits in 8 MiB.
#[test]
fn a_merge_passing_stored_states_through_does_not_charge_them() {
    let (schema, pane) = pane_of(2_000);
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(schema.clone(), vec![pane]).unwrap(),
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
        },
        Limits {
            max_bytes: 8 << 20,
            ..Limits::default()
        },
    )
    .unwrap();
    let rows = block_on(async {
        let mut output = dag.execute(&[1], context).unwrap().remove(0);
        let mut rows = 0;
        while let Some(batch) = output.next().await {
            rows += batch?.num_rows();
        }
        Ok::<_, Error>(rows)
    });
    assert_eq!(rows.unwrap(), 2_000);
}

/// A deployment's own view provider (not `MemorySource`): returns `PANES`
/// copies of one stored pane on every scan.
struct StoredPanes {
    schema: Arc<Schema>,
    pane: Batch,
}
impl asap_executor::sources::TableProvider for StoredPanes {
    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
    fn boundedness(&self) -> asap_executor::plan::Boundedness {
        asap_executor::plan::Boundedness::Bounded
    }
    fn scan(
        &self,
        _: &asap_executor::sources::ScanRequest,
        _: RunContext,
    ) -> Result<asap_executor::runtime::OutputStream<'_, Batch>, Error> {
        Ok(futures::stream::iter(vec![Ok(self.pane.clone()); PANES]).boxed_local())
    }
}

// A materialized view slot's states are the store's whatever provider returns them: the
// merge over twelve stored panes from a deployment's own provider fits the
// 32 MiB limit, as it does over `Operator::source`.
#[test]
fn a_materialized_view_providers_states_are_not_charged_to_the_run() {
    use asap_executor::physical_planner::CompiledPhysicalDAG;
    use asap_executor::sources::{BindingDataSources, TableProvider};
    use std::collections::BTreeMap;
    let (schema, pane) = pane();
    let merge = Operator::summary_merge(schema.clone(), 1, vec![0]).unwrap();
    let compiled = CompiledPhysicalDAG::from_operators(
        BTreeMap::from([(0, schema.clone())]),
        BTreeMap::from([(1, (vec![0], merge))]),
        vec![1],
    )
    .unwrap();
    let data_sources = BindingDataSources {
        materialized_views: BTreeMap::from([(
            0,
            Arc::new(StoredPanes { schema, pane }) as Arc<dyn TableProvider>,
        )]),
        ..Default::default()
    };
    let dag = compiled.instantiate(&data_sources).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits {
            max_bytes: 32 << 20,
            ..Limits::default()
        },
    )
    .unwrap();
    let rows = block_on(async {
        let mut output = dag.execute(&[1], context).unwrap().remove(0);
        let mut rows = 0;
        while let Some(batch) = output.next().await {
            rows += batch.unwrap().num_rows();
        }
        rows
    });
    assert_eq!(rows, SERIES as usize);
}
