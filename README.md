# ASAPPlanner

ASAPPlanner turns SQL, PromQL, and MetricsQL query workloads into legal candidate plans that may use Approximate Streaming Analytics Primitives (ASAPs), such as sketches and exact summaries. It normalizes language-specific queries into a shared representation, then enumerates and ranks semantically equivalent alternatives. Downstream systems choose, deploy, and execute a physical plan.

## Start here

- New to the repository? Read the [planner pipeline](docs/concepts/planner-pipeline.md), then the [glossary](docs/concepts/glossary.md).
- Want to run a query? Follow [Run and inspect a query](docs/guides/run-a-query.md).
- Extending Planner? Start with the [ASAP-aware mapping architecture](docs/architecture/asap-aware-mapping-implementation.md), then [extend ASAP-aware mapping](docs/guides/extend-asap-aware-mapping.md).
- Evaluating a design? Browse the [architecture](docs/architecture/README.md), [decisions](docs/decisions/README.md), and [proposals](docs/proposals/README.md).

The [documentation map](docs/README.md) gives each audience a complete reading path.

## Build

    cargo build
    cargo test --workspace

No external setup is required for the standard build and test suite.
