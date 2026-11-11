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
3. **Proposed:** Also compress (i) unordered properties that multiply (sketch type × width) and (ii)
   subsumption and sharing (k, ε, window, labels). It also records which rule introduced each
   variation.

A producer *satisfies* a consumer when it meets the consumer's requirement. One producer
*subsumes* another when it satisfies every consumer the other does: a sketch built for ε.001
subsumes one built for ε.01, a top-10 ranking subsumes a top-5 one.

| Compresses | Pre-memo | Today | Proposed |
|---|---|---|---|
| Shared sub-expressions | – | ✓ groups | ✓ |
| Product of unordered properties | – | – | ✓ unordered properties |
| Subsumption and sharing | – | – (copies) | ✓ requirements |

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

**(a)** A *group* is one distinct sub-expression. A *candidate* is one way to implement a group. Each candidate is specified by a chain of operators along with the groups it reads (i.e. the groups these operators take as input). Each combination of choices (sketch, parameters, direct or windowed, width) is written out as its own candidate, with every value fixed. Every shared producer is a separate group.
```rust
struct Memo      { groups: Vec<MemoGroup>, query_roots: Vec<QueryRoot<MemoGroupId>>,
                   candidates: Vec<Vec<CandidateForMemoGroup>> }        // indexed by group
struct MemoGroup { logical_node, child_groups, parents, queries }
struct CandidateForMemoGroup { root_node, input_groups, fused_groups,
                               origin, accuracy_budget_parts }
```
Field-by-field roles: see the appendix.
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

1. `sets()`: find consumers that one producer could satisfy together, e.g. `topk(5, x)` and
   `topk(10, x)` by job.
2. `coverings()`: decide which producers to offer the set, e.g. `Limit(10) ← Sort(x)`.
3. `derive()`: for each consumer × covering, add a new candidate that reads the producer (now a
   group), e.g. `Limit(5) ← [ranking]`.

Each consumer's original candidates stay, so the MILP chooses between sharing and not sharing. The
number of copies added is consumers × coverings.

**(c)** `pass2/memo/enumeration.rs`: from the query roots, pick one candidate per reached group and
recurse into its inputs. For example, G2 and G3 both pick `Est<-G7`, and G7 picks its only candidate.

Measurements and today's types: see the appendix.

## 3. Proposed: consumers, requirements, unordered properties

Today's memo only avoids storing the same sub-expression twice. Most of what's left is repetition
of two kinds: choices that combine freely (any sketch with any width), and producers that differ
only in how much they can serve (a k=10 ranking also answers k=5). So we store each choice once,
and what each consumer needs once, and work out the combinations and the sharing only when we
enumerate.

### Overview

**(a) What is stored.** Today's groups and candidates, plus three additions:

1. Each candidate lists its **unordered properties**: choices with no cost order between them, with their
   legal values (sketch ∈ {Kll, DD}). One candidate instead of one per value.
2. Each group lists its **consumers** (the groups that read it), each with its **requirement**
   (G2 needs ε.01 from S). One entry per consumer instead of one candidate per possible producer.
3. One group per summary that queries could share, with no sketch fixed. It is an ordinary
   `MemoGroup`; only its `logical_node`, a new operator `SummaryState`, is new. In the example
   this is S, `SummaryState(quantile, data, 300s)`. Today the summary only appears as concrete
   groups with the sketch fixed (G5, G6, … in Sec 2).

```
G0 Scan
G1 TimeRange 300s <- G0
S  SummaryState(quantile, data, 300s)
     [SummaryAgg <-G1]       unordered: sketch ∈ {Kll, DD}
     [Merge←TWChain <-G0]    unordered: sketch ∈ {Kll, DD}, tumbling-window width ∈ {60s}
     consumers: G2 requires ε.01, G3 requires ε.001
G2 q50: [exact<-G1] [Estimate(.5)<-S]
G3 q99: [exact<-G1] [Estimate(.99)<-S]
```

**(b) Rules** . For example, the tumbling-window rule adds one candidate to S, `[Merge←TWChain<-G0]` with
width ∈ {60s}, instead of one candidate per width × sketch in every query's group.

