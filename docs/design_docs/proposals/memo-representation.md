# Memo representation: compressing the space of logical DAGs

Brainstorm source of truth. Audience: planner designers. Status: proposal, nothing implemented.
Related issues: #1160 (summary state as its own group), #1171 (gcd widths only), #1197 (planning
slowdown), #1201 (producer–consumer formulation).

## Context

Stages 0–2 enumerate the set of logical DAGs that can answer a workload. Stage 3's MILP then picks
the cheapest one. Enumeration only decides what is *possible*; cost decides what is *optimal*.

The set of all possible DAGs is too large to list, so we store a **compressed representation** of
it. Any design to do this has three aspects:

- **(a) Representation:** what is stored
- **(b) Rules:** how we update/extend the compressed representation of the set of possible DAGs (e.g. adding different window candidates.)
- **(c) Enumeration:** how to enumerate the list of possible DAGs from the compressed representation

There have been three designs:

1. **Pre-memo:** an explicit list of whole DAGs. Nothing is compressed. This was the default until #934 (7bbade26, 2026-10-25); the memo first appeared as an opt-in search in #900 (89a5ada4).
2. **Today:** Memo groups share common sub-expressions. Every other kind of variation is written out as copies.
3. **Proposed:** Also compress (i) free properties that multiply (sketch type × width) and (ii)
   subsumption and sharing (k, ε, window, labels). It also records which rule introduced each
   variation.

| Compresses | Pre-memo | Today | Proposed |
|---|---|---|---|
| Shared sub-expressions | – | ✓ groups | ✓ |
| Product of free properties | – | – | ✓ free properties |
| Subsumption and sharing | – | – (copies) | ✓ requirements + derived properties |

## Running example

Two queries over `data[5m]`, every 60s:

```
q50: quantile_over_time(0.5,  data[5m])   ε .01
q99: quantile_over_time(0.99, data[5m])   ε .001
```

Choices: exact or a sketch; sketch ∈ {Kll, DD}; built directly (`SummaryAgg`) or from tumbling
windows (`Merge ← TWChain`, width 60s); private or shared.
(TWChain == TumblingWindowChain)

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

**(c)** Same as (a) i.e. we just store the list of DAGs

## 2. Today's memo: groups plus concrete candidates

**(a)** A *group* is one distinct sub-expression; equal expressions across queries share one group.
A *candidate* is one way to implement a group. Each candidate is a chain of concrete operators plus the groups it
reads. Each combination of choices (sketch, parameters, direct or windowed, width) is written out as its own candidate, with every value fixed. Every shared producer is a separate group.
```
G0 Scan
G1 TimeRange 300s <- G0
G2 q50: [exact<-G1] [Est←Agg Kll(.01)<-G1] [Est←Agg DD(.01)<-G1]
        [Est←Merge←TW Kll(.01)<-G0] [Est←Merge←TW DD(.01)<-G0]
        [Est<-G5] [Est<-G6] [Est<-G7] [Est<-G8]                 shared consumers
G3 q99: the same, built for .001, plus consumers of G5..G8
G5 Agg Kll(.001)<-G1   G6 Agg DD(.001)<-G1                       one group per
G7 TW  Kll(.001)<-G0   G8 TW  DD(.001)<-G0                       shared producer
```

**(b)** Rules add candidates and groups. The window rule adds windowed copies of each consumer for
every width. `SharedProducer` (`pass2/memo/rules/shared_producer.rs`), used by the summary and
top-k rules, works in three steps:

1. `sets()`: find consumers where one producer could subsume the others, e.g. `topk(5, x)` and
   `topk(10, x)` by job.
2. `coverings()`: decide which producers to offer the set, e.g. `Limit(10) ← Sort(x)`.
3. `derive()`: for each consumer × covering, add a new candidate that reads the producer (now a
   group), e.g. `Limit(5) ← [ranking]`.

Each consumer's original candidates stay, so the MILP chooses between sharing and not sharing. The
number of copies added is consumers × coverings.

**(c)** `pass2/memo/enumeration.rs`: from the query roots, pick one candidate per reached group and
recurse into its inputs. For example, G2 and G3 both pick `Est<-G7`, and G7 picks its only candidate.

Measurements and today's types: see the appendix.

## 3. Proposed: consumers, requirements, free and derived properties

Today's memo only avoids storing the same sub-expression twice. Most of what's left is repetition
of two kinds: choices that combine freely (any sketch with any width), and producers that differ
only in how much they can serve (a k=10 ranking also answers k=5). So we store each choice once,
and what each consumer needs once, and work out the combinations and the sharing only when we
enumerate.

