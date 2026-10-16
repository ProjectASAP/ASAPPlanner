// cargo run -p asap-devtools --bin convert_rqe_atomic_costs -- \
//     optimizer_cost/rqe_atomic_costs.json measured_operation_costs.json
//
// Reads sketch-bench's per-operation cost table (`rqe_atomic_costs.json`)
// and writes plan selection's measured table: a JSON array of
// `{"sketch": <SketchParams>, "operation": "update|merge|estimate",
// "cpu_ns_per_operation": <ns>}`, three entries per converted row
// (`asap_devtools::rqe_atomic_costs`). Pass it to `stage_pipeline
// --measured-operation-costs`, or set it as
// `PlanSelectionSettings::measured_operation_costs`. Skipped rows (exact
// accumulators, Hydra structures, unknown names) are listed on stderr.

use asap_devtools::rqe_atomic_costs::convert_rqe_atomic_costs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output), None) = (args.next(), args.next(), args.next()) else {
        return Err("usage: convert_rqe_atomic_costs <rqe_atomic_costs.json> <out.json>".into());
    };
    let rows: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&input)?)?;
    let converted = convert_rqe_atomic_costs(&rows)?;
    for skipped in &converted.skipped_rows {
        eprintln!("skipped {}: {}", skipped.sketch_name, skipped.reason);
    }
    std::fs::write(
        &output,
        serde_json::to_string_pretty(&converted.measured_operation_costs)?,
    )?;
    eprintln!(
        "wrote {} entries to {output}",
        converted.measured_operation_costs.len()
    );
    Ok(())
}