**(c) Enumeration** picks one candidate per group, as today, except for groups with consumers.
For those, it decides which consumers share a producer. Each producer gets a value for each unordered
property and the requirement it is **built for**: the strictest of its consumers' requirements
(ε.001 when q50 and q99 share, ε.01 for q50 alone).

### (a) Representation: details

**Properties.** A producer first picks one of the group's candidates: a structural choice (its
operators and the groups it reads), not a property. A *property* is one thing the producer fixes
under that candidate.

- **Ordered property:** requirements only set a lower bound, and cost grows with the value, so
  its value is computed from `built_for`. E.g. ranking size = k.
- **Unordered property:** its values have no cost order, so they are enumerated and requirements
  filter them. E.g. Kll vs. DD.

| # | Property | Kind | Value |
|---|---|---|---|
| 1 | sketch kind | unordered | enumerated |
| 2 | sketch parameters | ordered | `size(kind, ε)` |
| 3 | tumbling-window width | unordered | enumerated; must divide lookback and evaluation interval |
| 4 | window count | ordered | lookback / width |
| 5 | ranking size | ordered | k |
| 6 | kept labels | ordered | labels |
| 7 | `ExecutionTiming` (`IngestionTime`, `QueryTime`) | unordered | enumerated |
| 8 | `Lifetime` (`None`, `OneEvaluation(Medium)`, `AcrossEvaluations(Medium)`) | unordered | enumerated |

7 and 8 are physical: together they decide materialization.

*Order of setting* (property → property): a property is set after its parent; the root is the
candidate.
```
candidate
├── 1 sketch kind ── 2 sketch parameters
├── 3 width ── 4 window count
├── 5 ranking size
├── 6 kept labels
└── 7 ExecutionTiming ── 8 Lifetime
```

*Which requirement fields constrain which properties* (requirement → property, many-to-many):

| `Requirement` field | 1 kind | 2 params | 3 width | 4 count | 5 k | 6 labels | 7 timing | 8 lifetime |
|---|---|---|---|---|---|---|---|---|
| query kind | ✓ | | | | | | | |
| ε | | ✓ | | | | | | |
| lookback | | | ✓ | ✓ | | | | ✓ |
| evaluation interval | | | ✓ | | | | | ✓ |
| k | | | | | ✓ | | | |
| labels | | | | | | ✓ | | |
| max latency | | | | | | | ✓ | |

Compared with today (Sec 2); `+` new, `~` changed:
```rust
struct Memo      { groups: Vec<MemoGroup>, query_roots: Vec<QueryRoot<MemoGroupId>> }
struct MemoGroup { logical_node,
~                  candidates: Vec<Candidate>,                     // moved from Memo
+                  consumers: Vec<(MemoGroupId, Requirement)> }    // empty if none
//                 child_groups, parents, queries: dropped, derivable (appendix)
~struct Candidate { root_node, input_groups, fused_groups,          // was CandidateForMemoGroup
+                  unordered: Vec<UnorderedProperty> }
//                 origin: dropped (debug only); accuracy_budget_parts: into Requirement.accuracy
+enum UnorderedProperty {           // each with its legal values
    Sketch(Vec<SketchKind>), TumblingWindowWidthMs(Vec<u64>),
    ExecutionTiming(Vec<ExecutionTiming>), Lifetime(Vec<Lifetime>) }
+enum Lifetime { None, OneEvaluation(Medium), AcrossEvaluations(Medium) }
+struct Requirement {               // what a consumer needs
    query_kind: Option<QueryKind>,  // e.g. quantile
    accuracy,                       // ε, with budget parts folded in
    lookback_ms,                    // how far back it reads: 300s for data[5m]
    evaluation_interval_ms,         // how often it is evaluated: 60s
    k: Option<u64>,                 // top-k only
    labels: Option<LabelSet>,       // rollup and scan widening only
    max_latency_ms: Option<u64>,
}
+struct Producer {                  // not stored: built during enumeration, (c)
    candidate: CandidateId,         // which of the group's candidates
    unordered: Vec<ValueOfUnorderedProperty>, // one per UnorderedProperty, e.g. Sketch(Kll)
    built_for: Requirement,         // e.g. ε.001; ordered properties are computed from it
}
+trait RequirementField: Sized {    // implemented by each Requirement field
    fn combine_into_strictest_requirement(a: &Self, b: &Self) -> Option<Self>;  // None: can't share
    fn can_producer_satisfy_consumer(built_for: &Self, candidate: &Candidate, consumer: &Self) -> bool;
}
+fn combine_into_strictest_requirement(requirements: &[Requirement]) -> Option<Requirement>
+fn can_producer_satisfy_consumer(producer: &Producer, consumer: &Requirement) -> bool
```

