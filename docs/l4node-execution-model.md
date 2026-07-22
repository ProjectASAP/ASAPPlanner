# L4

L4 is the sketch-bound IR — `L4Node`/`SummaryExpr` in `asap-sketch`,
committing *what* answers each intent (a sketch family + params, or an
exact accumulator) that L3 (`QueryExpr`) deliberately leaves open. It
supports two different operations, previously only one of which had a
home in `ASAPController`.

## Planning-time L4

`asap_plan::bind`: `QueryExpr -> L4Node`. A *decision*, made once per query
shape, symbolically — which sketch family and params should answer this
intent, with no reference to what's actually stored anywhere.
`docs/design.md`'s "Sketch binding is already committed by L4" describes
this half, and it's the only half that existed before this doc.

## Serving-time L4

`L4Node -> Value`. A *lookup*, done on every query: resolve each leaf of
an already-decided tree against whatever is actually materialized right
now — which may be missing, may need merging across multiple instances,
or may disagree on params in ways planning time never sees. Until
`crates/sketch/src/exec.rs` (`SummaryExecutor` trait + `execute`
function), nothing in `ASAPController` defined this half; every
deployment answering queries would otherwise reinvent the same recursive
walk and merge rules independently.

See that module's doc comments for the design — the trait's split of
responsibility, the nested-composition rules, what's out of scope, and
the `SummaryAgg`-over-`SummaryEstimate` open question. This doc is just
the map; `exec.rs` is the territory.

## Consumer

ASAPQuery-backend's `data_plane` is the first (planned) consumer — see
its own `data_plane/docs/l4node-plan-executor-design.md`.
