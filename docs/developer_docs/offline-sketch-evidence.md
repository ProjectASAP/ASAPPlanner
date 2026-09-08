# Consuming offline sketch measurements

This document is for developers integrating sketch-bench with the planner. The
Rust schema is `asap_aware_mapping::empirical_cost::EvidenceArtifact`; its JSON
schema version is `1`. Required artifact-level `benchmark_version` and
`model_version` identify the producer and cost interpretation independently of
the serialization schema. Producers export offline benchmark measurements using
this contract; benchmark tooling is delivered separately from the core provider.

The checked-in [JSON Schema](offline-sketch-evidence.schema.json) describes the
wire format. `crates/asap-aware-mapping/tests/data/offline-evidence-synthetic.json`
is an explicitly fabricated format fixture, never benchmark evidence. Runtime
validation additionally checks cross-field constraints and matching context.

Resource values share the internal `asap_types::resources::PhysicalResources<Cpu,
Bytes>` container. Its CPU payload preserves units: `ModeledCpu { cpu_ops: f64 }`
represents modeled operations, while `MeasuredCpu` contains optional measured
`build_cpu_ns`, `update_cpu_ns`, `merge_cpu_ns`, `prepare_cpu_ns`, and `read_cpu_ns`
values. `Measurement` is defined in the shared types crate;
`MeasuredResources` is `PhysicalResources<MeasuredCpu, Measurement>`. Sharing
the container does not convert modeled CPU operations into measured nanoseconds.

`ResourceMeasurements` and `ExactResourceMeasurements` retain only a public
`resources` value of that shared measured type and provide v1 wire compatibility.
Rust consumers access measured CPU through `metrics.resources.cpu` and byte
dimensions through `metrics.resources`. The JSON remains flat: there is no new
`resources` or `cpu` object inside `metrics`. These names map as follows:

| Shared internal field | Sketch v1 JSON field | Exact-reference v1 JSON field |
| --- | --- | --- |
| `cpu.build_cpu_ns` | `build_cpu_ns` | `empty_build_cpu_ns` |
| `cpu.update_cpu_ns` | `update_cpu_ns` | `update_cpu_ns` |
| `cpu.merge_cpu_ns` | `merge_cpu_ns` | `merge_cpu_ns` |
| `cpu.prepare_cpu_ns` | `prepare_cpu_ns` | `prepare_cpu_ns` |
| `cpu.read_cpu_ns` | `read_cpu_ns` | `read_cpu_ns` |
| `retained_memory_bytes` | `retained_bytes` | `retained_bytes` |
| `peak_memory_bytes` | `peak_bytes` | `peak_bytes` |
| `scan_bytes` | `scan_bytes` | `scan_bytes` |
| `serialized_bytes` | `serialized_bytes` | `serialized_bytes` |
| `disk_bytes` | `disk_bytes` | `disk_bytes` |

Existing v1 field names and null behavior are unchanged. Sketch records can
add optional `prepare_cpu_ns` and `scan_bytes`; exact references can add optional
`merge_cpu_ns`, `serialized_bytes`, `disk_bytes`, and `scan_bytes`. These new
fields are omitted when unavailable by the compatibility serializers and accept
either a `Measurement` or null on input. They use the same numeric, sample-count,
and uncertainty validation as existing measurements. Old artifacts need no
rewrite or schema-version change. Representing a resource dimension does not
by itself add a workload operation or establish its semantic applicability.

Deserialize an artifact and an `EvidenceContext`, then construct an
`EmpiricalEvidenceProvider::new(artifact, context)`. Construction validates schema
version, configuration, provenance and numeric values. `lookup(algorithm, params)`
requires identical sketch parameters, distribution descriptors and environment
descriptors. The context supplies the evaluation timestamp. An expired, future,
missing, incompatible or ambiguous measurement returns a typed error; none of
these conditions supplies a zero cost. Select an explicit context for each
distribution or machine; the provider does not interpolate between datasets.

Each measured resource is an optional `Measurement` with `value`, optional `stddev`,
`samples`, and optional `method`. CPU fields are process CPU nanoseconds per
operation; `build_cpu_ns` measures empty construction. Building an ingested
snapshot additionally requires `sample_count × update_cpu_ns`; the lifecycle
helper returns that sum only when both measurements exist. Memory and disk
fields are bytes; `scan_bytes` records bytes read by scans, not storage occupancy.
Producer methods must state what
was measured and how normalization was performed. `retained_bytes` is distinct
from `peak_bytes`, `serialized_bytes`, and `disk_bytes`. Counter payload size
does not establish allocator footprint, process RSS or on-disk storage. Absent
measurements preserve the v1 null behavior, except the newly optional fields
listed above are omitted when unavailable. `stddev: null` means uncertainty was not measured,
not that variability is zero. A zero measurement must have actual evidence.