S's `logical_node` is a new logical operator, `SummaryState { kind, input, window }`: a summary
with no sketch or parameters fixed. Today's `SummaryAgg` can't be used because it fixes the
sketch.

- **Consumer:** a group that reads another group. G2 and G3 are consumers of S.
- **Requirement:** what a consumer needs from the group it reads. Where each field comes from:
  see the table below.
- **Producer:** a concrete instance of a group with consumers: a candidate, a value for each of its
  unordered properties, and the requirement it is built for. E.g. `Merge ← TWChain[5×60s] Kll(k for .001)`.
  - **Built for:** the strictest requirement of the consumers it serves. Not stored in the
    memo: it depends on which consumers share, which enumeration decides.

  For `S` built windowed with Kll for q50 and q99, `built_for` = (ε.001, 300s, 60s), so the
  ordered properties are sketch parameters = `size(Kll, ε.001)` and window count = 300s / 60s = 5.
- **`combine_into_strictest_requirement(requirements)`:** the strictest requirement among them,
  or none if they can't share. E.g. ε.01 and ε.001 give ε.001.
- **`can_producer_satisfy_consumer(producer, consumer)`:** the producer meets the consumer's
  requirement.

Both are global functions built from `RequirementField`, which each `Requirement` field
implements. The global combine works field by field, and is none if any field is. The global
check is the AND over fields; a field the consumer leaves unset (`None`) counts as true. A rule
only decides which fields its consumers set.

| `Requirement` struct field | Comes from | Set by rule | `combine_into_strictest_requirement` | `can_producer_satisfy_consumer` (built_for vs consumer) |
|---|---|---|---|---|
| query kind | query expression (`quantile_over_time` → quantile) | Pass 1 | equal, else none | the sketch kind supports it |
| ε | `QueryRequirements.accuracy`, split by budget parts | Pass 1 | min | ≤ |
| lookback | query expression, e.g. `[5m]` (`window_ms` today) | tumbling windows | max | ≥ for windowed candidates; = for direct ones |
| evaluation interval | query schedule (`evaluation_grid_ms()` today) | tumbling windows | gcd | divides |
| k | query expression | top-k | max | ≥ |
| labels | query expression | rollup, scan widening | union | ⊇ |
| max latency | `QueryRequirements.max_latency_ms` | Pass 1 | min | the `ExecutionTiming` answers within it |

The satisfy check takes the candidate because of lookback: a windowed producer over 10m serves a 5m
consumer by merging only the last 5 windows, but a direct `SummaryAgg` over 10m is one sketch of
all 10 minutes and can't.

### (b) Rules: details

Each rule takes the compressed representation and extends it. Notation, one group per line:
`G: [candidate] [candidate]  consumers: ...`, with unordered properties after `|` in a candidate.

**Pass 1** (`pass1/`). Adds S, its direct candidate and its consumers.
```
in:   G2 q50: [exact <-G1]
      G3 q99: [exact <-G1]
out:  G2 q50: [exact <-G1] [Estimate(.5) <-S]
      G3 q99: [exact <-G1] [Estimate(.99) <-S]
      S:      [SummaryAgg <-G1 | sketch ∈ {Kll, DD}]  consumers: G2 ε.01, G3 ε.001
```

**Tumbling windows** (`pass2/memo/window_candidates.rs`). Adds the windowed candidate and the
lookback and evaluation-interval requirement fields.
```
in:   S: [SummaryAgg <-G1 | sketch]  consumers: G2 ε.01, G3 ε.001
out:  S: [SummaryAgg <-G1 | sketch] [Merge←TWChain <-G0 | sketch, width ∈ {60s}]  consumers: G2 (ε.01, 300s, 60s), G3 (ε.001, 300s, 60s)
```

