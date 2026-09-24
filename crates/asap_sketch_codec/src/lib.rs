//! Runtime-independent decoding of the sketchlib protobuf envelope.

use asap_sketchlib::proto::sketchlib::{
    sketch_envelope::SketchState, DdSketchState, KllState, SketchEnvelope,
};
use asap_sketchlib::DdSketch;
use prost::Message;

pub fn envelope_state(bytes: &[u8]) -> Result<Option<SketchState>, String> {
    SketchEnvelope::decode(bytes)
        .map(|envelope| envelope.sketch_state)
        .map_err(|error| format!("decode SketchEnvelope: {error}"))
}

pub fn ddsketch_state(bytes: &[u8]) -> Result<(DdSketchState, f64), String> {
    let envelope =
        SketchEnvelope::decode(bytes).map_err(|error| format!("decode SketchEnvelope: {error}"))?;
    match envelope.sketch_state {
        Some(SketchState::Ddsketch(state)) => Ok((state, envelope.sample_p)),
        _ => Err("SketchEnvelope contains no DDSketch state".into()),
    }
}

pub fn reconstruct_ddsketch(bytes: &[u8]) -> Result<(DdSketch, f64), String> {
    let (state, sample_p) = ddsketch_state(bytes)?;
    if !state.alpha.is_finite() || !(0.0..1.0).contains(&state.alpha) || state.alpha == 0.0 {
        return Err("DDSketch alpha must be finite and between zero and one".into());
    }
    Ok((
        DdSketch::from_raw(state.alpha, state.store_counts, state.store_offset),
        sample_p,
    ))
}

pub fn kll_state(bytes: &[u8]) -> Result<KllState, String> {
    let envelope =
        SketchEnvelope::decode(bytes).map_err(|error| format!("decode SketchEnvelope: {error}"))?;
    match envelope.sketch_state {
        Some(SketchState::Kll(state)) => Ok(state),
        _ => Err("SketchEnvelope contains no KLL state".into()),
    }
}

pub fn encode_ddsketch(sketch: &DdSketch) -> Vec<u8> {
    let envelope = SketchEnvelope {
        format_version: 1,
        producer: None,
        hash_spec: None,
        sample_p: 0.0,
        sketch_state: Some(SketchState::Ddsketch(sketch.to_proto())),
    };
    envelope.encode_to_vec()
}

pub fn encode_kll(sketch: &asap_sketchlib::sketches::kll::KLL<f64>) -> Vec<u8> {
    use asap_sketchlib::proto::sketchlib::CoinState;
    let (state, bit_cache, remaining_bits) = sketch.wire_coin();
    SketchEnvelope {
        format_version: 1,
        producer: None,
        hash_spec: None,
        sample_p: 0.0,
        sketch_state: Some(SketchState::Kll(KllState {
            k: sketch.wire_k(),
            m: sketch.wire_m(),
            num_levels: sketch.wire_num_levels(),
            levels: sketch.wire_levels(),
            items: sketch.wire_items(),
            coin: Some(CoinState {
                state,
                bit_cache,
                remaining_bits,
            }),
            offset: 0.0,
            value_scale: 0,
            residuals: Vec::new(),
        })),
    }
    .encode_to_vec()
}
