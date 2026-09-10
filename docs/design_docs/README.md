# ASAPPlanner design overview

ASAPPlanner is a reusable planning library. It converts queries and workload
requirements into legal, ranked, deployment-independent Post-ASAP candidates.
It does not commit, deploy, or execute a physical plan; downstream systems such
as ASAPQuery-backend bind the candidates to physical alternatives, make the
deployment-level decision, and run the selected contract.

## Planner component flow

```mermaid
flowchart TD
    subgraph INPUTS[Inputs]
        QUERY[SQL, PromQL, or MetricsQL queries]
        WORKLOAD[Query and data workloads<br/>accuracy and latency requirements<br/>planning horizon]
        CATALOG[Schema and function catalogs]
        PROVIDER[Downstream capabilities<br/>physical alternatives and cost evidence]
    end

    subgraph PLANNER[ASAPPlanner boundary - reusable semantic planning]
        subgraph FRONTENDS[Query frontends]
            PARSE[Parse and translate]
            CANON[Resolve names and canonicalize]
            PRE[Pre-ASAP QueryExpr DAG]
            PARSE --> CANON --> PRE
        end

        subgraph MAPPING[ASAP-aware mapping]
            CSE[Canonical sharing and CSE]
            STRATEGIES[Replacement strategies<br/>enumerate legal alternatives]
            SPACE[PlanSpace of candidate<br/>Post-ASAP workload DAGs]
            CSE --> STRATEGIES --> SPACE
        end

        subgraph CORRECTNESS[Legality and accuracy]
            LEGAL[Semantic, schema, phase,<br/>and capability checks]
            BUDGET[AccuracyBudgetAllocator<br/>proposes local requirements]
            ACCURACY[AccuracyModel<br/>derives and propagates guarantees]
            SATISFY[Keep candidates whose end-to-end<br/>guarantees satisfy query requirements]
            LEGAL --> BUDGET --> ACCURACY --> SATISFY
        end

        subgraph RANKING[Lifecycle expansion and planner ranking]
            LIFECYCLE[Expand legal summary-maintenance<br/>lifecycle alternatives]
            COST[Cost models annotate and rank<br/>every eligible candidate]
            RANK[Preserve every ranked candidate<br/>with cost and guarantee]
            LIFECYCLE --> COST --> RANK
        end

        subgraph OUTPUTS[Planner output boundary]
            POST[PlanSpace and RankedGroups<br/>all legal Post-ASAP candidates]
            CONTRACT[Aligned costs, summary parameters,<br/>lifecycle contracts and guarantees]
            EXPLAIN[Assumptions and<br/>structured rejections]
        end
    end

    subgraph DOWNSTREAM[Downstream physical-planning boundary]
        BIND[Bind each logical candidate to<br/>concrete algorithms and topology]
        PHYSICAL[Enumerate feasible complete physical alternatives<br/>with stable identities and resource evidence]
        DRANK[Apply deployment constraints and<br/>cost or rerank physical alternatives]
        COMMIT[Commit one compatible<br/>whole-workload physical plan]
        COMPILE[Compile runtime plan projections<br/>and materialization actions]
        DEPLOY[Validate, deploy, and activate]
        EXECUTE[Execute queries and<br/>maintain summary state]
        FEEDBACK[Report capabilities, readiness,<br/>measured cost and accuracy evidence]
        BIND --> PHYSICAL --> DRANK --> COMMIT --> COMPILE --> DEPLOY --> EXECUTE --> FEEDBACK
    end

    QUERY --> PARSE
    CATALOG --> CANON
    PRE --> CSE
    WORKLOAD --> STRATEGIES
    WORKLOAD --> BUDGET
    WORKLOAD --> LIFECYCLE
    PROVIDER --> LEGAL
    PROVIDER -. optional evidence feedback .-> COST
    SPACE --> LEGAL
    SATISFY --> LIFECYCLE
    RANK --> POST
    RANK --> CONTRACT
    RANK --> EXPLAIN

    POST --> BIND
    CONTRACT --> BIND
    FEEDBACK -. next planning cycle .-> PROVIDER
    PHYSICAL -. optional evidence feedback .-> PROVIDER
```

