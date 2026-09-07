# Physical boundary byte estimates

The physical-plan adapter accepts an optional `BoundaryProfile` in the
immutable `PhysicalEvidenceSnapshot`. `dag_export --planner-cost-json` accepts
the same profile in a top-level `boundaries` field. With no profile, these
dimensions remain unestimated and the existing resource objective is preserved.

The profile is supplementary physical-plan binding: each entry supplies the
complete physical node, its authoritative statistics, and an explicit list of
boundary actions on its output. Every reachable node needs an entry; an empty
list declares ordinary in-memory dataflow with no boundary traffic. The full
node/statistics match prevents rebinding evidence to a different operator with
the same ID. Additional entries may describe other candidate plans. Evidence
must match the immutable snapshot version and be current at planning time:
`observed_at_ms <= planning_time < valid_until_ms`.

Supported boundaries are:

| Boundary | Required evidence | Dimension |
|---|---|---|
| Network/exchange/deployment transfer | Distinct nonempty source and destination locations | Network bytes |
| Materialization/persistence | Memory, disk, or object-store medium | Materialization bytes |

Every boundary also declares its unique physical ID, output logical bytes,
encoded bytes per execution, and positive copy count. Logical bytes must equal
the producer's output statistic. Encoded bytes capture an explicit compression
or serialization estimate; empty and nonempty payloads must agree with the
logical edge. A remote persistence operation may declare both a network action
and a materialization action with distinct IDs; these contribute to different
dimensions and require separate calibration coefficients.

```text
boundary_bytes = encoded_bytes * copies * executions
```

For a shared boundary (`consumer: null`), execution multiplicity comes from
the producer. For a per-consumer boundary, `consumer` must identify an actual,
reachable parent of the producer; multiplicity comes from that consumer.
`Once` means one execution; `PerEvaluation` uses the comparison scope's demand.
Shared producers are traversed once regardless of fan-out. Duplicate boundary
IDs fail closed instead of being ambiguously counted or silently dropped.

Example: a retained producer materializes 40 encoded bytes once. Two consumers
each receive two copies of those 40 bytes over three evaluations. The totals
are 40 materialization bytes and 480 network bytes. Logical in-memory edges
without a boundary action add no traffic. A retained producer can therefore
have a once-only persistence action and repeated transfers to its readers.

`BoundaryEstimate` keeps network and materialization totals separate and returns
per-node and per-boundary terms with model, evidence, and calibration provenance.
A `BoundaryCalibration` supplies finite, nonnegative cost coefficients in the
same cost units as the base resource model, and a nonempty version. Both
alternatives use that profile when ranking. This models byte work, not transfer
latency, bandwidth contention, memory lifetime, or storage requests.

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
cargo test -p asap-aware-mapping --test boundary_cost
cargo test -p asap-devtools --bin dag_export boundary_bytes_export_and_change_plan_selection
python3 -m unittest discover -s tools/dag-viewer -p 'test_render.py'
```