### Overview

**(a) What is stored.** Today's groups and candidates, plus three additions:

1. Each candidate lists its **free properties**: choices the planner can make freely, with their
   legal values (sketch ∈ {Kll, DD}). One candidate instead of one per value.
2. Each group lists its **consumers** (the groups that read it), each with its **requirement**
   (G2 needs ε.01 from S). One entry per consumer instead of one candidate per possible producer.
3. A new group S, `SummaryState(quantile, data, 300s)`: the quantile summary of data over 5m, with
   no sketch fixed.

```
G0 Scan
G1 TimeRange 300s <- G0
S  SummaryState(quantile, data, 300s)
     [SummaryAgg <-G1]       free: sketch ∈ {Kll, DD}
     [Merge←TWChain <-G0]    free: sketch ∈ {Kll, DD}, tumbling-window width ∈ {60s}
     consumers: G2 requires ε.01, G3 requires ε.001
G2 q50: [exact<-G1] [Estimate(.5)<-S]
G3 q99: [exact<-G1] [Estimate(.99)<-S]
```

**(b) Rules** . For example, the tumbling-window rule adds one candidate to S, `[Merge←TWChain<-G0]` with
width ∈ {60s}, instead of one candidate per width × sketch in every query's group.

**(c) Enumeration** picks one candidate per group, as today. Then, for each group with consumers,
it decides which consumers share a producer. Each producer gets a value for each free property, and
the properties its consumers need (**derived properties**: ε.001 when q50 and q99 share, ε.01 for
q50 alone).

### (a) Representation: details

```rust
struct Memo      { groups: Vec<MemoGroup>, query_roots: Vec<QueryRoot<MemoGroupId>> }
struct MemoGroup { logical_node, candidates: Vec<Candidate>,
                   consumers: Vec<(MemoGroupId, Requirement)> }    // empty if none
struct Candidate { root_node, input_groups, fused_groups,
                   free: Vec<FreeProperty> }                       // empty unless it builds a summary
enum FreeProperty { Sketch(Vec<SketchKind>), TumblingWindowWidthMs(Vec<u64>) }  // legal values
struct Requirement {                // what a consumer needs
    accuracy,                       // ε, with budget parts folded in
    lookback_ms,                    // how far back it reads: 300s for data[5m]
    evaluation_interval_ms,         // how often it is evaluated: 60s
    k: Option<u64>,                 // top-k only
    labels: Option<LabelSet>,       // rollup and scan widening only
}
fn combine(requirements: &[Requirement]) -> Option<DerivedProperties>  // not stored; see below
```

S's `logical_node` is a new logical operator, `SummaryState { kind, input, window }`: a summary
with no sketch or parameters fixed. Today's `SummaryAgg` can't serve, because it fixes the
sketch and is only one way to build S (the direct one).

- **Consumer:** a group that reads another group. G2 and G3 are consumers of S; in (b), T5 and T10 are consumers of a ranking.
- **Requirement:** what a consumer needs from the group it reads. Each field comes from somewhere:
  ε from the query's `QueryRequirements.accuracy`, split by budget parts; lookback, k and labels
  from the query expression (or a consumer above); evaluation interval from the query's schedule.
  Lookback is `window_ms` in today's code (`TumblingWindowRequirement`,
  `pass2/memo/shared_configurations.rs`); evaluation interval is `evaluation_grid_ms()`
  (`crates/types/src/workload/mod.rs`): the fixed interval, combined by gcd with a range query's
  step and its evaluation phase.
- **Producer:** a candidate with all its properties fixed, e.g. `Merge ← TWChain[5×60s] Kll(k for .001)`
  (`struct Producer` in (c)).
  Consumers read producers. The properties come in two kinds:
  - **Free properties:** picked from legal values, with no order between them: sketch type, and
    tumbling-window width (the length of each window, e.g. 60s gives `TWChain[5×60s]` over 5m; not
    the sketch size). Consumers that share a producer must agree on them.
  - **Derived properties:** set from the requirements of the consumers it serves. They are ordered:
    a stronger value serves a weaker requirement (a sketch built for ε.001 also serves a consumer
    requiring ε.01; a k=10 ranking also serves k=5).

  For `S` built windowed with Kll, serving q50 and q99:

  | Derived property | Value |
  |---|---|
  | ε built for | ε.001 |
  | sketch parameters | Kll k for .001, `size(sketch, ε)` |
  | lookback kept | 300s |
  | evaluation interval | 60s |
  | window count | 5, lookback / width at 60s |

  Derived properties also restrict the free values: the width must divide the evaluation interval
  (so window boundaries line up with evaluation times) and the lookback.
