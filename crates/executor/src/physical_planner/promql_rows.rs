//! A bounded PromQL source row carries the entire label set, not just labels
//! mentioned by the query. The source adapter owns this lossless encoding.
// Some helpers here serve only the Planner's own tests (`test-support`).
#![cfg_attr(
    not(any(test, feature = "test-support")),
    allow(dead_code, unused_imports)
)]
use super::*;
use planner_types::ir::schema::DataType;
use planner_types::ir::schema::FieldDataType as SummaryFamilyType;
use std::rc::Rc;

/// Not a legal PromQL label name, so it cannot shadow a user label.
pub use planner_types::ir::schema::PROMQL_SERIES_IDENTITY as SERIES_IDENTITY_COLUMN;

/// Canonical, reversible identity. JSON object encoding preserves label names,
/// empty values and escaping; sorting makes ingestion order irrelevant.
pub fn encode_series_identity(labels: &BTreeMap<String, String>) -> Result<String, Error> {
    serde_json::to_string(labels).map_err(|error| invalid(error.to_string()))
}

pub fn decode_series_identity(encoded: &str) -> Result<BTreeMap<String, String>, Error> {
    let labels: BTreeMap<String, String> =
        serde_json::from_str(encoded).map_err(|error| invalid(error.to_string()))?;
    if encode_series_identity(&labels)? != encoded {
        return Err(invalid("series identity is not canonically encoded"));
    }
    Ok(labels)
}

/// The series identity `identity` less the labels `excluding`, encoded as
/// [`encode_series_identity`] writes it: `{"__name__":"m","instance":"1",
/// "job":"a"}` less `instance` and `__name__` is `{"job":"a"}`. The encoding
/// is canonical (names sorted, one way to escape each character), so this
/// copies the kept `"name":"value"` pairs' bytes in order, decoding and
/// encoding nothing. A name token without an escape is the name itself
/// (JSON escapes only `"`, `\` and control characters), so it is compared
/// with the excluded names byte for byte; a token with one is compared with
/// each excluded name's encoding. An error for input that is not a JSON
/// object of string labels.
pub fn series_identity_without(identity: &str, excluding: &[String]) -> Result<String, Error> {
    let bytes = identity.as_bytes();
    let malformed = || invalid("series identity is not canonically encoded");
    // The index after the JSON string token that starts at `start`.
    let string_end = |start: usize| -> Option<usize> {
        if bytes.get(start) != Some(&b'"') {
            return None;
        }
        let mut i = start + 1;
        while let Some(&byte) = bytes.get(i) {
            match byte {
                b'\\' => i += 2,
                b'"' => return Some(i + 1),
                _ => i += 1,
            }
        }
        None
    };
    // Whether the name token `token`, quotes included, names an excluded
    // label.
    let excluded = |token: &str| {
        let name = &token[1..token.len() - 1];
        if name.contains('\\') {
            excluding
                .iter()
                .any(|label| serde_json::to_string(label).is_ok_and(|encoded| encoded == token))
        } else {
            excluding.iter().any(|label| label == name)
        }
    };
    if bytes.first() != Some(&b'{') {
        return Err(malformed());
    }
    let mut out = String::with_capacity(identity.len());
    out.push('{');
    let mut i = 1;
    if bytes.get(i) == Some(&b'"') {
        loop {
            let name_end = string_end(i).ok_or_else(malformed)?;
            if bytes.get(name_end) != Some(&b':') {
                return Err(malformed());
            }
            let value_end = string_end(name_end + 1).ok_or_else(malformed)?;
            if !excluded(&identity[i..name_end]) {
                if out.len() > 1 {
                    out.push(',');
                }
                out.push_str(&identity[i..value_end]);
            }
            i = value_end;
            if bytes.get(i) != Some(&b',') {
                break;
            }
            i += 1;
        }
    }
    if bytes.get(i) != Some(&b'}') || i + 1 != bytes.len() {
        return Err(malformed());
    }
    out.push('}');
    Ok(out)
}

/// Resolve the row representation before candidate search; see
/// [`planner_types::ir::schema::series_identity::with_promql_series_identity`].
pub fn with_series_identity(root: &Rc<OperatorNode>) -> Result<Rc<OperatorNode>, Error> {
    planner_types::ir::schema::series_identity::with_promql_series_identity(root).map_err(invalid)
}

