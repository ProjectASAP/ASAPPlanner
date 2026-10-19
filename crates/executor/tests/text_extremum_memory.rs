//! Text MIN/MAX holds, and is charged for, only its current value: each
//! replaced value's bytes are released.
use asap_executor::arrow::array::{DictionaryArray, Int64Array};
use asap_executor::arrow::datatypes::UInt32Type;
use asap_executor::operators::{Operator, Reduction};
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, BatchColumn};
use asap_executor::Error;
use futures::{executor::block_on, StreamExt};
use std::sync::Arc;

/// Run `measures` over `batch`, grouped by `groups`, under `max_bytes`;
/// the result's first batch.
fn aggregate(
    input: Arc<Schema>,
    batch: Batch,
    groups: Vec<usize>,
    measures: Vec<(String, Reduction)>,
    max_bytes: usize,
) -> Result<Batch, Error> {
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(input.clone(), vec![batch]).unwrap(),
    )
    .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::aggregate(input, groups, measures).unwrap(),
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
        dag.execute(&[1], context)
            .unwrap()
            .remove(0)
            .next()
            .await
            .unwrap()
            .map(|batch| (*batch).clone())
    })
}

// MAX over 20,000 ascending 200-byte texts in one group holds one text at a
// time: each replaced text's bytes are released, so it fits in 8 MiB
// (the input is about 4 MB). Charging every text it ever held would need
// another 4 MB and more.
#[test]
fn text_max_charges_only_the_current_value() {
    const ROWS: usize = 20_000;
    let field = |name: &str, dtype| Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable: false,
        table: None,
    };
    let input = Arc::new(Schema {
        fields: vec![field("k", DataType::Int64), field("t", DataType::Utf8)],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let texts = (0..ROWS).map(|i| format!("{i:0200}")).collect::<Vec<_>>();
    let text: DictionaryArray<UInt32Type> = texts.iter().map(String::as_str).collect();
    let batch = Batch::try_from_columns(
        input.clone(),
        vec![
            BatchColumn::Plain(Arc::new(Int64Array::from_iter_values(std::iter::repeat_n(
                0, ROWS,
            )))),
            BatchColumn::Plain(Arc::new(text)),
        ],
    )
    .unwrap();
    let result = aggregate(
        input,
        batch,
        vec![0],
        vec![("max".into(), Reduction::Max(1))],
        8 << 20,
    );
    let result = result.unwrap();
    assert_eq!(result.num_rows(), 1);
    assert_eq!(
        format!("{:?}", result.to_rows()[0][1]),
        format!(
            "{:?}",
            asap_executor::values::Value::Utf8(texts[ROWS - 1].as_str().into())
        )
    );
}