- **`combine(requirements) -> Option<DerivedProperties>`:** the weakest derived properties that
  serve every given requirement, or none if they can't share. E.g. ε.01 and ε.001 combine to
  ε.001. Each rule in (b) gives how its field combines.
- **`subsumes(d: DerivedProperties, r: Requirement) -> bool`:** a producer with properties d can
  serve a consumer requiring r.

Derived properties are not stored in the memo: they depend on which consumers share a producer,
which the memo does not decide. They are computed from the memo once the rules have finished, when
listing producer options in (c).

### (b) Rules: details

Each rule takes the compressed representation and extends it. Notation, one group per line:
`G: [candidate] [candidate]  consumers: ...`, with free properties after `|` in a candidate.

**Pass 1** (`pass1/`). Adds S, its direct candidate and its consumers.
```
in:   G2 q50: [exact <-G1]
      G3 q99: [exact <-G1]
out:  G2 q50: [exact <-G1] [Estimate(.5) <-S]
      G3 q99: [exact <-G1] [Estimate(.99) <-S]
      S:      [SummaryAgg <-G1 | sketch ∈ {Kll, DD}]  consumers: G2 ε.01, G3 ε.001
```
ε: subsumes ≤, combine min.

**Tumbling windows** (`pass2/memo/window_candidates.rs`). Adds the windowed candidate and the
lookback and evaluation-interval requirement fields.
```
in:   S: [SummaryAgg <-G1 | sketch]  consumers: G2 ε.01, G3 ε.001
out:  S: [SummaryAgg <-G1 | sketch] [Merge←TWChain <-G0 | sketch, width ∈ {60s}]  consumers: G2 (ε.01, 300s, 60s), G3 (ε.001, 300s, 60s)
```
Lookback: subsumes ≥, combine max. Evaluation interval: subsumes if it divides, combine gcd.

**Shared sizes and layouts** (`shared_configuration_readers.rs`, `state_groups.rs`). Deleted.
Today it adds one group per concrete summary: G5–G126 for three quantiles (G5–G6 direct,
G7–G126 windowed at 60 widths, each × Kll/DD). Free properties and producer options replace them;
see "Relation to `SharedProducer`" below.

**Top-k** (`topk.rs`), on `topk by (job) (5, x)` and `topk by (job) (10, x)`. Adds a ranking group
R that both read. k: subsumes ≥, combine max.
```
in:   T5:  [Limit(5) ← Sort <-X]
      T10: [Limit(10) ← Sort <-X]
out:  T5:  [Limit(5) ← Sort <-X] [Limit(5) <-R]
      T10: [Limit(10) ← Sort <-X] [Limit(10) <-R]
      R:   [Sort <-X]  consumers: T5 k=5, T10 k=10
```

**Rollup, finer aggregate** (`rollup.rs`, `finer_aggregate.rs`), on `sum by (job) (x)` and
`sum by (job, instance) (x)`. Adds an aggregate group A both read, at labels to be chosen. Labels:
subsumes ⊇, combine union.
```
in:   Aj:  [Sum by job <-X]
      Aji: [Sum by job,instance <-X]
out:  Aj:  [Sum by job <-X] [RollUp to {job} <-A]
      Aji: [Sum by job,instance <-X] [RollUp to {job,instance} <-A]
      A:   [Sum <-X]  consumers: Aj {job}, Aji {job,instance}
```
`RollUp` to the producer's own labels is a no-op. Only for aggregations that roll up.

**Scan widening** (`scan_labels.rs`), on `sum by (job) (m)` and `sum by (instance) (m)`. Adds a scan
group M both read. Labels: subsumes ⊇, combine union.
```
in:   Sj: [Scan m keep {job}]
      Si: [Scan m keep {instance}]
out:  Sj: [Scan m keep {job}] [Project {job} <-M]
      Si: [Scan m keep {instance}] [Project {instance} <-M]
      M:  [Scan m]  consumers: Sj {job}, Si {instance}
```

**Summary inputs, summary results** (`summary_inputs.rs`, `summary_results.rs`). Not needed: they
exist to expose intermediate results as groups, and groups like S already are those.

