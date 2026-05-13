# Checklist for May 05 Synchronization

This checklist is used to check if all we have discussed in the May 05 synchronization meeting has been captured in the README and design docs.

## Last Week's Action Items

### Design Checklist:

If supported, do we have examples?
1. Is precomputation considered?
    - **Not included in V1**
2. Is batched query considered? Do we support spatial reuse?
    - Define the DAG as the output of L3 and L4 (Reuse in both layers)
    - **Example from Zeying.**
3. Is query arrival pattern considered? Do we support temporal reuse?
    - **Example.**
4. Do we have a cost model?
    - How different are the cost models across different platforms? Do we need a unified cost model?
    - We should at least need some feeling of "cost" for L4. **L4 only output candidates.**
5. Is deployment mode (distribution + data life cycle) considered?
    - No
6. Is platform-specific optimization considered? (e.g., GPU vs CPU, in-memory vs disk-based, etc.)
    - No
7. Is accuracy guarantee considered?
    - Yes
8. Can we cover existing ASAP artifacts with this formalization?
    - We can not but we can use V1 results to cover part of it.

### Scope of V1 Controller

0. Query with subpopulation, look-back window, TopK all supported
1. Batched query supported
2. Repeating query supported
3. DAG as output
4. Accuracy requirements enforced (Theoretical estimate only)
5. V1 output candidates, no guarantee of optimality
6. Other costs NOT included
7. Deployment and platform specific stuff NOT included

### TBD(efined)

1. Controller input
2. L3 output / L4 input
3. L4 output

### TBD(one)

1. Zeying shows 2 examples
2. We try to define the interfaces in code level
