//! Content ids of plan parts: a part's JSON, hashed with 128-bit FNV-1a, so
//! two processes that hold the same part give the same id
//! ([`ViewCoverage::definition_id`](super::physical_asap_dag::ViewCoverage),
//! [`PhysicalASAPDAG::plan_id`](super::physical_asap_dag::PhysicalASAPDAG::plan_id)).

use serde::Serialize;

/// The 128-bit FNV-1a hash, in hex, of `value`'s JSON (non-finite floats
/// serialized by `serde_f64`): stable across processes and Rust versions,
/// equal for equal serializations.
pub(crate) fn content_id(value: &impl Serialize) -> String {
    let text = serde_json::to_string(value).expect("a plan part serializes");
    fnv1a_128_hex(&text)
}

/// The 128-bit FNV-1a hash of `text`, in hex.
fn fnv1a_128_hex(text: &str) -> String {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let hash = text.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u128::from(byte)).wrapping_mul(PRIME)
    });
    format!("{hash:032x}")
}
