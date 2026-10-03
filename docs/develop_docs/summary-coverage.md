# Summary coverage contract

`Schema` describes an edge's field layout and committed state type. It does not
say which observations a summary state was built from: a KLL over `[0,1)` and a
KLL over `[1,3)` have equal schemas. `OperatorNode.coverage` records that
separately, as optional logical metadata. `None` means unknown, not unrestricted.
Rewriting a node's inputs clears it along with other assessed metadata.

`SummaryCoverage` records:

- `source`: the observation stream, including its time axis.
- `input`, `reduction`: must equal the producing `SummaryAgg`'s fields of the same
  name. `with_coverage` checks this and requires state output.
- `regions`: a union of `CoverageRegion`s. Each pairs half-open time bounds in
  milliseconds with a conjunction of non-null equality predicates over population
  dimensions; an empty predicate map means all observations of the source.

Every observation in a region contributes once to the state. Declarations come
from trusted composition rules or catalogs; nothing is inferred from SQL.

`merge_disjoint` requires equal source/input/reduction and provably disjoint
regions. Adjacent intervals coalesce only with identical population predicates;
gaps remain separate. Conflicting values for the same dimension prove disjointness.
Different dimensions do not: `region=us` can overlap `tier=premium`.

`us×[0,1)` merged with `eu×[1,2)` remains two regions, not `{us,eu}×[0,2)`.
Empty `regions` is known empty coverage. Empty merge input is invalid.

Not covered: arbitrary or null predicates, unbounded time, idempotent set-union
families, and checking that coverage contains a requested query window or
population. The contract provides no runtime merge capability, accuracy
certificate, storage policy or execution timing.