**Shared sizes and layouts** (`shared_configuration_readers.rs`, `state_groups.rs`). Deleted.
Today it adds one group per concrete summary: G5–G126 for three quantiles (G5–G6 direct,
G7–G126 windowed at 60 widths, each × Kll/DD). Unordered properties and producer options replace them;
see "Relation to `SharedProducer`" below.

**Top-k** (`topk.rs`), on `topk by (job) (5, x)` and `topk by (job) (10, x)`. Adds a ranking group
R that both read.
```
in:   T5:  [Limit(5) ← Sort <-X]
      T10: [Limit(10) ← Sort <-X]
out:  T5:  [Limit(5) ← Sort <-X] [Limit(5) <-R]
      T10: [Limit(10) ← Sort <-X] [Limit(10) <-R]
      R:   [Sort <-X]  consumers: T5 k=5, T10 k=10
```

**Rollup, finer aggregate** (`rollup.rs`, `finer_aggregate.rs`), on `sum by (job) (x)` and
`sum by (job, instance) (x)`. Adds an aggregate group A both read, at labels to be chosen.
```
in:   Aj:  [Sum by job <-X]
      Aji: [Sum by job,instance <-X]
out:  Aj:  [Sum by job <-X] [RollUp to {job} <-A]
      Aji: [Sum by job,instance <-X] [RollUp to {job,instance} <-A]
      A:   [Sum <-X]  consumers: Aj {job}, Aji {job,instance}
```
`RollUp` to the producer's own labels is a no-op. Only for aggregations that roll up.

**Scan widening** (`scan_labels.rs`), on `sum by (job) (m)` and `sum by (instance) (m)`. Adds a scan
group M both read.
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

Enumeration emits every logical DAG the memo represents. It walks from the query roots and picks
one candidate per group (step 1). At a group with consumers it instead decides the producers and
who reads which (step 2), then continues below each producer's candidate.

```
for chosen in choose_candidates(memo):                                   # step 1
  for each combination of one Assignment per group with consumers in chosen: # step 2
    emit the DAG
```

Step 1 covers every group without consumers: the query roots, and groups such as G0 and G1 that no
rule gave consumers (see open question 6).

**Step 1.** `fn choose_candidates(memo: &Memo) -> Vec<HashMap<MemoGroupId, CandidateId>>`
- Input: the memo.
- Output: every choice of one candidate per reached group, as today, except that groups with
  consumers get no entry; step 2 handles them.
- Example output, one of 4 (each query exact or reading S):
  `{G2: Estimate(.5) <-S, G3: Estimate(.99) <-S}`.

**Step 2.** `fn assignments(memo: &Memo, chosen: &HashMap<MemoGroupId, CandidateId>, group: MemoGroupId) -> Vec<Assignment>`
```rust
type Assignment = Vec<(Producer, Vec<MemoGroupId>)>;   // each producer, with the consumers reading it
```
- Input: the memo, one output of step 1, and one group with consumers that it reached.
- Output: every way to build that group and serve its consumers.
- Example: `assignments(memo, {G2: Estimate(.5) <-S, G3: Estimate(.99) <-S}, S)` returns 20
  `Assignment`s:
  ```
  4 like  [(P1 built for ε.001, [G2, G3])]                         both share one producer
  16 like [(P1 built for ε.01, [G2]), (P2 built for ε.001, [G3])]  each has its own
  ```
  4 = P1's candidate (direct or windowed) × sketch (Kll or DD). 16 = 4 for P1 × 4 for P2.

**Implementing `assignments`.** Two options. Both take C, the consumers of the group that read
it under `chosen`, and decide which consumers share a producer and what requirement it is built
for. Both then call the helper `finish`, specified below them.

*Option 1: split then combine.* Enumerate every set partition of C (their number is the Bell
number). Each part gets one producer, built for the strictest requirement in that part. {G2, G3}
has two partitions: {G2, G3} (shared) and {G2}, {G3} (separate).
```
C = [(g, r) in memo.groups[group].consumers  if group in chosen[g].input_groups]
for each partition of C into parts:
  finish([ (combine_into_strictest_requirement([r for (_, r) in part]),   # built for
            [g for (g, _) in part])                                       # its readers
           for part in partition ])
```

