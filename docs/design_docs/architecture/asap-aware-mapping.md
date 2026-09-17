# ASAP-aware mapping

ASAP-aware mapping explores ways to answer exact query intent with summaries,
sharing, roll-ups and semantic rewrites. Its output preserves eligible
alternatives so callers can compare compatible choices across a workload.
See the [system overview](README.md) for the full pipeline and ownership boundary,
and [mapping concepts](../concepts/asap-aware-mapping.md) for vocabulary.

## Why choices interact

A percentile intent may admit KLL, DDSketch, or exact execution, subject to the
requested guarantee. A locally cheaper algorithm may prevent sharing with
another query. Similarly, sharing a parent changes the effective demand on its
children. Independent rankings are useful views, but do not themselves define a
compatible whole-workload plan.

The [candidate-search design](asap-aware-plan-search.md) explains this dependency
structure. Accuracy and semantic legality constrain candidates before cost can
prefer one. Lack of proof keeps an approximate alternative unavailable; it does
not authorize weakening the query requirement.

## Design invariants

- Preserve the supported legal alternatives through ranking. Cost preferences
  cannot remove a semantically useful choice during rule enumeration.
- Keep semantic legality, accuracy proof, and cost evidence explicit and distinct.
- Preserve sharing identity and parent/child compatibility during selection.
- Carry summary algorithms, parameters and execution contracts into the output;
  downstream physical binding must preserve those decisions.
- Derive explanations from the same candidate space and rejection data.

Planner exposes optional coordinated selection and semantic materialization APIs.
It does not deploy state, assign machines, or execute queries. Physical and
lifecycle comparisons require their corresponding capability and cost evidence;
see the [downstream boundary](planner-downstream-boundary.md).

## Detailed designs and implementation

- [Accuracy guarantees](../proposals/asap-aware-mapping/end-to-end-accuracy-guarantees.md)
- [Analytical resource cost](../proposals/asap-aware-mapping/analytical-resource-cost.md)
- [Workload demand and summary lifecycle](../proposals/asap-aware-mapping/workload-demand-and-summary-lifecycle.md)
- [Shared maintained populations](../proposals/asap-aware-mapping/maintained-populations.md)
- [Physical-plan integration](physical-plan-integration.md)
- [Optimization dimensions](../proposals/asap-aware-mapping/optimizations.md) and [summary properties](../proposals/asap-aware-mapping/summary-properties.md)
- [Code architecture](../../develop_docs/asap-aware-mapping-architecture.md), [contracts](../../develop_docs/asap-aware-mapping-contracts.md), and [extension tasks](../../develop_docs/extend-asap-aware-mapping.md)

The proposal collection includes implemented mechanisms and open extensions;
read each document's implementation status before relying on a capability.