/// Construct source rows only from full identities. The named label columns
/// are projections of that same identity and cannot independently redefine it.
pub fn series_row(
    schema: &SchemaRef,
    labels: &BTreeMap<String, String>,
    timestamp: i64,
    value: f64,
) -> Result<Vec<crate::values::Value>, Error> {
    use crate::values::Value;
    let identity = encode_series_identity(labels)?;
    let mut found = false;
    let row = schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            if field.name == SERIES_IDENTITY_COLUMN {
                if field.dtype != SummaryFamilyType::Plain(DataType::Utf8)
                    || field.nullable
                    || found
                {
                    return Err(invalid("invalid series identity column"));
                }
                found = true;
                Ok(Value::Utf8(identity.clone().into()))
            } else if Some(index) == schema.time_index {
                Ok(Value::Timestamp(timestamp))
            } else if field.name == "value"
                && field.dtype == SummaryFamilyType::Plain(DataType::Float64)
            {
                Ok(Value::Float64(value))
            } else if field.dtype == SummaryFamilyType::Plain(DataType::Utf8) {
                Ok(labels.get(&field.name).map_or_else(
                    || Value::Utf8("".into()),
                    |value| Value::Utf8(value.clone().into()),
                ))
            } else {
                Err(invalid("unsupported PromQL source column"))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !found {
        return Err(invalid("source lacks its full series identity"));
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity less `excluding` by decoding, filtering and encoding
    /// again: the reference [`series_identity_without`] must equal byte for
    /// byte.
    fn decoded_without(identity: &str, excluding: &[String]) -> String {
        let mut labels = decode_series_identity(identity).unwrap();
        labels.retain(|name, _| !excluding.contains(name));
        encode_series_identity(&labels).unwrap()
    }

    /// Over label sets with escaped quotes, backslashes, control
    /// characters, unicode and empty values, excluding none, some or every
    /// label (and labels that are absent), the streaming rewrite equals the
    /// decoded one byte for byte.
    #[test]
    fn series_identity_without_equals_decode_filter_encode() {
        let texts = [
            "",
            "a",
            "job",
            "q\"uote",
            "back\\slash",
            "\\\"",
            "tab\tnew\nline\u{1}",
            "ünïcødé ☃ 𝄞",
            "\"",
            "\\",
            "comma,colon:brace}{",
            "__name__",
        ];
        let mut identities = Vec::new();
        for (i, first) in texts.iter().enumerate() {
            for (j, second) in texts.iter().enumerate() {
                let mut labels = BTreeMap::new();
                labels.insert((*first).to_string(), (*second).to_string());
                labels.insert((*second).to_string(), format!("{first}{i}"));
                labels.insert(format!("{second}{j}"), String::new());
                labels.insert("__name__".to_string(), "m".to_string());
                identities.push(encode_series_identity(&labels).unwrap());
            }
        }
        identities.push(encode_series_identity(&BTreeMap::new()).unwrap());
        let mut exclusions: Vec<Vec<String>> = vec![vec![], vec!["absent".into()]];
        exclusions.extend(texts.iter().map(|text| vec![(*text).to_string()]));
        exclusions.push(texts.iter().map(|text| (*text).to_string()).collect());
        for identity in &identities {
            let every: Vec<String> = decode_series_identity(identity)
                .unwrap()
                .into_keys()
                .collect();
            for excluding in exclusions.iter().chain([&every]) {
                assert_eq!(
                    series_identity_without(identity, excluding).unwrap(),
                    decoded_without(identity, excluding),
                    "{identity} less {excluding:?}"
                );
            }
            assert_eq!(series_identity_without(identity, &every).unwrap(), "{}");
            assert_eq!(series_identity_without(identity, &[]).unwrap(), *identity);
        }
    }

    /// Input that is not a JSON object of string labels is an error.
    #[test]
    fn series_identity_without_refuses_a_malformed_identity() {
        for identity in [
            "",
            "{",
            "}",
            "[]",
            r#"{"a"}"#,
            r#"{"a":1}"#,
            r#"{"a":"b",}"#,
            r#"{"a":"b""c":"d"}"#,
            r#"{"a":"b"} "#,
            r#"{"a":"b"#,
            r#"{"a\":"b"}"#,
        ] {
            assert!(
                series_identity_without(identity, &["x".into()]).is_err(),
                "{identity}"
            );
        }
    }
}