*Option 2: pick then group* (proposed). List the **producer options**: start from the consumers'
requirements and keep adding the strictest requirement of any two options until nothing new
appears; that gives every distinct strictest requirement over every subset of C (here
{ε.01, ε.001}). Then try every way to give each consumer one option (`pick`). The consumers that
picked the same option o (`readers[o]`) share one producer built for o.
Requirements are only partially ordered, field by field: (ε.001, 5m) and (ε.01, 10m) are
incomparable, and their strictest combination (ε.001, 10m) is a third option.
```
C = [(g, r) in memo.groups[group].consumers  if group in chosen[g].input_groups]
options = {r for (_, r) in C}
repeat until no change:
  options += { combine_into_strictest_requirement([a, b]) for a, b in options }
for each pick in (every map from consumers in C to one option each):    # |options|^|C| maps
  readers[o] = [g for g in C if pick[g] == o]
  skip if any o != combine_into_strictest_requirement(requirements of readers[o])   # overbuilt
  finish([ (o, readers[o]) for each picked o ])
```
A producer is *overbuilt* when it is built for a stricter requirement than all its readers need,
e.g. q50 alone picks ε.001. It costs more and serves no one extra, so it is skipped.

*Helper `finish`*, used by both: gives each producer a candidate of the group and a value for
each of its unordered properties, and keeps an assignment only if every producer satisfies its readers.
```
finish(producers: [(built_for, readers)]):
  for each producer, each candidate c of group, each combination of values for c.unordered:
    p = Producer { candidate: c, unordered: values, built_for }
    if can_producer_satisfy_consumer(p, r) for every reader r of p:
      keep
  emit one Assignment per combination of kept choices across the producers
```

Option 2 yields option 1's assignments minus dominated ones: two producers built for the same
requirement, which cost more than one shared. The number of partitions is the Bell number (15 for 4 consumers,
115,975 for 10). Options stay short (at most one per distinct ε here) and each consumer picks independently,
so a later stage can keep sharing compressed.

**Enumeration output on the running example: 29 DAGs.**

| Step 1 output: G2 (q50), G3 (q99) | Step 2 output: `Assignment` of S | DAGs |
|---|---|---|
| exact, exact | – | 1 |
| reads S, exact | P1 built for ε.01 ← q50 | 4: P1's candidate × sketch |
| exact, reads S | P1 built for ε.001 ← q99 | 4 |
| reads S, reads S | P1 built for ε.001 ← q50, q99 | 4 |
| reads S, reads S | P1 built for ε.01 ← q50; P2 built for ε.001 ← q99 | 16: 4 for P1 × 4 for P2 |
| **Total** | | **29** |

Two of the 29 DAGs written out, one shared and one separate:
```
shared:   P1 = Merge ← TWChain[5×60s] Kll(k for .001) ← Scan
          q50: Estimate(.5) ← P1        q99: Estimate(.99) ← P1
separate: q50: Estimate(.5)  ← SummaryAgg DD(α for .01) ← TimeRange ← Scan
          q99: Estimate(.99) ← Merge ← TWChain Kll(k for .001) ← Scan
```

The memo stores 2 candidates per query, 2 candidates of S, their unordered properties and 2 consumers'
requirements, never the 29 DAGs. For three quantiles at one ε, today's 127 groups / 247
candidates per root become 6 groups / 2 candidates per root.

## 4. End-to-end example: today vs. proposed, step by step

The running example (q50 ε.01, q99 ε.001, `data[5m]` every 60s). Today's side is from a real run:
`stage_dump/q2_running_example/` (`stage_pipeline --max-memo-candidates 1000`, printed with
`print_stages.py`). Which rule added a candidate is read from its `origin` and sketch size.
Proposed is hand-written. Legal tumbling-window widths are the 60 divisors of 60s (1ms … 60s);
only 60s is written out.

Notation: `Est` = `Project ← SummaryEstimate`; `TWC[n×w]` = `SummaryTumblingWindowChain` of n
windows of width w; `budget_parts` is 1 throughout and omitted.

### Step 0: Stage 0

