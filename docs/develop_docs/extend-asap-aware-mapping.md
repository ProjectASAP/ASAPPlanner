# Extend ASAP-aware mapping

Start with the [code architecture](asap-aware-mapping-architecture.md) and
[shared contracts](asap-aware-mapping-contracts.md). Choose the task matching
the behavior you need to change:

| Task | Guide |
| --- | --- |
| Add a logical rewrite or replacement source | [Add a replacement strategy](add-replacement-strategy.md) |
| Add an algorithm, its readout and accuracy contract | [Add a summary algorithm](add-summary-algorithm.md) |
| Change preferences, sizing or cost evidence | [Customize a cost model](customize-cost-model.md) |
| Report a new candidate kind | [Extend explanations](extend-explanations.md) |
| Embed Planner and consume ranked candidates | [Library API](library-api.md) |
| Analyze frontend corpora and coverage | [Corpus verification](corpus-verification.md) |

Keep legality, preference, and reporting responsibilities separate. For changes
spanning several components, follow each applicable guide and test the full path
from a frontend query through search to its accepted result or rejection.