**Relation to `SharedProducer`.** Today the summaries (`SummaryStates`) and top-k (`TopKRankings`) rules
implement the trait. This proposed design handles these automatically:
- `sets()`, consumers that could share, becomes the group's consumer list;
- `coverings()`, producers to offer, becomes the group's producer options, computed in (c);
- `derive()`, a copied candidate per consumer × producer, disappears: each consumer has one
  candidate reading the group (`Limit(5) <-R`), whatever producer is chosen later.

### (c) Enumeration: details

Enumeration has two steps: choose a candidate per group, as today, then assign consumers to
producers.

```
for each choice of candidate per reached group     # reached: from a root via chosen candidates' inputs
  for each reached group S with consumers:
    for each Assignment of S (below)               # one DAG per assignment
  emit the DAG
```

**Assign consumers to producers**, once per group S. Input from the 3a structs; output types
belong to enumeration and are not stored in the memo:
```rust
// input C: memo.groups[S].consumers, kept if the consumer's chosen Candidate reads S
//   e.g. [(G2 q50, ε.01), (G3 q99, ε.001)]

struct Producer {                         // one build of S
    candidate: CandidateId,               // which of S's candidates
    free: Vec<ValueOfFreeProperty>,       // one per FreeProperty, e.g. Sketch(Kll), Width(60s)
    derived: DerivedProperties,           // e.g. ε.001
}
struct Assignment {                       // output
    producers: Vec<Producer>,
    reads: Vec<(MemoGroupId, usize)>,     // consumer → index into producers
}
//   shared:   producers [P1 ε.001],           reads [(q50, 0), (q99, 0)]
//   separate: producers [P1 ε.01, P2 ε.001], reads [(q50, 0), (q99, 1)]
```

Two ways to compute it, giving the same output:
- **Split then combine.** Split C into groups in every way; each group gets one producer at
  `combine(group)`.
- **Pick then group** (proposed). First list the **producer options**, `Vec<DerivedProperties>`: every distinct `combine` over
  subsets of C, i.e. the derived properties a producer of S may have. Compute them by combining
  pairs until nothing new appears. Here: {ε.01, ε.001}. Then each consumer picks an option that
  subsumes its requirement; consumers that pick the same option share one producer. Skip a pick
  when a weaker option serves the same consumers (overbuilt).

Either way, then choose each `Producer`'s `candidate` and `free` values.

**Why pick then group.** It drops only dominated assignments: it never builds two producers of S
with the same derived properties, which costs more than one shared producer. n consumers at one
ε have 15 splits for n=4 and 115,975 for n=10; all but "everyone shares" are dominated, and pick
then group yields only that one. More importantly, the options are a short list known before
deciding who shares (at most one per distinct ε here), and each consumer's pick is independent of
the others. That lets a later stage keep sharing compressed instead of expanding it into splits.

**Worked through:**

| Case | Producers ← consumers | DAGs |
|---|---|---|
| both exact | – | 1 |
| q50 reads S, q99 exact | P1 (ε.01) ← q50 | 4 (2 candidates of S × 2 sketches) |
| q99 reads S, q50 exact | P1 (ε.001) ← q99 | 4 |
| both read S, shared | P1 (ε.001) ← q50, q99 | 4 |
| both read S, separate | P1 (ε.01) ← q50; P2 (ε.001) ← q99 | 16 |
| **Total** | | **29** |

```
shared:   P1 = Merge ← TWChain[5×60s] Kll(k for .001) ← Scan
          q50: Estimate(.5) ← P1        q99: Estimate(.99) ← P1
separate: q50: Estimate(.5)  ← SummaryAgg DD(α for .01) ← TimeRange ← Scan
          q99: Estimate(.99) ← Merge ← TWChain Kll(k for .001) ← Scan
```

The memo stores 2 candidates per query, 2 candidates of S, their free properties and 2 consumers'
requirements, never the 29 DAGs. For three quantiles at one ε, today's 127 groups / 247
candidates per root become 6 groups / 2 candidates per root.

## 4. Stage 3 over the proposed memo

Stage 3 chooses along three axes:

| Axis | Scope | Stored as |
|---|---|---|
| Which candidate | within a group | `MemoGroup.candidates` |
| Which free-property values | within a candidate | `Candidate.free` |
| Which consumers share a producer | across groups | `MemoGroup.consumers` + producer options |

As a facility-location problem:
```
x[g,c]  group g uses candidate c
y[r,p]  consumer r reads producer option p (p subsumes r)
z[p]    producer p is built  ⇐  any y[r,p]
```