Both: four groups, no candidates yet.
```
G0  Scan
G1  TimeRange 300s          <- G0
G2  quantile(.5)            <- G1      root of q50
G3  quantile(.99)           <- G1      root of q99
```

### Step 1: Pass 1

Today: each query gets three candidates, the sketch sized for that query's ε.
```
G2  quantile(.5)                                     root of q50
    candidates:
      [0]  Aggregate quantile(.5)           <- G1    origin: pass1
      [1]  Est(.5) ← SummaryAgg Kll(k=567)  <- G1    origin: pass1
      [2]  Est(.5) ← SummaryAgg DD(α=.01)   <- G1    origin: pass1
G3  quantile(.99)                                    root of q99
    candidates:
      [0]  Aggregate quantile(.99)            <- G1  origin: pass1
      [1]  Est(.99) ← SummaryAgg Kll(k=6045)  <- G1  origin: pass1
      [2]  Est(.99) ← SummaryAgg DD(α=.001)   <- G1  origin: pass1
```
4 groups · 3 candidates per root.

Proposed: one new group S; each query gets one candidate reading it.
```
G2  quantile(.5)                                     root of q50
    candidates:
      [0]  Aggregate quantile(.5)  <- G1
      [1]  Est(.5)                 <- S
G3  quantile(.99)                                    root of q99
    candidates:
      [0]  Aggregate quantile(.99)  <- G1
      [1]  Est(.99)                 <- S
S   SummaryState(quantile, data, 300s)
    candidates:
      [0]  SummaryAgg  <- G1
           unordered:
             Sketch ∈ {Kll, DD}
    consumers:
      G2  requires { ε: .01 }
      G3  requires { ε: .001 }
```
5 groups · 2 candidates per root.

### Step 2: Tumbling windows (Pass 2, rule 1)

Today: a windowed copy of each sketch candidate per width × sketch.
```
G2  quantile(.5)
    candidates:
      [0..2]     as in step 1
      [3..122]   120 new, origin: tumbling_windows, one per width × sketch:
        [3]    Est(.5) ← SummaryMerge ← TWC[300000×1ms] Kll(k=567)  <- G0
        [4]    Est(.5) ← SummaryMerge ← TWC[300000×1ms] DD(α=.01)   <- G0
        …
        [121]  Est(.5) ← SummaryMerge ← TWC[5×60s] Kll(k=567)       <- G0
        [122]  Est(.5) ← SummaryMerge ← TWC[5×60s] DD(α=.01)        <- G0
G3  quantile(.99)
    candidates:
      [0..2]     as in step 1
      [3..122]   the same at Kll(k=6045), DD(α=.001)
```
4 groups · 123 candidates per root.

Proposed: one candidate on S; the consumers' requirements gain lookback and interval.
```
S   SummaryState(quantile, data, 300s)
    candidates:
      [0]  SummaryAgg  <- G1
           unordered:
             Sketch ∈ {Kll, DD}
      [1]  SummaryMerge ← TWChain  <- G0                         new
           unordered:
             Sketch ∈ {Kll, DD}
             TumblingWindowWidthMs ∈ {1, 2, …, 60000}            60 values
    consumers:
      G2  requires { ε: .01,  lookback: 300s, interval: 60s }
      G3  requires { ε: .001, lookback: 300s, interval: 60s }
```
5 groups · 2 candidates per root · 2 candidates of S.

### Step 3: Shared configurations (Pass 2, rule 9)

Today: `SharedProducer` for summaries. The set is {q50, q99}; the covering is the sketch sized for
ε.001; `derive()` copies q50's candidates at that size.
```
G2  quantile(.5)
    candidates:
      [0..122]    as in step 2
      [123]       Est(.5) ← SummaryAgg Kll(k=567)   <- G1    duplicate of [1]
      [124]       Est(.5) ← SummaryAgg Kll(k=6045)  <- G1
      [125]       Est(.5) ← SummaryAgg DD(α=.001)   <- G1
      [126..245]  120 × Est(.5) ← SummaryMerge ← TWC[n×w] {Kll(k=6045), DD(α=.001)}  <- G0
G3  quantile(.99)
    candidates:
      [0..122]    as in step 2
      [123]       Est(.99) ← SummaryAgg Kll(k=6045)  <- G1   duplicate of [1]
```
4 groups · G2 246, G3 124 candidates.

