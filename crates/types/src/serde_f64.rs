//! JSON-safe scalar encoding shared by logical literals and physical values.
//! Finite values remain numbers; non-finite values use explicit strings.
use serde::{de::Error, Deserialize, Deserializer, Serializer};

pub fn serialize<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.is_nan() {
        serializer.serialize_str("NaN")
    } else if *value == f64::INFINITY {
        serializer.serialize_str("+Inf")
    } else if *value == f64::NEG_INFINITY {
        serializer.serialize_str("-Inf")
    } else {
        serializer.serialize_f64(*value)
    }
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Encoded {
        Number(f64),
        Special(String),
    }
    match Encoded::deserialize(deserializer)? {
        Encoded::Number(value) => Ok(value),
        Encoded::Special(value) => match value.as_str() {
            "NaN" => Ok(f64::NAN),
            "+Inf" => Ok(f64::INFINITY),
            "-Inf" => Ok(f64::NEG_INFINITY),
            _ => Err(D::Error::custom("expected NaN, +Inf or -Inf")),
        },
    }
}