Sketch parameters are not an axis. They are a derived property, `size(sketch, ε)`, set after
sharing. Stage 3 sees them either as discrete values (one per producer option) or as a
constraint `k ≥ k_min(r) · y[r,p]`. The constraint form needs a cost that is linear in k (Kll
roughly is; DD in 1/α).

## Implications

- Smaller memo and dumps. The private-chain copies and the guarantee duplicates go away, because
  the guarantee is computed per (producer, consumer) instead of being part of node identity.
- The search shrinks only where costs separate: a cost that depends on width × sketch still needs a
  variable per (width, sketch) pair.
- Correctness check: decode both designs on small workloads and compare the sets of DAGs, using
  `enumeration.rs` as the reference.

## Open questions

1. Do `topk`, `rollup`, `finer_aggregate` and `scan_labels` really fit `Requirement`, with the
   `combine` and `subsumes` in (b)?
2. Where do factored candidates get expanded? (i) Just before Stage 2: small change, but the MILP
   stays large and #1197 isn't fixed. (ii) Keep them factored into the MILP (#1201): fixes #1197, but
   a larger change. This also decides where producer options (and so derived properties) are
   computed: at the end of Stage 1 in (i), while building the MILP in (ii).
3. Can a producer's cost depend on its consumers (query-time hold time, evaluation alignment)? If
   so, the model becomes facility cost + per-assignment cost.
4. Should multi-operator private chains give way to one operator per candidate (classic Cascades)?
5. Sketch parameters in Stage 3: discrete values per producer option, or a `k ≥ …` constraint?

## Glossary

| Term | Meaning |
|---|---|
| Group | one distinct logical sub-expression; equal expressions across queries share one |
| Candidate | one way to implement a group: its own operators + the groups it reads |
| Consumer | a group that reads another group, with its requirement |
| Requirement | what a consumer demands (ε with budget parts, lookback, evaluation interval, k, labels); from the query's requirements, expression and schedule |
| Producer | a candidate of a group with consumers + free-property values + derived properties |
| Free property | a producer choice with legal values and no order (sketch type, tumbling-window width); sharers must agree |
| Derived properties | a producer's properties set from the requirements it serves (ε built for, sketch parameters, lookback kept); ordered |
| `combine(requirements)` | the weakest derived properties serving all of them, or none |
| `subsumes(d, r)` | a producer with derived properties d can serve a consumer requiring r |
| Producer options | every distinct `combine` over subsets of a group's consumers: the derived properties a producer may have |
| Assign consumers to producers | per group: which producers to build and which one each consumer reads |
| Sketch parameters | Kll k, DD α: a derived property, `size(sketch, ε)`, not a choice |
| Sharing | several consumers reading one producer; decided in Stage 3 |

## Appendix: today's memo, measured and typed

### Measurements

Measured at c7d50717 on p50/p90/p99 of `data[5m]`, ε .01, every 60s, 10k series (`stage_pipeline
--workload <w.json> --max-memo-candidates 1000 --out run.json`):

| | Value |
|---|---|
| Groups after Pass 2 | 127 (5 logical + 122 shared producers) |
| Candidates per root group | 247 = 4 pass1 + 120 private window chains + 123 shared consumers |
| Widths enumerated | 60, every divisor of 300s down to 1ms (#1171) |
| Duplicates | cands 1/123 and 124/246 differ only in `guarantee`, which is part of node identity |
| MILP | 10,041 variables, 5.2s |

### Today's memo types

`crates/types/src/stages/{memo,candidates}.rs`:

| Field | Role | Example | Do we need it? |
|---|---|---|---|
| `MemoGroup.logical_node` | what the group computes | G1 = TimeRange 300s | keep |
| `.child_groups` | group-id view of the node's children | G1 → [G0] | derivable |
| `.parents` | reverse of `child_groups` | G1 → [G2,G3] | derivable; Stages 2 and 3 never read it |
| `.queries` | queries that reach the group | G1 → [0,1] | cache of `query_roots` |
| `Candidate.root_node` | the candidate's own operators | `Est ← Agg Kll` | keep |
| `.input_groups` | where its own operators stop | direct → G1, windowed → G0 | keep |
| `.fused_groups` | other groups one candidate also implements | heap for `topk(10, sum_over_time(..))` | keep; rare |
| `.origin` | which rule made it | `tumbling_windows` | debug only |
| `.accuracy_budget_parts` | error-budget shares the sketch is sized for | each operand of `a / b` gets 2 | move into `Requirement` in the new design (see Sec. 3) |