Proposed: nothing. The consumer list already says who could share; the size follows from
`built_for` in enumeration.

### Step 4: Shared summary state groups (Pass 2, rule 10)

Today: every concrete summary build becomes its own group, and each query gets a candidate reading
each one (origin: shared_build).
```
G4    SummaryAgg Kll(k=567)            <- G1    read by G2
G5    SummaryAgg Kll(k=6045)           <- G1    read by G2, G3
G6    SummaryAgg DD(α=.001)            <- G1    read by G2, G3
G7    TWC[300000×1ms] Kll(k=6045)      <- G0    read by G2, G3
…                                               60 widths × {Kll(k=6045), DD(α=.001)}
G66   TWC[5×60s] Kll(k=6045)           <- G0    read by G2, G3
…
G126  TWC[5×60s] DD(α=.001)            <- G0    read by G2, G3
G2  quantile(.5)
    candidates:
      [0..245]    as in step 3
      [246]       Est(.5)                  <- G4
      [247]       Est(.5)                  <- G4    differs from [246] only in guarantee
      [248]       Est(.5)                  <- G5
      [249]       Est(.5)                  <- G6
      [250..369]  Est(.5) ← SummaryMerge   <- G7 … G126
G3  quantile(.99)
    candidates:
      [0..123]    as in step 3
      [124..246]  the same reading G5 … G126
```
127 groups · G2 370, G3 247 candidates.

Proposed: nothing. S is already the one group for this summary.

### Step 5: Enumeration

Today: pick one candidate per reached group. The run's Stage 1 output (L1):
```
G2  Est(.5)                 <- G5
G3  Est(.99)                <- G5
G5  SummaryAgg Kll(k=6045)  <- G1      one shared direct Kll
G1  TimeRange 300s          <- G0
G0  Scan
```

Proposed: step 1, then step 2 (Sec 3(c)).
```
step 1 output (one of 4):
  G2  Est(.5)   <- S
  G3  Est(.99)  <- S
step 2 output for S (one of 15,006 = 122 shared + 122² separate):
  P1  candidate:  [0] SummaryAgg <- G1
      unordered:
        Sketch = Kll
      built_for:  { ε: .001, lookback: 300s, interval: 60s }
      read by:    G2, G3
```
The same DAG as today's L1: P1 is `SummaryAgg Kll(k for .001)`, read by both. With width
∈ {60s} there are 29 DAGs (Sec 3(c)); with all 60 widths each producer has 2 + 2 × 60 = 122
choices, so 1 + 3 × 122 + 122² = 15,251.

### Summary

| After | Today: groups · candidates (G2, G3) | Proposed: groups · candidates (G2, G3, S) |
|---|---|---|
| Stage 0 | 4 · – | 4 · – |
| Pass 1 | 4 · 3, 3 | 5 · 2, 2, 1 |
| Tumbling windows | 4 · 123, 123 | 5 · 2, 2, 2 |
| Shared configurations | 4 · 246, 124 | unchanged |
| Shared state groups | 127 · 370, 247 | unchanged |

## 5. Stage 3 over the proposed memo

Stage 3 chooses along three axes:

| Axis | Scope | Stored as |
|---|---|---|
| Which candidate | within a group | `MemoGroup.candidates` |
| Which unordered-property values | within a candidate | `Candidate.unordered` |
| Which consumers share a producer | across groups | `MemoGroup.consumers` + producer options |

As a facility-location problem:
```
x[g,c]  group g uses candidate c
y[r,p]  consumer r reads a producer built for option p
z[p]    producer p is built  ⇐  any y[r,p]
```

Sketch parameters are not an axis. They follow from `built_for`, `size(sketch, ε)`, set after
sharing. Stage 3 sees them either as discrete values (one per producer option) or as a
constraint `k ≥ k_min(r) · y[r,p]`. The constraint form needs a cost that is linear in k (Kll
roughly is; DD in 1/α).

## 6. How to extend

None of these touches `assignments`, `finish` or the enumeration loop.

