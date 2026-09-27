//! Versioned physical output batches. Deployment identities and coverage remain
//! outside this payload and must be checked before decoding with the bound schema.
use crate::{
    summary_kernels::{
        datasketches_kll::DatasketchesKLLAccumulator, dd_sketch::DDSketchAccumulator,
        exact::ExactAccumulator, hll_sketch::HllSketchAccumulator,
        weighted_frequency::WeightedFrequency, SumAccumulator,
    },
    values::{Batch, Schema, Value},
    AggregateCore, Error,
};
use planner_types::post_asap::{SummaryFamilyType, SummarySchema};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBatch {
    version: u32,
    schema: SummarySchema,
    rows: Vec<Vec<Cell>>,
}

#[derive(Serialize, Deserialize)]
enum Cell {
    Plain(Value),
    Summary {
        family: SummaryFamilyType,
        codec: StateCodec,
        bytes: Vec<u8>,
    },
}

// Codec identity is distinct from algorithm identity: old integer CMS/CS bytes
// must never be interpreted as Float64 weighted state with typed item tuples.
#[derive(Serialize, Deserialize)]
enum StateCodec {
    WeightedFrequencyV1,
    ExactAccumulatorV1,
    SumAccumulatorV1,
    KllMsgpackV1,
    DdMsgpackV1,
    HllMsgpackV1,
}
fn invalid(message: impl ToString) -> Error {
    Error::Invalid(message.to_string())
}
impl StateCodec {
    fn for_state(state: &dyn AggregateCore) -> Result<Self, Error> {
        let state = state.as_any();
        if state.is::<WeightedFrequency>() {
            Ok(Self::WeightedFrequencyV1)
        } else if state.is::<ExactAccumulator>() {
            Ok(Self::ExactAccumulatorV1)
        } else if state.is::<SumAccumulator>() {
            Ok(Self::SumAccumulatorV1)
        } else if state.is::<DatasketchesKLLAccumulator>() {
            Ok(Self::KllMsgpackV1)
        } else if state.is::<DDSketchAccumulator>() {
            Ok(Self::DdMsgpackV1)
        } else if state.is::<HllSketchAccumulator>() {
            Ok(Self::HllMsgpackV1)
        } else {
            Err(invalid("physical summary has no persisted native codec"))
        }
    }
    fn decode(&self, bytes: &[u8]) -> Result<Arc<dyn AggregateCore>, Error> {
        Ok(match self {
            Self::WeightedFrequencyV1 => Arc::new(WeightedFrequency::from_bytes(bytes)?),
            Self::ExactAccumulatorV1 => {
                Arc::new(ExactAccumulator::deserialize_from_bytes(bytes).map_err(invalid)?)
            }
            Self::SumAccumulatorV1 => {
                Arc::new(SumAccumulator::deserialize_from_bytes(bytes).map_err(invalid)?)
            }
            Self::KllMsgpackV1 => {
                Arc::new(DatasketchesKLLAccumulator::from_msgpack_bytes(bytes).map_err(invalid)?)
            }
            Self::DdMsgpackV1 => {
                Arc::new(DDSketchAccumulator::from_msgpack_bytes(bytes).map_err(invalid)?)
            }
            Self::HllMsgpackV1 => {
                Arc::new(HllSketchAccumulator::from_msgpack_bytes(bytes).map_err(invalid)?)
            }
        })
    }
}

/// Encode a validated physical output, preserving Float64 and typed identities.
/// This format is independent of the logical and physical plan wire formats.
pub fn encode_batch(batch: &Batch) -> Result<Vec<u8>, Error> {
    let rows = batch
        .rows()
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| {
                    Ok(match value {
                        Value::Summary { family, state } => Cell::Summary {
                            family: family.clone(),
                            codec: StateCodec::for_state(state.as_ref())?,
                            bytes: state.serialize_to_bytes(),
                        },
                        value => Cell::Plain(value.clone()),
                    })
                })
                .collect::<Result<Vec<_>, Error>>()
        })
        .collect::<Result<Vec<_>, Error>>()?;
    rmp_serde::to_vec_named(&StoredBatch {
        version: 1,
        schema: batch.schema().as_ref().clone(),
        rows,
    })
    .map_err(invalid)
}

