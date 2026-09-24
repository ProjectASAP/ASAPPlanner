//! SketchEnvelopeAccumulator — wraps a raw SketchEnvelope protobuf payload
//! received via OTLP ingest so it can be stored through the `Store` trait.
//!
//! The accumulator preserves the opaque proto bytes and decodes them lazily
//! (via `SketchEnvelope::decode`) only when merge or query operations need
//! the inner sketch type.

use crate::{AggregateCore, KeyByLabelValues, SerializableToSink};
use asap_sketchlib::proto::sketchlib::{sketch_envelope, SketchEnvelope};
use prost::Message;
use serde_json::Value;
use std::collections::HashMap;

use crate::AggregationType;
use crate::Statistic;

/// Accumulator that stores a serialized `SketchEnvelope` protobuf.
///
/// This is the simplest viable path for OTLP sketch ingest: the OTel Collector
/// has already computed the sketch, so the backend just stores the bytes and
/// serves them back at query time.
#[derive(Debug, Clone)]
pub struct SketchEnvelopeAccumulator {
    /// Raw protobuf-encoded `SketchEnvelope`.
    pub payload: Vec<u8>,
    /// Sketch type string cached from decoding (e.g. "CountMin", "KLL").
    pub sketch_type: String,
}

impl SketchEnvelopeAccumulator {
    /// Create from raw protobuf bytes.  Decodes the envelope once to cache
    /// the sketch type; the full payload is kept for later use.
    pub fn from_proto_bytes(
        payload: Vec<u8>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let sketch_type = match SketchEnvelope::decode(payload.as_slice()) {
            Ok(env) => match env.sketch_state {
                Some(sketch_envelope::SketchState::CountMin(_)) => "CountMin".to_string(),
                Some(sketch_envelope::SketchState::CountSketch(_)) => "CountSketch".to_string(),
                Some(sketch_envelope::SketchState::Kll(_)) => "KLL".to_string(),
                Some(sketch_envelope::SketchState::Hll(_)) => "HLL".to_string(),
                Some(sketch_envelope::SketchState::Ddsketch(_)) => "DDSketch".to_string(),
                Some(sketch_envelope::SketchState::Univmon(_)) => "UnivMon".to_string(),
                Some(sketch_envelope::SketchState::Hydra(_)) => "Hydra".to_string(),
                Some(sketch_envelope::SketchState::Coco(_)) => "CocoSketch".to_string(),
                Some(sketch_envelope::SketchState::Elastic(_)) => "Elastic".to_string(),
                None => "Unknown".to_string(),
            },
            Err(e) => {
                return Err(format!("Failed to decode SketchEnvelope: {}", e).into());
            }
        };

        Ok(Self {
            payload,
            sketch_type,
        })
    }
}

// ---------------------------------------------------------------------------
// Trait implementations
// ---------------------------------------------------------------------------

impl SerializableToSink for SketchEnvelopeAccumulator {
    fn serialize_to_json(&self) -> Value {
        serde_json::json!({
            "type": "SketchEnvelopeAccumulator",
            "sketch_type": self.sketch_type,
            "payload_bytes": self.payload.len(),
        })
    }

    fn serialize_to_bytes(&self) -> Vec<u8> {
        self.payload.clone()
    }
}

impl AggregateCore for SketchEnvelopeAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn type_name(&self) -> &'static str {
        "SketchEnvelopeAccumulator"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn merge_with(
        &self,
        other: &dyn AggregateCore,
    ) -> Result<Box<dyn AggregateCore>, Box<dyn std::error::Error + Send + Sync>> {
        if other.get_accumulator_type() != self.get_accumulator_type() {
            return Err(format!(
                "Cannot merge SketchEnvelopeAccumulator with {:?}",
                other.get_accumulator_type()
            )
            .into());
        }

        // For now, merging opaque envelopes is not supported — each window is
        // a self-contained sketch produced by the OTel Collector.  Return self
        // as-is so the store can still call merge_with without panicking.
        Ok(Box::new(self.clone()))
    }

    fn get_accumulator_type(&self) -> AggregationType {
        // Opaque wrapper — report as the generic multi-subpopulation bucket.
        // Direct dispatch is not supported; native sketch query path must
        // decode the envelope and delegate to the correct accumulator.
        AggregationType::MultipleSubpopulation
    }

    fn get_keys(&self) -> Option<Vec<KeyByLabelValues>> {
        None
    }

    fn query_statistic(
        &self,
        _statistic: Statistic,
        _key: &Option<KeyByLabelValues>,
        _query_kwargs: &HashMap<String, String>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        Err(
            "SketchEnvelopeAccumulator: query_statistic not supported; decode envelope first"
                .into(),
        )
    }
}

impl crate::MultipleSubpopulationAggregate for SketchEnvelopeAccumulator {
    fn query(
        &self,
        _statistic: Statistic,
        _key: &KeyByLabelValues,
        _query_kwargs: Option<&HashMap<String, String>>,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync>> {
        Err(
            "SketchEnvelopeAccumulator: direct query not supported; use native sketch query path"
                .into(),
        )
    }

    fn clone_boxed(&self) -> Box<dyn crate::MultipleSubpopulationAggregate> {
        Box::new(self.clone())
    }
}
