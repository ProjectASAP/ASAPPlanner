# Storage operation estimates

The physical-plan ranking adapter accepts an optional `StorageIoProfile` in
its immutable `PhysicalEvidenceSnapshot`. `dag_export --planner-cost-json`
accepts the same profile in the document's top-level `storage_io` field.
Omitting the profile preserves the existing CPU/memory/scan-byte objective;
operation counts are unestimated, not inferred to be zero.

A supplied profile must cover every reachable physical node, with an explicit
empty `accesses` list for nodes doing no storage I/O. Entries bind the complete
`PhysicalDagNode` and `OperatorStatistics`, so a reused ID cannot silently
borrow evidence from a different plan. Profiles may contain additional nodes
for other alternatives. Their evidence generation must equal the planner
snapshot version, and `observed_at_ms <= planning_time < valid_until_ms`.

Each access supplies a storage operation, independent extent sizes in bytes,
and a positive effective payload size per request. Disk extents represent
contiguous ranges; object extents represent separately requested objects or
multipart payloads. Requests cannot coalesce across extents.

```text
operations_per_execution = sum(ceil(extent_bytes / bytes_per_request))
operations = operations_per_execution * executions
executions = 1 for Once, scope evaluation count for PerEvaluation
```

Zero-length extents cost zero data requests. Empty-object creation, metadata
requests, retries, seek latency, prefetch, and multipart control requests are
outside this payload model. The caller must supply the actual physical
request payload limit, not assume an object-store or disk block size.

For example, two 40-byte objects with 32-byte requests require four GETs per
execution. Three evaluations require 12 GETs, even if two parents share the
scan. A retained `Once` scan requires four GETs over the same horizon. A
160-byte write with a 64-byte request size requires three writes per execution.
Round before multiplying by demand, using checked integer arithmetic.

The output keeps four operation-count dimensions: disk reads, disk writes,
object GETs, and object PUTs. Counts do not replace byte estimates. A versioned
`StorageCalibration` assigns a finite, nonnegative cost per operation in the
same cost units as the base calibration. Both alternatives use the same
profile and coefficients before ranking. Scan read extents must add up to the
scan's authoritative `source_read_bytes`; explicit additional storage actions
can be bound to other physical nodes.

A request-only objective may set all base CPU, scan-byte, and memory
coefficients to zero if the snapshot supplies valid storage evidence and at
least one positive storage coefficient. With a zero base objective, missing
storage evidence or an all-zero storage calibration makes the comparison
unavailable. This check occurs when the target snapshot is available;
standalone base-resource calibration still rejects an all-zero objective.
The physical-plan adapter requires a nonblank base calibration version even
for a request-only objective, so exported combined model provenance remains
identifiable; a storage calibration version cannot substitute for it.

`StorageEstimate` returns totals and per-node terms with model, evidence, and
calibration versions. DAG annotations expose counts as `CostInput`s with
`operations` units. Per-node terms use `physical_node:<id>:<dimension>`;
annotation provenance identifies `storage-requests-v1` and its calibration.
The viewer displays these terms in the existing cost sidebar. The base byte
estimate and storage request estimate remain independently inspectable.

Missing entries, expired/future evidence, incompatible node snapshots, zero
request sizes, invalid calibration, and overflow return typed analytical
errors. When used by plan ranking/export they make that comparison unavailable.
This profile extends the physical-plan adapter; the separate summary-maintenance
lifecycle estimator retains its existing dimensions. Cache behavior is a
separate model input and is not inferred here.

Verification:

```sh
cargo test -p asap-aware-mapping --test storage_io
cargo test -p asap-aware-mapping --lib storage_io
cargo test -p asap-devtools --bin dag_export storage_requests_export_and_change_plan_selection
```
