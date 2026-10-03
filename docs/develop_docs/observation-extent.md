# Summary coverage contract

Schema describes field layout; summary coverage describes eligible observations.
`OperatorNode.observation_extent` is optional logical metadata. `None` means unknown,
not unrestricted coverage. Rewriting inputs clears it along with other assessed
metadata. `with_observation_extent` validates declared coverage and checks state kind
and SummaryAgg input/grouping agreement. Provenance is supplied by a trusted
composition rule/catalog; this API does not infer predicates from arbitrary SQL.

`ObservationExtent` records source and revision identity, update expression,
grouping, once-per-observation multiplicity and a union of joint `ExtentRegion`s.
Each region pairs half-open time bounds in milliseconds with a conjunction of
non-null equality predicates over canonical population dimensions. Source identity
must include the time axis and observation-identity namespace. Revision identifies
the input snapshot/update contract used to construct the state.

`merge_disjoint` requires equal input identities and provably disjoint joint
regions. Adjacent intervals coalesce only with identical population predicates;
gaps remain separate. Conflicting equality predicates on the same dimension prove
population disjointness. Independent predicates do not: region=US can overlap
tier=premium. Different source/revision/input/grouping contracts fail.

US×[0,1) merged with EU×[1,2) remains two regions, not
{US,EU}×[0,2). This avoids inventing missing cross-population/time coverage.
Empty regions describe empty observation coverage. Empty merge input is invalid.

This contract supports conservative once-per-observation composition. Arbitrary
predicates, null predicates, unbounded time coverage, idempotent set-union algebra,
coverage inference and full requested-window containment need explicit extensions.
It never labels unsupported/unknown predicates disjoint. It provides no runtime
merge capability, accuracy certificate, storage policy or execution timing.

The following merge PR must require known coverage, derive the output union and
validate it rather than treating matching schemas as sufficient authorization.
Logical transport and CSE must preserve and compare coverage metadata.

## Why extent

`ObservationExtent` describes the declared set of source observations represented
by a state. The name borrows the set meaning of "extent" from object databases;
it is a project-specific term, not an ODMG class extent implementation.
`None` means unknown extent; an empty region list means a known empty extent.
Disjoint union preserves gaps and joint population/time relationships.
The later query-relative coverage check asks whether this extent satisfies a
requested population/window. Declaring an extent does not prove that check.
