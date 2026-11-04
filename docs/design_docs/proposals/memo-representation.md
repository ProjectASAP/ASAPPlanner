# Memo representation: compressing the space of logical DAGs

Brainstorm source of truth. Audience: planner designers. Status: proposal, nothing implemented.
Related issues: #1160 (summary state as its own group), #1171 (gcd widths only), #1197 (planning
slowdown), #1201 (producer–consumer formulation).

## Context

Stages 0–2 enumerate the set of logical DAGs that could answer a workload. Stage 3's MILP then picks
the cheapest one. Enumeration only decides what is *possible*; cost decides what is *optimal*, and
only in Stage 3.

The set is far too large to list, so we store a **compressed representation** of it. Any design has
three aspects:

- **(a) Representation:** what is stored.
- **(b) Rules:** how passes extend what is stored.
- **(c) Enumeration:** how to decode the stored form back into the set of DAGs.

There have been three designs:

1. **Pre-memo:** an explicit list of whole DAGs. Nothing is compressed.
2. **Today's memo:** groups share common sub-expressions. Every other kind of variation is written
   out as copies.
3. **Proposed:** also compresses (i) independent choices that multiply (sketch type × width) and
   (ii) strength variants and sharing (k, ε, window, labels). It also records which rule introduced
   each variation.

| Compresses | Pre-memo | Today | Proposed |
|---|---|---|---|
| Shared sub-expressions | – | ✓ groups | ✓ |
| Product of independent choices | – | – | ✓ `open` |
| Strength variants and sharing | – | – (copies) | ✓ requirements |

## Running example

Two queries over `data[5m]`, every 60s:

```
q50: quantile_over_time(0.5,  data[5m])   ε .01
q99: quantile_over_time(0.99, data[5m])   ε .001
```

Choices: exact or a sketch; sketch ∈ {Kll, DD}; shape ∈ {direct `SummaryAgg`, windowed
`Merge ← TWChain` with width 60s}; private or shared.

## 1. Pre-memo: an explicit list of whole DAGs

The rules in `pass2/asap_aware_cse.rs` still work this way: they take whole `QueryRoot`s, e.g.
`rollup_candidates(roots)` and `with_union_scan_labels(roots)`.

**(a)** One complete DAG per combination. Shared parts such as `Scan → TimeRange` are repeated in
every entry.
```
L1: q50 exact                   | q99 exact
L2: q50 direct Kll(k for .01)   | q99 exact
...
Ln: one shared windowed Kll(k for .001) → Estimate(.5), Estimate(.99)
```

**(b)** A rule maps whole DAGs to whole DAGs. For example, the sharing rule finds two builds it can
merge and emits a new DAG.

**(c)** The list is the set.

## 2. Today's memo: groups plus concrete candidates

**(a)** A *group* is one distinct sub-expression; equal expressions across queries share one group.
A *candidate* is one way to implement a group: a chain of concrete operators plus the groups it
reads. Every variation is a separate candidate, and every shared configuration is a separate group.
```
G0 Scan
G1 TimeRange 300s <- G0
G2 q50: [exact<-G1] [Est←Agg Kll(.01)<-G1] [Est←Agg DD(.01)<-G1]
        [Est←Merge←TW Kll(.01)<-G0] [Est←Merge←TW DD(.01)<-G0]
        [Est<-G5] [Est<-G6] [Est<-G7] [Est<-G8]                 shared readers
G3 q99: the same, built for .001, plus readers of G5..G8
G5 Agg Kll(.001)<-G1   G6 Agg DD(.001)<-G1                       one group per
G7 TW  Kll(.001)<-G0   G8 TW  DD(.001)<-G0                       shared configuration
```

**(b)** Rules add candidates and groups. The window rule adds windowed copies of each reader for
every width. `SharedProducer` (`pass2/memo/rules/shared_producer.rs`), used by the summary and
top-k rules, works in three steps:

1. `sets()`: find readers whose producers differ only in strength, e.g. `topk(5, x)` and
   `topk(10, x)` by job.