The record retains the complete command, dataset identity, implementation
revision, environment and repetition count. Distribution parameters should
include generator configuration and seed, or trace checksum and sampling rules.
Validity intervals are supplied by the producer or deployment policy; they are
an explicit applicability assumption, not a measured property.

`EmpiricalCostModel` implements the planner's existing `CostModel` boundary.
It derives the planner's default parameter configurations for the requested
accuracy, and orders algorithms by mean measured update CPU only when all
candidates have applicable measurements. Otherwise it preserves discovery order.
It returns every candidate and keeps default sizing. Its final candidate scores
retain `DefaultCostModel`'s dimensionless structural meaning; do not label those
scores as CPU or measured savings.

Deployment cost models can own the provider and call `lookup` with their own
parameter sizing. This preserves the deployment's other cost and capability
hooks. The provider's lifecycle helper returns available build/update CPU costs
for a single independently instantiated state. It deliberately leaves retention,
retirement and read costs unknown. In particular, a point-frequency benchmark
read does not price a total-count read, even when both use CMS. A deployment must
match readout semantics and supply the missing lifecycle and raw-query evidence
before selecting and pricing a complete physical plan. Never combine these
nanosecond costs with CPU operation counts without explicit calibration.

`error` contains offline observed statistics and a query descriptor. Its metric
name defines what mean/max refer to; null max does not imply a per-key maximum
was measured. Query semantics and error metrics must agree before the evidence
is shown as applicable to a planner query. Offline frequency error does not
establish total-count or quantile error. The adapter never converts observed
errors into formal accuracy guarantees or runtime feedback, and never shrinks
parameters solely because one dataset had low observed error.

## Query-matched offline recommendations

`empirical_comparison::recommend_offline` consumes the companion
[`OfflineComparisonEvidence` JSON format](offline-comparison-evidence.schema.json).
This combines the sketch artifact with explicit query bindings and a separately
identified exact implementation measured on the same machine, OS, runtime and
input distribution. Its `disjoint_live_state_v1` timing contract requires
construction, ingestion, exact preparation and read CPU to be timed separately
with retained state alive. The original upstream consuming-wrapper timings must
not be passed as these disjoint phase measurements.

`MeasurementQueryBinding` is the producer's explicit assertion identifying the
read/error probe population. The consumer checks that binding and the error
record's readout kind/value type; it cannot recover or certify the original
probe set from an aggregate error number alone.

The supported workload is an immutable i64 point-frequency snapshot, fully
ingested before a sequence of reads from its measured all-distinct-key probe
population. Each state's input size must equal the measured distribution's sample
count. The model multiplies per-state quantities by the number of identical,
independent state instances. Reads use the measured average over this probe
population; arbitrary keys outside that population are not covered. It does not
estimate sliding windows, interleaved updates, post-merge error, persistence CPU
or retirement. Nonzero merges are explicitly unavailable until matching
post-merge error and an exact merge baseline exist.

The caller supplies an `EmpiricalAccuracyRequirement`: the exact observed error
metric, maximum accepted mean, and minimum number of offline trials. This is
separate from `AccuracyTarget`. Every candidate must match the readout descriptor,
error metric, trial count and all ordinary distribution/configuration/environment
checks. A zero observed error is neither proof of exactness nor a per-key bound.

For each compatible sketch, CPU is empty construction plus input count times
update CPU plus read count times read CPU. The exact reference also includes one
complete prepare pass. The comparison ends with retained state. These measured
components produce an estimate of the specified execution sequence, not a newly
measured end-to-end runtime. Unknown required costs reject an alternative. The
objective weights CPU nanoseconds and retained byte-seconds explicitly; unknown
memory cannot be used with a nonzero memory weight. Peak requested allocations
and per-state snapshot disk/serialized sizes remain separate optional reported
dimensions. The disk size does not imply that the modeled workload writes files.

The result retains every rejected candidate and reason, the exact reference,
selected configuration, and dimensional savings. If no sketch both meets the
offline criterion and beats the exact reference, the exact path wins. If the
exact baseline is missing, stale or incompatible, no benefit recommendation is
available.

Deployments connecting this result to formal planning supply
`formal_minimums: Some(...)`, computed by their existing sizing formulas.
Selection only admits supported algorithms and configurations that dominate
those minima. `parameters_at_least` is a conservative componentwise check;
deployment-specific layout constraints, including power-of-two widths, remain
the deployment's responsibility. `selected_sketch() == None` means preserve the
exact path. A deployment must additionally match its actual point-frequency
query; a CMS configuration match does not authorize applying these error
observations to PromQL `count_over_time` or bare total counts.
