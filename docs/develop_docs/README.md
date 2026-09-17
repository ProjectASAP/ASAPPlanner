# Developer documentation

Use these pages to integrate Planner, change its implementation, or validate
behavior. The [system design](../design_docs/architecture/README.md) explains
ownership; [user guides](../user_guide_docs/README.md) cover query inspection.

## Integrate the library

- [Public library functions and examples](library-api.md)
- [Mapping code architecture](asap-aware-mapping-architecture.md)
- [Mapping contracts](asap-aware-mapping-contracts.md)

## Change planner behavior

- [Extension task index](extend-asap-aware-mapping.md)
- [Add a replacement strategy](add-replacement-strategy.md)
- [Add a summary algorithm](add-summary-algorithm.md)
- [Customize a cost model](customize-cost-model.md)
- [Extend explanations](extend-explanations.md)

## Reference and verification

- [Pre-ASAP IR](pre-asap-ir.md) and [Post-ASAP IR](post-asap-ir.md)
- [Physical operators and evidence](physical-operator-reference.md)
- [Physical boundary costs](physical-boundary-costs.md) and [storage operations](storage-operation-costs.md)
- [Accuracy implementation companion](../design_docs/proposals/asap-aware-mapping/end-to-end-accuracy-guarantees-developer-guide.md)
- [DDSketch ratio certification](../design_docs/proposals/asap-aware-mapping/ddsketch-quantile-ratios.md)
- [Offline sketch evidence](offline-sketch-evidence.md)
- [Replacement explanations](replacement-explanations.md)
- [Corpus verification](corpus-verification.md) and [corpus sources](metrics-observability-corpora.md)
