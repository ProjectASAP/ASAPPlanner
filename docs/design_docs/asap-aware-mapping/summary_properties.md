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

## Mergeability

Can two independently maintained summaries be combined to produce a summary of their union?

Mergeability enables:

- distributed aggregation,
- time-bucket merging,
- group-by roll-up,
- hierarchical summaries,
- and some forms of shared computation.

## Subpopulation Awareness

Can one summary represent multiple subpopulations and answer queries for individual subsets?

This determines whether shared multi-subpopulation designs are possible.

## Subtractability

Can one summary be removed from another?

Conceptually:

```text
Summary(A ∪ B) - Summary(B) → Summary(A)
```

Subtractability is useful for sliding windows, dynamic partitions, and incremental maintenance.


## Deletion Support

Can individual items be removed from a summary?

Deletion support may be necessary for:

- sliding windows,
- corrections,
- mutable datasets,
- and retractions.

This is distinct from subtracting one complete summary from another.

## Time Awareness

Does the summary natively model time or window semantics?

A time-aware summary may support operations that are difficult or expensive using a time-agnostic sketch.

## Linearity

Can the summary participate in linear combinations or related algebraic operations?

Linearity may enable composition, subtraction, hierarchical aggregation, or recovery of related statistics.

## Composability

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

## Accuracy Under Continued Insertion

Does the summary's error behavior change as more items are inserted?

Some summaries provide guarantees largely independent of stream length, while others may degrade or require resizing.

The planner needs this information when selecting long-lived summaries.