2. `coverings()`: decide which producers to offer the set, e.g. `Limit(10) ← Sort(x)`.
3. `derive()`: for each reader × covering, add a new candidate that reads the producer (now a
   group), e.g. `Limit(5) ← [ranking]`.

Each reader's original candidates stay, so the MILP chooses between sharing and not sharing. The
number of copies added is readers × coverings.

**(c)** `pass2/memo/enumeration.rs`: from the query roots, pick one candidate per reached group and
recurse into its inputs. For example, G2 and G3 both pick `Est<-G7`, and G7 picks its only candidate.

**Measured** at c7d50717 on p50/p90/p99 of `data[5m]`, ε .01, every 60s, 10k series (`stage_pipeline
--workload <w.json> --max-memo-candidates 1000 --out run.json`):

| | Value |
|---|---|
| Groups after Pass 2 | 127 (5 logical + 122 shared configurations) |
| Candidates per root group | 247 = 4 pass1 + 120 private window chains + 123 shared readers |
| Widths enumerated | 60, every divisor of 300s down to 1ms (#1171) |
| Duplicates | cands 1/123 and 124/246 differ only in `guarantee`, which is part of node identity |
| MILP | 10,041 variables, 5.2s |

Types (`crates/types/src/stages/{memo,candidates}.rs`):

| Field | Role | Example | Verdict |
|---|---|---|---|
| `MemoGroup.logical_node` | what the group computes | G1 = TimeRange 300s | keep |
| `.child_groups` | group-id view of the node's children | G1 → [G0] | derivable |
| `.parents` | reverse of `child_groups` | G1 → [G2,G3] | derivable; Stages 2 and 3 never read it |
| `.queries` | queries that reach the group | G1 → [0,1] | cache of `query_roots` |
| `Candidate.root_node` | the candidate's own operators | `Est ← Agg Kll` | keep |
| `.input_groups` | where its own operators stop | direct → G1, windowed → G0 | keep |
| `.fused_groups` | other groups one candidate also implements | heap for `topk(10, sum_over_time(..))` | keep; rare |
| `.origin` | which rule made it | `tumbling_windows` | debug only |
| `.accuracy_budget_parts` | error-budget shares the sketch is sized for | each operand of `a / b` gets 2 | move into `Requirement` |

## 3. Proposed: summary groups, open choices, requirements

**(a)**
```
G0 Scan
G1 TimeRange 300s <- G0
S  quantile summary of data/300s                     (summary group)
     direct    SummaryAgg <-G1       open: sketch ∈ {Kll, DD}
     windowed  Merge←TWChain <-G0    open: sketch ∈ {Kll, DD}, width ∈ {60s}
     readers   G2 needs ε.01, G3 needs ε.001
G2 q50: [exact<-G1] [Estimate(.5)<-S]
G3 q99: [exact<-G1] [Estimate(.99)<-S]
```
```rust
struct Memo      { groups: Vec<MemoGroup>, query_roots: Vec<QueryRoot<MemoGroupId>> }
struct MemoGroup { logical_node, candidates: Vec<Candidate>,
                   readers: Vec<(MemoGroupId, Requirement)> }      // summary groups only
struct Candidate { root_node, input_groups, fused_groups,
                   open: ConfigurationSpace }                       // empty unless a summary build
struct ConfigurationSpace { sketches: Vec<SketchKind>, widths: Vec<u64> }
struct Requirement { accuracy /* budget parts included */, window_ms, grid_ms }
```

**(b)** Rules declare structure instead of copying candidates:

| A rule declares | Here |
|---|---|
| a structural candidate | `[exact]`, `[Estimate<-S]`; the direct and windowed shapes |
| a free-choice domain in `open` | sketch ∈ {Kll, DD}; width ∈ {60s} |
| a `Requirement` field with `combine` and `covers` | accuracy: combine = min ε; covers = p.ε ≤ r.ε |
| a `derive` function: the reader's operators on top of a producer | `Estimate(q)` |

This is `SharedProducer` split into its parts:

| `SharedProducer` | Proposed |
|---|---|
| `Need` | `Requirement` |
| `sets()` | `MemoGroup.readers` |
| `coverings()` | the combine closure (below) |
| `derive()` copies | `derive()` applied during enumeration; nothing copied |

**(c)** Enumeration:
```
for each choice of candidate per reached group               # as today
  for each summary group S; R = readers whose candidate reads S:
    for each partition of R into blocks                       # who shares
      for each block B:
        p = combine(requirements of B)
        pick a shape c of S and values v from c.open
        producer = c with v; sketch params = size(v.sketch, p)
        each r in B: derive(r) on top of producer
  emit the DAG
```

Worked through:

| Case | Blocks | DAGs |
|---|---|---|
| both exact | – | 1 |
| q50 reads S, q99 exact | {q50} at ε.01 | 4 (2 shapes × 2 sketches) |
| q99 reads S, q50 exact | {q99} at ε.001 | 4 |
| both read S, shared | {q50,q99} at ε.001 | 4 |
| both read S, separate | {q50} at ε.01, {q99} at ε.001 | 16 |
| **Total** | | **29** |

```
shared:   P = Merge ← TWChain[5×60s] Kll(k for .001) ← Scan
          q50: Estimate(.5) ← P        q99: Estimate(.99) ← P
separate: q50: Estimate(.5)  ← SummaryAgg DD(α for .01) ← TimeRange ← Scan
          q99: Estimate(.99) ← Merge ← TWChain Kll(k for .001) ← Scan
```

The memo stores 2 candidates per query, 2 shapes, 2 small `open` lists and 2 requirements, never the
29 DAGs. For three quantiles at one ε, today's 127 groups / 247 candidates per root become 6 groups /
2 candidates per root.

### Combine, covers, closure

For a requirement type R:
- `covers(p, r)`: can a producer built for requirement p serve a reader that needs r?
- `combine(a, b)`: the weakest requirement that covers both, or none if they can't share.

Rules: `covers(combine(a,b), a)` and `covers(combine(a,b), b)`, and no weaker requirement covers
both. A requirement with several fields combines and covers field by field. Some checks need the
configuration too (a width must divide the reader's grid), so `covers` takes (configuration,
reader requirement).

| Requirement | `covers(p, r)` | `combine(a, b)` |
|---|---|---|
| accuracy ε | p.ε ≤ r.ε | min |
| window | p.window ≥ r.window | max |
| grid / width | p.width divides r.grid | gcd |
| top-k `k` | p.k ≥ r.k | max |
| grouping / scan labels | p's labels ⊇ r's labels | union |
| sketch type (free choice) | equal | equal, or none |

**Combine closure:** start with the readers' requirements and add `combine` of every pair until
nothing new appears. The result is every requirement that is the combine of some subset of readers:
the distinct producers worth building. Building stronger than the combine of a producer's readers
costs more and helps no one, assuming cost only grows as requirements strengthen.

- Here: {q50} → ε.01, {q99} → ε.001, {q50,q99} → ε.001. Closure = {ε.01, ε.001}.
- With several fields, a combine can be new: (ε.01, 5m) and (ε.001, 1m) combine to (ε.001, 5m).
- It replaces the partition step: each reader picks one closure element that covers it. q50 may pick
  ε.01 or ε.001; q99 must pick ε.001. The number of partitions grows very fast with the number of
  readers; the closure stays small (n elements when the order is total).
- Today's G5–G8 are closure elements (× shape × sketch), stored as groups. The proposal computes them.

### Dimensions

| Dimension | Scope | Lives in |
|---|---|---|
| A. Candidate | within a group | `MemoGroup.candidates` |
| B. Configuration (shape, `open` values) | within a candidate | `Candidate.open` |
| C. Producer–consumer mapping (sharing) | across groups | `readers` + combine closure |

Sketch parameters are not a dimension: they are `size(sketch, p)`, computed last, after sharing.
Shapes, widths and sketch types have only certain legal values; parameters only need to satisfy a
lower bound. Stage 3 sees parameters either as discrete values (one per closure element) or as a
constraint `k ≥ k_min(r) · y[r,p]`. The constraint form needs a cost that is linear in k (Kll
roughly is; DD in 1/α).

Stage 3 (facility location):
```
x[g,c]  group g uses candidate c                         A
y[r,p]  reader r reads closure element p (p covers r)    B, C
z[p]    producer p is built  ⇐  any y[r,p]
```

**Adding a dimension:**

| Kind | Examples | Add |
|---|---|---|
| Free choice (unordered; sharers must agree) | sketch type, storage tier | a field in `ConfigurationSpace`, its cost, equality in `covers` |
| Requirement (ordered; stronger serves weaker) | ε, window, grid, k, labels | a field in `Requirement`, its `combine`, its `covers` |

Today, adding a dimension means writing a rule that copies candidates for each value.

### Pass 2 rules under the proposal

Classified from file headers only; not yet verified against each rule's code.

| Rule (`pass2/memo/rules/`) | Today | Becomes |
|---|---|---|
| `shared_configuration_readers` | sizes and window layouts via `SharedProducer` | `open` + accuracy/window/grid requirements |
| `topk` | `Limit(k)` from a `k_max` ranking via `SharedProducer` | requirement `k`; derive `Limit(k)` |
| `rollup`, `finer_aggregate` | coarse aggregate from a finer one | requirement grouping; derive merge-to-coarser |
| `scan_labels` | scans widened to a label union | requirement label set |
| `summary_inputs`, `summary_results` | expose intermediate results as groups | not needed once summary groups exist |

## Implications

- Smaller memo and dumps. The private-chain copies and the guarantee duplicates go away, because
  the guarantee is computed per (producer, reader) instead of being part of node identity.
- The search shrinks only where costs separate: a cost that depends on width × sketch still needs a
  variable per (width, sketch) pair.
- Correctness check: decode both designs on small workloads and compare the sets of DAGs, using
  `enumeration.rs` as the reference.

## Open questions

1. Do `topk`, `rollup`, `finer_aggregate` and `scan_labels` really fit `Requirement`, with the
   combine and covers above?
2. Where do factored candidates get expanded? (i) Just before Stage 2: small change, but the MILP
   stays large and #1197 isn't fixed. (ii) Keep them factored into the MILP (#1201): fixes #1197, but
   a larger change.
3. Can a producer's cost depend on its readers (query-time hold time, evaluation alignment)? If so,
   the model becomes facility cost + per-assignment cost.
4. Should multi-operator private chains give way to one operator per candidate (classic Cascades)?
5. Sketch parameters in Stage 3: discrete values per closure element, or a `k ≥ …` constraint?

## Glossary

| Term | Meaning |
|---|---|
| Group | one distinct logical sub-expression; equal expressions across queries share one |
| Candidate | one way to implement a group: its own operators + the groups it reads |
| Summary group | a group whose value is a mergeable summary (sketch or exact accumulator) over an input and window; its candidates are shapes |
| Shape | how a summary is built: direct (`SummaryAgg`) or windowed (`Merge ← TWChain`) |
| `open` | a candidate's free choices (sketch type, width) |
| Reader | a group that reads a summary group |
| Requirement | what one reader needs: ε (budget parts included), window, grid, … |
| `combine` / `covers` | the weakest requirement satisfying both / whether a producer serves a reader |
| Combine closure | all combines of reader subsets: the producers worth building |
| Producer | a built configuration: shape + `open` values + closure element |
| `derive` | a reader's own operators on top of a producer (`Estimate(q)`, `Limit(k)`) |
| Sketch parameters | Kll k, DD α: `size(sketch, requirement)`, not a choice |
| Sharing | several readers reading one producer; decided in Stage 3 |
