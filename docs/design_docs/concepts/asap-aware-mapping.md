# Key concepts in details

## Target Sub-DAG

A **Target Sub-DAG** is a region of the pre-ASAP plan where a transformation may apply.

Examples include:

- an aggregation that could be answered by a sketch,
- two computations that could share common work,
- several group-by computations that could be related through roll-up,
- or an expression that has a semantically equivalent representation.

The target may consist of a single operator or several connected operators.

---

## Alternative

An **Alternative** is one valid local realization of a Target Sub-DAG.

For example:

```text
Target:

Quantile(latency, 0.99)

Alternatives:

KLL(latency)
DDSketch(latency)
ExactQuantile(latency)
```

Another example involves computation sharing:

```text
Target:

Aggregate(source, by=[service, region])
Aggregate(source, by=[service])

Alternatives:

1. Compute both independently

2. Compute:
      Aggregate(source, by=[service, region])
   then roll up to:
      Aggregate(..., by=[service])
```

An alternative represents a semantic choice, not necessarily an approximation.

---

## Transformation

A **Transformation** describes a class of plan changes.

Conceptually, a transformation answers:

> When a particular structure appears in a plan, what valid alternatives can replace it?

Examples include:

- replacing an aggregation with compatible summaries,
- sharing equivalent computation,
- rolling up between related group-by levels,
- rewriting an expression into an equivalent form,
- choosing different ways to organize subpopulations,
- choosing different time representations.

A transformation proposes valid alternatives. It does not decide which candidate plan is globally best.

---

## Candidate Plan

A **Candidate Plan** is a complete post-ASAP plan formed by choosing a compatible set of alternatives across the plan.

For example, one candidate plan may choose:

```text
Quantile implementation:
    KLL

Subpopulation organization:
    shared multi-subpopulation summary

Related group-bys:
    roll up from finer-grained aggregation

Semantic form:
    rewrite avg as sum/count
```

Another candidate plan may make different choices at any of these decision points.

The planner compares candidate plans, not isolated alternatives, when interactions between decisions matter.

---

## Cost Model

The **Cost Model** estimates the trade-offs of candidate plans.

Depending on the planning stage, costs may include:

- summary storage,
- ingestion or update work,
- query latency,
- raw-data processing,
- maintenance overhead,
- expected accuracy,
- and workload-dependent reuse.

The cost model is intentionally separate from transformation legality.

A transformation answers:

> Is this alternative valid?

The cost model answers:

> How attractive is the resulting candidate plan relative to other valid candidate plans?

This separation allows the same planning framework to support different cost models, including heuristic, analytical, and empirically learned models.
