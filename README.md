# ASAPPlanner

ASAPPlanner turns SQL, PromQL, and MetricsQL query workloads into plans that may use Approximate Streaming Analytics Primitives (ASAPs), such as sketches and exact summaries. It normalizes language-specific queries into a shared representation, enumerates semantically equivalent alternatives, and selects the cheapest one that meets each query's accuracy target. Downstream systems deploy and execute the physical plan.

## Start here

- New to the repository? Read the [planner pipeline](docs/design_docs/concepts/planner-pipeline.md), then the [glossary](docs/design_docs/concepts/glossary.md).
- Want to run a query? Follow [Run and inspect a query](docs/user_guide_docs/run-a-query.md).
- Embedding Planner? Use the [library API guide](docs/develop_docs/library-api.md).
- Extending Planner? Start with the [ASAP-aware mapping architecture](docs/develop_docs/asap-aware-mapping-architecture.md).
- Evaluating a design? Browse the [design documentation](docs/design_docs/README.md), [developer documentation](docs/develop_docs/README.md), and [user guides](docs/user_guide_docs/README.md).

The [documentation map](docs/README.md) gives each audience a complete reading path.

## Build

    cargo build
    cargo test --workspace

No external setup is required for the standard build and test suite.
