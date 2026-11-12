use super::*;

fn col(name: &str, dtype: DataType) -> Field {
    Field::plain(name, dtype, false)
}

#[test]
fn schema_new_has_no_time_or_unique_key() {
    let s = Schema::new(vec![col("k", DataType::Utf8), col("v", DataType::Float64)]);
    assert!(s.time_index.is_none());
    assert!(!s.has_unique_key());
    assert_eq!(s.column_id("k"), Some(0));
    assert_eq!(s.column_id("v"), Some(1));
    assert_eq!(s.column_id("missing"), None);
}

#[test]
fn schema_with_time_index_populates_metadata() {
    let s = Schema::with_time_index(
        vec![
            col("ts", DataType::Timestamp),
            col("service", DataType::Utf8),
            col("value", DataType::Float64),
        ],
        0,
        vec![vec![0, 1]],
    );
    assert_eq!(s.time_index, Some(0));
    assert!(s.has_unique_key());
    assert_eq!(s.unique_keys, vec![vec![0, 1]]);
}

#[test]
fn add_unique_key_dedupes() {
    let mut s = Schema::new(vec![col("a", DataType::Utf8), col("b", DataType::Utf8)]);
    s.add_unique_key(vec![0]);
    s.add_unique_key(vec![0]);
    s.add_unique_key(vec![0, 1]);
    assert_eq!(s.unique_keys, vec![vec![0], vec![0, 1]]);
}

#[test]
fn schema_serde_roundtrip() {
    let s = Schema::with_time_index(
        vec![
            col("ts", DataType::Timestamp),
            col("value", DataType::Float64),
        ],
        0,
        vec![vec![0]],
    );
    let json = serde_json::to_string(&s).unwrap();
    let back: Schema = serde_json::from_str(&json).unwrap();
    assert_eq!(s, back);
}

#[test]
fn schema_without_fields_is_rejected() {
    assert!(serde_json::from_str::<Schema>(r#"{"closed":true}"#).is_err());
}
