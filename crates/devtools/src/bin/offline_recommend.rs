//! Evaluate an explicit offline error/resource requirement against measured data.
use asap_aware_mapping::empirical_comparison::{
    recommend_offline, OfflineComparisonEvidence, OfflineComparisonRequest,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: offline_recommend COMPARISON-EVIDENCE.json REQUEST.json".into());
    }
    let evidence: OfflineComparisonEvidence =
        serde_json::from_str(&std::fs::read_to_string(&args[0])?)?;
    let request: OfflineComparisonRequest =
        serde_json::from_str(&std::fs::read_to_string(&args[1])?)?;
    let recommendation = recommend_offline(&evidence, &request)?;
    serde_json::to_writer_pretty(
        std::io::stdout(),
        &serde_json::json!({
            "schema_version":1,
            "benchmark_version":evidence.sketch_evidence.benchmark_version,
            "model_version":evidence.sketch_evidence.model_version,
            "request":request,
            "recommendation":recommendation,
            "accuracy_scope":"observed offline mean error on the exact declared probe population; not an unseen-data or realtime guarantee"
        }),
    )?;
    Ok(())
}
