# Current-series population candidates

`CurrentSeriesStrategy` is an opt-in `ReplacementStrategy` for deployments that
can continuously maintain complete PromQL current-series inputs. It matches
canonical cross-series quantile aggregates and descending value Sort/Limit over
a direct open time-series Scan. Temporal ranges, shifted selectors, nested inputs
and bottom-k do not match.

The selected post-ASAP DAG is:

```text
KeepPreAsap(Scan) [maintenance rows]
  -> ValueOperation::MaintainCurrentSeries(population) [maintenance rows]
  -> ValueOperation::ReadCurrentSeries(quantile q | top-k k) [read rows]
```

The population owns source, label predicates, grouping and five-minute selector
lookback semantics. Updates replace each series' latest value; stale markers and
expiry retract it. It retains the full population, not only the largest k values.
Compatible workload consumers use the largest requested k and one quantile
population; canonical CSE shares their maintenance producer. Different sources,
matchers or groups do not share. The readout remains exact.

IR validation checks the maintenance input against the population contract,
phase placement and the readout's compatibility with its producer. The only new
maintenance-row/read-row bridge is this explicit producer/readout pair.

The strategy does not parse query text, choose a deployment, assign byte budgets
or claim a performance benefit. Backends lower these typed operations into their
state implementation, bind resource/coverage limits and price build, update,
residency, retirement and readout costs. Other deployments must keep it disabled
until they support that contract. Existing default strategy selection is unchanged.
