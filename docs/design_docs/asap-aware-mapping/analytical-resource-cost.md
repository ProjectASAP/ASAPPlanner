# Analytical resource cost model

Issue #323 replaces structural node counts with an explicit estimate of three
physical resource dimensions:

- CPU work (`operations`)
- peak retained state (`bytes`)
- input scan I/O (`bytes`)

The dimensions remain visible in exported provenance. A deployment supplies
non-negative calibration coefficients to convert them to one comparable
planner objective:

```text
cost = cpu_ops * cost_per_cpu_op
     + scan_bytes * cost_per_scan_byte
     + peak_memory_bytes * cost_per_retained_byte
```

The calibration has a required version string. These coefficients are a
policy boundary, not benchmark constants baked into the planner.

## Workload scope

`AnalyticalInputs` requires input rows, input bytes, group count, and the
number of evaluations in the comparison scope. Zero or missing inputs are an
error. Exact pre-ASAP aggregation processes and scans the input once per
evaluation, retaining 16 bytes per group:

```text
cpu_ops    = input_rows * evaluation_count
scan_bytes = input_bytes * evaluation_count
memory     = group_count * 16
```

A post-ASAP sketch is built once and read for every evaluation:

```text
cpu_ops    = input_rows * update_ops(params)
             + evaluation_count * read_ops(params)
scan_bytes = input_bytes
memory     = group_count * state_bytes(params)
```

CMS and CountSketch use depth updates/readout and `width * depth * 8` bytes.
Their heap variants add logarithmic update work and `heap_size * 16` bytes.
KLL, HLL, KMV, and Theta derive work and state directly from their sized
parameters. The formulas use the concrete parameters already selected to
satisfy the query's accuracy target.

DDSketch is deliberately unavailable until a value-distribution/range input
can determine occupied bins. The model never invents a bin count. Arithmetic
overflow, parameter mismatches, invalid calibration, unsupported plans, and
missing workload statistics also fail closed.

## Planning and export

`AnalyticalCostModel` ranks legal sketch alternatives by calibrated cost and
supplies the same estimates to plan costing. It does not alter accuracy or
lifecycle legality. `dag_export --analytical-cost-json '<model>'` uses that
model for selection and exports baseline, selected, and benefit annotations,
including every input, resource estimate, coefficient, and model version.

If no analytical evidence is supplied, costs are `Unavailable`. Structural
node counts are not used as a fallback because they have no physical unit and
can reverse the result of a CPU/memory/scan comparison.

