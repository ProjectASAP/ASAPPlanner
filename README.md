# ASAPPlanner

ASAPPlanner turns SQL, PromQL, and MetricsQL query workloads into legal candidate plans that may use Approximate Streaming Analytics Primitives (ASAPs), such as sketches and exact summaries. It normalizes language-specific queries into a shared representation, enumerates semantically equivalent alternatives, checks each against the queries' accuracy and latency requirements, and selects a physical plan under the deployment's cost model. Downstream systems deploy and execute that plan.

## Start here

- New to the repository? Read the [planner pipeline](docs/design_docs/concepts/planner-pipeline.md), then the [glossary](docs/design_docs/concepts/glossary.md).
- Want to run a query? Follow [Run and inspect a query](docs/user_guide_docs/run-a-query.md).
- Want to see how a workload is planned, stage by stage? Use the [Stage Viewer](docs/user_guide_docs/stage-viewer.md).
- Embedding Planner? Use the [library API guide](docs/develop_docs/library-api.md).
- Extending Planner? Read the [planning stages design](docs/design_docs/proposals/planner-layering.md), then the [ASAP-aware mapping architecture](docs/develop_docs/asap-aware-mapping-architecture.md), which maps each stage to its code, then [extend ASAP-aware mapping](docs/develop_docs/extend-asap-aware-mapping.md).
- Evaluating a design? Browse the [design documentation](docs/design_docs/README.md), [developer documentation](docs/develop_docs/README.md), and [user guides](docs/user_guide_docs/README.md).

The [documentation map](docs/README.md) gives each audience a complete reading path.

## Build

    cargo build
    cargo test --workspace

No external setup is required for the standard build and test suite.

### Optional: share dependency builds across worktrees

Each Git worktree has its own `target/`, so a new worktree rebuilds every
dependency (DataFusion, Arrow, HiGHS, ...). [kache](https://github.com/kunobi-ninja/kache)
caches compiler outputs on your machine and restores them in other worktrees:

    cargo install kache --locked
    kache init

`kache init` makes kache Cargo's compiler wrapper and starts its background
daemon. Its other offers (shell shims, a login service) are optional. To use
kache without `kache init`, set `rustc-wrapper = "kache"` under `[build]` in
`~/.cargo/config.toml`.

- `kache stats --last-build` shows the hit rate; `kache explain <crate>` says why
  a crate missed.
- `KACHE_DISABLED=1 cargo build` bypasses the cache for one build.
- The HiGHS C++ build (`highs-sys`) still reruns in each new worktree, because
  its output records its absolute build path.
- The cache needs about 14 GiB per toolchain and profile.