**Add a rule.**
1. If it adds sharing, create the shared group with its `logical_node` (like S, R).
2. Add the candidates that read it.
3. Register its consumers, each with a `Requirement` that sets only the fields the rule uses.

E.g. top-k adds R, `[Limit(5) <-R]` to T5, `[Limit(10) <-R]` to T10, and consumers T5 k=5, T10 k=10.

**Add a requirement field.**
1. Say where its value comes from (query, schedule, or reader) and which rule sets it.
2. Implement `RequirementField`: combine and satisfy.
3. Add a row to the requirement → property matrix.

**Add a property.**
1. Place it in the order of setting (its parent).
2. Mark the requirement fields that constrain it in the matrix; add a field if none fits.
3. Ordered if one field orders it and a stricter value never costs less: write its value as a
   function of `built_for`. No Stage 3 variable. E.g. ranking size = k.
4. Otherwise unordered: add an `UnorderedProperty` variant; the rule that creates the candidate
   sets its legal values. One more Stage 3 axis. E.g. `Lifetime`.

## Implications

- Smaller memo and dumps. The private-chain copies and the guarantee duplicates go away, because
  the guarantee is computed per (producer, consumer) instead of being part of node identity.
- The search shrinks only where costs separate: a cost that depends on width × sketch still needs a
  variable per (width, sketch) pair.
- Correctness check: decode both designs on small workloads and compare the sets of DAGs, using
  `enumeration.rs` as the reference.

## Open questions

1. Do `topk`, `rollup`, `finer_aggregate` and `scan_labels` really fit `Requirement`, with the
   combine and satisfy rules in (a)?
2. Where do factored candidates get expanded? (i) Just before Stage 2: small change, but the MILP
   stays large and #1197 isn't fixed. (ii) Keep them factored into the MILP (#1201): fixes #1197, but
   a larger change. This also decides where producer options are
   computed: at the end of Stage 1 in (i), while building the MILP in (ii).
3. Can a producer's cost depend on its consumers (query-time hold time, evaluation alignment)? If
   so, the model becomes facility cost + per-assignment cost.
4. Should multi-operator private chains give way to one operator per candidate (classic Cascades)?
5. Sketch parameters in Stage 3: discrete values per producer option, or a `k ≥ …` constraint?
6. Should every group list its consumers (G1: G2, G3, S with empty requirements; each root: its
   query)? Then step 1 disappears and `assignments` runs on every group, but every rule must
   register its reads.
7. Is tumbling-window width ordered? The cheapest width that divides every reader's interval and
   lookback is their gcd (#1171), if a narrower width is never cheaper. Then each producer in Sec 4
   has 4 choices, not 122.
8. Is the order of setting a tree or a DAG? Possible extra parents: `Lifetime` on width (a
   window lives until it leaves the lookback); `ExecutionTiming` on sketch kind (if some sketch can't be built at
   ingestion).

## Glossary

| Term | Meaning |
|---|---|
| Group | one distinct logical sub-expression; equal expressions across queries share one |
| Candidate | one way to implement a group: its own operators + the groups it reads |
| Consumer | a group that reads another group, with its requirement |
| Requirement | what a consumer demands (query kind, ε with budget parts, lookback, evaluation interval, k, labels, max latency); from the query's requirements, expression and schedule |
| Producer | a concrete instance of a group with consumers: a candidate + unordered-property values + `built_for` |
| Property | one thing a producer fixes (Sec 3(a) table) |
| Ordered property | requirements only set a lower bound, so its value is computed from `built_for` (ranking size = k) |
| Unordered property | values with no cost order, enumerated and filtered by requirements (Kll vs. DD) |
| Built for | the requirement a producer is built to: the strictest of its consumers' |
| `combine_into_strictest_requirement(requirements)` | the strictest requirement among them, or none if they can't share |
| `can_producer_satisfy_consumer(producer, r)` | the producer meets requirement r |
| Subsumes | one producer satisfies every consumer another does |
| Producer options | every distinct strictest requirement over subsets of a group's consumers: the requirements a producer may be built for |
| Assignment | per group with consumers: the producers to build, each with the consumers that read it |
| Sketch parameters | Kll k, DD α: `size(sketch, built_for.ε)`, not a choice |
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
