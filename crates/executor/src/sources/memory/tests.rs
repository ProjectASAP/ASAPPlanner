//! Tests of a stored view's shared texts ([`StoredView`]).

use super::*;
use planner_types::ir::schema::{Field, Schema};

/// `(series, value)` rows: a text column and a number.
fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::plain("series", DataType::Utf8, false),
        Field::plain("value", DataType::Float64, false),
    ]))
}

/// One row per series in `series`, valued `value`.
fn window(series: impl IntoIterator<Item = u32>, value: f64) -> Batch {
    let rows = series
        .into_iter()
        .map(|series| {
            vec![
                crate::values::Value::Utf8(format!("{{\"instance\":\"host-{series}\"}}").into()),
                crate::values::Value::Float64(value),
            ]
        })
        .collect();
    Batch::try_new(schema(), rows).unwrap()
}

/// The rows of every stored window, read as a run reads them.
fn read_back(view: &StoredView) -> Vec<String> {
    view.rows
        .iter()
        .flat_map(|(_, batches)| batches)
        .map(|batch| format!("{:?}", view.readable(batch).unwrap().to_rows()))
        .collect()
}

const WIDTH_MS: i64 = 15_000;

// Each window reads back as appended, its texts held once for the view:
// the view's bytes are each stored batch's keys and numbers, and the
// shared texts with their table of positions, exactly.
#[test]
fn windows_read_back_as_appended_and_count_their_texts_once() {
    let mut view = StoredView::default();
    let mut appended = Vec::new();
    for w in 1..=4 {
        let batch = window(0..1_000, w as f64);
        appended.push(format!("{:?}", batch.to_rows()));
        view.append(((w - 1) * WIDTH_MS, w * WIDTH_MS), vec![batch], 60_000)
            .unwrap();
    }
    assert_eq!(read_back(&view), appended);
    let texts = &view.shared_texts_by_column[&0];
    assert_eq!(texts.len(), 1_000);
    let keys_and_numbers: usize = view
        .rows
        .iter()
        .flat_map(|(_, batches)| batches)
        .map(Batch::stored_bytes)
        .sum();
    assert_eq!(view.bytes(), keys_and_numbers + texts.memory_bytes());
    // The table of positions is counted: it is part of the texts' bytes.
    let text_bytes: usize = (0..1_000)
        .map(|series| format!("{{\"instance\":\"host-{series}\"}}").len())
        .sum();
    assert!(texts.memory_bytes() >= text_bytes + 1_000 * (4 + 4 + 12));
}

// Series that come and go: each window holds 100 series, a new hundred
// every window, and the view keeps two windows. The shared texts are
// compacted when the texts no stored row uses outnumber those it uses,
// so they stay within twice the live texts (and 64), and every stored
// window still reads back as appended.
#[test]
fn shared_texts_of_series_that_come_and_go_stay_bounded() {
    let mut view = StoredView::default();
    let mut appended = std::collections::VecDeque::new();
    for w in 1..=50u32 {
        let batch = window((w * 100)..(w * 100 + 100), f64::from(w));
        appended.push_back(format!("{:?}", batch.to_rows()));
        if appended.len() > 2 {
            appended.pop_front();
        }
        let end = i64::from(w) * WIDTH_MS;
        view.append((end - WIDTH_MS, end), vec![batch], 2 * WIDTH_MS as u64)
            .unwrap();
        assert!(
            view.shared_texts_by_column[&0].len() <= 2 * 200 + 64,
            "window {w}"
        );
        assert_eq!(read_back(&view), Vec::from(appended.clone()), "window {w}");
    }
}
