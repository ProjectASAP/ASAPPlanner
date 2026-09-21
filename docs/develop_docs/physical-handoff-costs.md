# Physical handoff byte estimates

`asap_types::resources` owns the canonical `PhysicalHandoffBytes`, `PhysicalHandoffKind`,
and `MaterializationMedium` definitions in `resources/physical_handoff.rs`. The mapping
crate re-exports those same types from `physical_handoff_cost` for import compatibility;
all estimator and export consumers therefore use shared definitions, not copies.
Their existing JSON format is unchanged. The shared byte counters support checked
addition without depending on planner errors. Snapshot binding, validation,
calibration, and ranking remain in the mapping crate. handoff traffic/write work
is distinct from CPU work, scanned bytes, and stored byte occupancy; it is not
collapsed into the generic CPU/byte resource container.

The physical-plan adapter accepts an optional `PhysicalHandoffProfile` in the
immutable `PhysicalEvidenceSnapshot`. `dag_export --planner-cost-json` accepts
the same profile in a top-level `boundaries` field. With no profile, these
dimensions remain unestimated and the existing resource objective is preserved.

The profile's `plans` list binds handoffs to complete physical alternatives.
Each plan supplies its `root` and a `nodes` map containing every physical node,
its authoritative statistics, and an explicit list of handoff actions on its
output. An empty handoff list declares ordinary in-memory dataflow with no
handoff traffic. Exactly one plan must match the root, complete node set, and
node/statistics snapshot; missing or ambiguous matches fail closed. This lets
alternatives reuse a producer identity while declaring different transfers to
their respective consumers. All alternatives share the profile's immutable
evidence generation and calibration. Evidence
must match the immutable snapshot version and be current at planning time:
`observed_at_ms <= planning_time < valid_until_ms`.

Supported handoffs are:

| Handoff | Required evidence | Dimension |
|---|---|---|
| Network/exchange/deployment transfer | Distinct nonempty source and destination locations | Network bytes |
| Materialization/persistence | Memory, disk, or object-store medium | Materialization bytes |

Every handoff also declares its unique physical ID, output logical bytes,
encoded bytes per execution, and positive copy count. Logical bytes must equal
the producer's output statistic. Encoded bytes capture an explicit compression
or serialization estimate; empty and nonempty payloads must agree with the
logical edge. A remote persistence operation may declare both a network action
and a materialization action with distinct IDs; these contribute to different
dimensions and require separate calibration coefficients.

```text
handoff_bytes = encoded_bytes * copies * executions
```

For a shared handoff (`consumer: null`), execution multiplicity comes from
the producer. For a per-consumer handoff, `consumer` must identify an actual,
reachable parent of the producer; multiplicity comes from that consumer.
`Once` means one execution; `PerEvaluation` uses the comparison scope's demand.
Shared producers are traversed once regardless of fan-out. Duplicate handoff
IDs fail closed instead of being ambiguously counted or silently dropped.
handoff profiles currently require `CacheProfile::NoCache`. Cache evidence does
not identify which physical transfers or materializations are skipped on a hit,
and handoff execution counts derive from the comparison scope rather than a
post-cache schedule. Combining a handoff profile with `CacheProfile::Evidence`
therefore fails closed until cache-aware handoff execution evidence exists.
Supported exports retain the no-cache profile provenance alongside handoff
model/calibration versions.

Example: a retained producer materializes 40 encoded bytes once. Two consumers
each receive two copies of those 40 bytes over three evaluations. The totals
are 40 materialization bytes and 480 network bytes. Logical in-memory edges
without a handoff action add no traffic. A retained producer can therefore
have a once-only persistence action and repeated transfers to its readers.

`PhysicalHandoffEstimate` keeps network and materialization totals separate and returns
per-node and per-handoff terms with model, evidence, and calibration provenance.
A `PhysicalHandoffCalibration` supplies finite, nonnegative cost coefficients in the
same cost units as the base resource model, and a nonempty version. Both
alternatives use that profile when ranking. This models byte work, not transfer
latency, bandwidth contention, memory lifetime, or storage requests.
Base CPU, scan, and retained-memory coefficients may all be zero when at least
one handoff coefficient is positive. An absent handoff profile or an entirely
zero objective remains unavailable for ranking.
Both the base and handoff calibration versions must be nonempty, including
when the base coefficients are all zero; combined annotations identify both.

Annotations expose totals in `bytes`, plus terms named
`physical_node:<id>:<dimension>` and `boundary:<id>:<dimension>`. The existing
viewer cost sidebar displays all terms and the combined formula/calibration
version (`physical-boundary-bytes-v1`) with the evidence version. No network
traffic is inferred from logical edges, operator buffers, or scan bytes.

Unknown endpoints, mismatched payloads, absent node evidence, duplicate IDs,
stale evidence, invalid coefficients, and integer overflow return typed errors;
ranking/export report the comparison as unavailable. This extends the physical
plan adapter; lifecycle-specific summary-maintenance costing and caching are
separate follow-up integration points.

Verification:

```sh
cargo test -p asap-aware-mapping --test physical_handoff_cost
cargo test -p asap-devtools --bin dag_export handoff_bytes_export_and_change_plan_selection
python3 -m unittest discover -s tools/dag-viewer -p 'test_render.py'
```