The ordering in the diagram is a correctness boundary:

1. Frontends normalize source-language queries into the shared Pre-ASAP IR.
2. ASAP-aware mapping enumerates alternatives; it does not choose one.
3. Legality and the accuracy model reject candidates that cannot prove the
   requested semantics and end-to-end guarantee.
4. Lifecycle expansion and cost models rank only eligible alternatives while
   preserving the candidate set. Cost cannot make an illegal candidate legal.
5. Downstream systems bind those candidates to concrete physical alternatives.
   Physical feasibility and deployment costs may change their order; downstream
   then chooses and commits the complete physical plan.

## Module map

| Area | Main crate or module | Responsibility |
|---|---|---|
| Shared IR | `asap-types` | Pre-ASAP and Post-ASAP expressions, schemas, workloads, guarantees, and exported plan data |
| Query frontends | `frontend-sql`, `frontend-promql`, `frontend-metricsql` | Parse source languages and produce canonical Pre-ASAP queries |
| ASAP-aware mapping | `asap-aware-mapping` | Candidate generation, CSE, legality, accuracy propagation, lifecycle expansion, costing, and ranking |
| Developer inspection | `devtools` | Expose planner DAGs, alternatives, decisions, and explanations for inspection |
| End-to-end validation | `integration-tests` | Verify behavior across frontends, mapping, and output IR |

## Inputs and outputs

Planner inputs are intentionally broader than a query string. Selection may
depend on query recurrence, data arrival and distribution, accuracy and latency
requirements, the planning horizon, available materialized state, downstream
capabilities, and complete cost evidence. Missing or stale evidence must remain
explicit rather than being treated as zero.

The primary output boundary is `PlanSpace` plus its `cost_sorted` view: every
memo group retains its legal Post-ASAP candidates, ranked with index-aligned
costs. Downstream combines compatible choices across groups rather than
assuming that the first candidate in each group is already a feasible physical
workload plan. Each candidate carries the
planner-owned semantic decisions needed downstream: summary algorithms and
parameters, shared producer identities, logical time coverage, maintenance
lifecycle requirements, and accuracy guarantees. Explanation output records
costs, assumptions, and why candidates were rejected before ranking.

ASAPQuery-backend and other downstream applications translate the candidates
into physical alternatives. They own concrete implementations, storage layout,
placement, sharding, deployment-level cost and compatibility, final commitment,
serving, and operational feedback. Their physical planning can reorder
candidates because it has evidence that the reusable Planner does not, but it
must not silently change Planner-owned semantics.

When a downstream provider supplies complete physical alternatives and evidence
back to Planner, `global_selection*` APIs can perform the final compatible
whole-plan comparison as a convenience. That is an iterative specialization of
the same boundary, not a requirement that the reusable Planner hide the ranked
candidate set from downstream consumers.

## Detailed designs

- [Parsing and canonicalization](parse_and_canonicalize.md)
- [Pre-ASAP IR](pre-asap-ir.md)
- [Post-ASAP IR](post-asap-ir.md)
- [ASAP-aware mapping](asap-aware-mapping/README.md)
- [Accuracy guarantees](asap-aware-mapping/end-to-end-accuracy-guarantees.md)
- [Workload demand and summary lifecycle](asap-aware-mapping/workload-demand-and-summary-lifecycle.md)
- [Physical-plan integration](asap-aware-mapping/physical-plan-integration.md)
- [Analytical resource cost](asap-aware-mapping/analytical-resource-cost.md)
- [Searching over plans](asap-aware-mapping/searching_over_plans.md)
- [Explainability](asap-aware-mapping/explainability.md)
- [ASAPPlanner and downstream application boundaries](asapplanner-downstream-boundary.md)
