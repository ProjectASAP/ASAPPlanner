# Summary coverage contract

`Schema` describes an edge's field layout and committed state type. It does not
say which observations a summary state was built from: a KLL over `[0,1)` and a
KLL over `[1,3)` have equal schemas. `OperatorNode.coverage` records that
separately. It is required on summary nodes (`SummaryAgg`, and `SummaryMerge`,
which derives it): `validate_structure` rejects them with `CoverageError::Missing`
when it is `None`. Other nodes leave it `None`. Rewriting a node's inputs clears
it, so a rewriter must declare it again with `with_coverage`.

`SummaryCoverage` records:

- `source`: the observation data source. It can be any tabular data, not
  necessarily a time series; region time bounds refer to its time column.
- `input`, `reduction`: must equal the producing `SummaryAgg`'s fields of the same
  name. `with_coverage` checks this and requires state output.
- `regions`: a union of `CoverageRegion`s. Each pairs optional half-open time
  bounds in milliseconds (`None`: no time restriction, e.g. a source without a
  time column) with a conjunction of non-null equality predicates over population
  dimensions; an empty predicate map means all observations of the source.

Every observation in a region contributes once to the state.

## Trusted declarations

Coverage is declared by the composition rule or catalog that built the subtree.
Only `input` and `reduction` are checked against the producer. Population and
time bounds are trusted: population is not compared with `SummaryAgg.filter`,
`Filter` nodes or `Scan.predicates`, and `TimeRange` is relative, so absolute
bounds cannot be checked. A wrong declaration therefore passes:

```text
A = SummaryAgg(filter: region='us'), declared {region: eu} × [0,1)   ← wrong
B = SummaryAgg(filter: region='us'), declared {region: us} × [0,1)
merge_disjoint(A, B) is accepted, and every US observation is counted twice.
```

Issue #570 tracks checking population against the subtree's predicates.

## Merging

`merge_disjoint` requires equal source/input/reduction and provably disjoint
regions. Adjacent intervals coalesce only with identical population predicates;
gaps remain separate. Conflicting values for the same dimension prove disjointness.
Different dimensions do not: `region=us` can overlap `tier=premium`. A region
without time bounds overlaps every region it is not population-disjoint from.

`us×[0,1)` merged with `eu×[1,2)` remains two regions, not `{us,eu}×[0,2)`.
Empty `regions` is known empty coverage. Empty merge input is invalid.

Not covered: arbitrary or null predicates, idempotent set-union families, and
checking that coverage contains a requested query window or population. The
contract provides no runtime merge capability, accuracy certificate, storage
policy or execution timing.