/// Decode only against the installed output contract. The caller supplies its
/// per-read payload limit; checking state parameters is part of Batch validation.
pub fn decode_batch(bytes: &[u8], expected: Schema, max_bytes: usize) -> Result<Batch, Error> {
    if bytes.len() > max_bytes {
        return Err(invalid("native output payload exceeds read budget"));
    }
    let stored: StoredBatch = rmp_serde::from_slice(bytes).map_err(invalid)?;
    if stored.version != 1 {
        return Err(invalid("unsupported native output format"));
    }
    if stored.schema != *expected {
        return Err(invalid(
            "native output schema differs from installed contract",
        ));
    }
    let rows = stored
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| {
                    Ok(match cell {
                        Cell::Plain(value) => value,
                        Cell::Summary {
                            family,
                            codec,
                            bytes,
                        } => Value::Summary {
                            family,
                            state: codec.decode(&bytes)?,
                        },
                    })
                })
                .collect::<Result<Vec<_>, Error>>()
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Batch::try_new(expected, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary_kernels::weighted_frequency::FrequencyAlgorithm;
    use planner_types::{
        post_asap::{SketchAlgorithm, SketchKind, SketchParams, SummaryField},
        pre_asap::DataType,
    };

    fn weighted(algorithm: SketchAlgorithm) -> Batch {
        let (native, params) = match algorithm {
            SketchAlgorithm::CmsWithHeap => (
                FrequencyAlgorithm::Cms,
                SketchParams::CmsWithHeap {
                    width: 64,
                    depth: 5,
                    heap_size: 8,
                },
            ),
            _ => (
                FrequencyAlgorithm::CountSketch,
                SketchParams::CountSketchWithHeap {
                    width: 64,
                    depth: 5,
                    heap_size: 8,
                },
            ),
        };
        let family =
            SummaryFamilyType::Sketch(SketchKind::new(algorithm, params), Default::default());
        let schema = Arc::new(SummarySchema {
            fields: vec![
                SummaryField {
                    name: "group".into(),
                    dtype: SummaryFamilyType::Plain(DataType::Utf8),
                    nullable: false,
                },
                SummaryField {
                    name: "state".into(),
                    dtype: family.clone(),
                    nullable: false,
                },
            ],
            time_index: None,
        });
        let mut state = WeightedFrequency::new(native, 64, 5, 8).unwrap();
        state
            .update(&[Value::Int64(7), Value::Utf8("service-a".into())], 0.125)
            .unwrap();
        state
            .update(
                &[Value::Utf8("7".into()), Value::Utf8("service-b".into())],
                0.25,
            )
            .unwrap();
        Batch::try_new(
            schema,
            vec![vec![
                Value::Utf8("job-a".into()),
                Value::Summary {
                    family,
                    state: Arc::new(state),
                },
            ]],
        )
        .unwrap()
    }

    // Fractional rates and distinct typed item tuples survive both heap codecs.
    #[test]
    fn weighted_outputs_roundtrip_without_integer_conversion() {
        for algorithm in [
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap,
        ] {
            let batch = weighted(algorithm);
            let bytes = encode_batch(&batch).unwrap();
            let restored = decode_batch(&bytes, batch.schema().clone(), bytes.len()).unwrap();
            let scores = |b: &Batch| {
                let Value::Summary { state, .. } = &b.rows()[0][1] else {
                    panic!()
                };
                state
                    .as_any()
                    .downcast_ref::<WeightedFrequency>()
                    .unwrap()
                    .rows(8)
                    .iter()
                    .map(|row| row.iter().map(|v| v.key().unwrap()).collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            };
            assert_eq!(scores(&batch), scores(&restored));
            assert!(decode_batch(&bytes, batch.schema().clone(), bytes.len() - 1).is_err());
        }
    }

    // A storage tag cannot send weighted physical output through an integer heap decoder.
    #[test]
    fn legacy_sketch_reader_rejects_native_batch_frames() {
        let sample = super::super::SketchSampleState {
            bytes: encode_batch(&weighted(SketchAlgorithm::CmsWithHeap)).unwrap(),
            encoding: super::super::SketchEncoding::NativeBatchV1,
        };
        let result = super::super::delta_apply::per_window_summary_states(
            &[(60_000, &sample)],
            super::super::delta_apply::DeltaSketchKind::CmsWithHeap {
                rows: 5,
                cols: 64,
                heap_size: 8,
            },
        );
        assert!(result.is_err());
    }

    // Every admitted native summary codec survives the same typed boundary.
    #[test]
    fn native_summary_families_and_nonfinite_plain_values_roundtrip() {
        use planner_types::post_asap::{ExactKind, ExactParams};
        let exact = SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        let sketch = |algorithm, params| {
            SummaryFamilyType::Sketch(SketchKind::new(algorithm, params), Default::default())
        };
        let cases: Vec<(SummaryFamilyType, Arc<dyn AggregateCore>)> = vec![
            (
                exact.clone(),
                Arc::new(ExactAccumulator::new(exact.clone(), false).unwrap()),
            ),
            (exact, Arc::new(SumAccumulator::new())),
            (
                sketch(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
                Arc::new(DatasketchesKLLAccumulator::new(200)),
            ),
            (
                sketch(
                    SketchAlgorithm::DDSketch,
                    SketchParams::DDSketch { alpha: 0.01 },
                ),
                Arc::new(DDSketchAccumulator::new(0.01)),
            ),
            (
                sketch(SketchAlgorithm::Hll, SketchParams::Hll { precision: 12 }),
                Arc::new(HllSketchAccumulator::new(
                    asap_sketchlib::HllVariant::Regular,
                    12,
                )),
            ),
        ];
        for (family, state) in cases {
            let schema = Arc::new(SummarySchema {
                fields: vec![SummaryField {
                    name: "state".into(),
                    dtype: family.clone(),
                    nullable: false,
                }],
                time_index: None,
            });
            let batch =
                Batch::try_new(schema.clone(), vec![vec![Value::Summary { family, state }]])
                    .unwrap();
            let bytes = encode_batch(&batch).unwrap();
            let restored = decode_batch(&bytes, schema, bytes.len()).unwrap();
            assert_eq!(encode_batch(&restored).unwrap(), bytes);
        }
        let schema = Arc::new(SummarySchema {
            fields: vec![SummaryField {
                name: "value".into(),
                dtype: SummaryFamilyType::Plain(DataType::Float64),
                nullable: false,
            }],
            time_index: None,
        });
        let batch = Batch::try_new(
            schema.clone(),
            vec![
                vec![Value::Float64(f64::NAN)],
                vec![Value::Float64(f64::INFINITY)],
            ],
        )
        .unwrap();
        let restored = decode_batch(&encode_batch(&batch).unwrap(), schema, usize::MAX).unwrap();
        assert!(matches!(restored.rows()[0][0], Value::Float64(v) if v.is_nan()));
        assert!(matches!(restored.rows()[1][0], Value::Float64(v) if v == f64::INFINITY));
    }

    // Recovery validates the format, bound schema and actual sketch parameters.
    #[test]
    fn corrupt_or_relabelled_output_is_rejected() {
        let batch = weighted(SketchAlgorithm::CmsWithHeap);
        let bytes = encode_batch(&batch).unwrap();
        let wrong = weighted(SketchAlgorithm::CountSketchWithHeap);
        assert!(decode_batch(&bytes, wrong.schema().clone(), usize::MAX).is_err());
        let mut stored: StoredBatch = rmp_serde::from_slice(&bytes).unwrap();
        stored.version = 2;
        assert!(decode_batch(
            &rmp_serde::to_vec_named(&stored).unwrap(),
            batch.schema().clone(),
            usize::MAX
        )
        .is_err());
        stored.version = 1;
        let Cell::Summary { bytes: payload, .. } = &mut stored.rows[0][1] else {
            panic!()
        };
        *payload = b"legacy integer heap".to_vec();
        assert!(decode_batch(
            &rmp_serde::to_vec_named(&stored).unwrap(),
            batch.schema().clone(),
            usize::MAX
        )
        .is_err());
    }
}
