# Summary properties to model

To determine whether transformations are valid, ASAP-aware mapping needs a common description of summary capabilities.

Important properties include:
- Mergeability
- Subpopulation-awareness
- Subtractability
- Ability to delete an item
- Time awareness
- Linearity
- Composability
- Accuracy trends

### Mergeability

Can two independently maintained summaries be combined to produce a summary of their union?

This enables distributed aggregation, time-bucket merging, group-by roll-up, hierarchical summaries, and some forms of shared computation.

### Subpopulation Awareness

Can one summary represent multiple subpopulations and answer queries for individual subsets?

This determines whether shared multi-subpopulation designs are possible.

### Subtractability

Can one summary be removed from another?

Conceptually:

```text
Summary(A ∪ B) - Summary(B) → Summary(A)
```

Subtractability is useful for sliding windows, dynamic partitions, and incremental maintenance.


### Deletion Support

Can individual items be removed from a summary?

Deletion support may be necessary for:

- sliding windows,
- corrections,
- mutable datasets,
- and retractions.

This is distinct from subtracting one complete summary from another.

### Time Awareness

Does the summary natively model time or window semantics?

A time-aware summary may support operations that are difficult or expensive using a time-agnostic sketch.

### Linearity

Can the summary participate in linear combinations or related algebraic operations?

Linearity may enable composition, subtraction, hierarchical aggregation, or recovery of related statistics.

### Composability

Can a summary serve as the input to another logical or summary operator?

For example:

```text
raw data
   ↓
summary A
   ↓
summary B
```

or:

```text
fine-grained summaries
        ↓
      roll-up
        ↓
coarser summary
```

Composability determines which multi-stage candidate plans are legal.

### Accuracy Under Continued Insertion

Does the summary's error behavior change as more items are inserted?

Some summaries provide guarantees largely independent of stream length, while others may degrade or require resizing.

The planner needs this information when selecting long-lived summaries.

## End-to-end guarantees for nested summaries

A single summary is sized against its own `AccuracyTarget`, but a summary
over another summary's readout is legal only if the composed error still
meets the requirement on the outer value. Legality is established before
costing:

```text
candidate generation
    -> guarantee propagation            (AccuracyModel::propagate)
    -> AccuracyTarget satisfaction      (AccuracyModel::satisfies)
    -> legal candidates only            (illegal ones -> MemoGroup::rejected)
    -> cost ranking / global selection  (CostModel)
```

Every finalized post-ASAP value carries a machine-readable
`ResultGuarantee`: a typed error metric, symbolic bound and probability
expressions, and provenance. Exact values have zero error; a sketch
readout's guarantee is derived from the sizing formula that produced its
parameters.

The built-in sketch contracts distinguish their error norms and confidence
semantics:

- CMS uses an L1-frequency bound, while CountSketch uses an L2-frequency
  bound and is sized from both width and odd median depth. The two are not
  interchangeable.
- KLL, KMV, and Theta expose identified 99%-confidence contracts derived from
  committed parameters. HLL retains its RSE magnitude but has unknown failure
  probability because its current parameters encode precision, not a
  confidence-level budget; it therefore cannot satisfy `EpsilonDelta`.
- TopK membership is certified only when the widened confidence interval of
  the kth selected item is strictly above every excluded item's widened
  interval. Missing or overlapping interval evidence fails closed.
- Hydra adds its shared-grid collision error to the inner sketch error and
  union-bounds their failure probabilities. A typed evidence provider may
  instantiate the shared-grid terms; without it they remain symbolic and an
  accuracy-targeted Hydra candidate is not admitted.

The default `AccuracyModel` is deliberately conservative. It supports
registered same-metric additive, relative, and Lipschitz rules and exact
sum/max/min over approximate inputs. It uses union-bound probabilities
without assuming independence, preserves unknown statistics as unknown,
and rejects unsupported composition instead of treating the child as exact.
An `AccuracyBudgetAllocator` can propose resized layers; the `CostModel`
ranks only candidates that satisfy the accuracy requirement.
