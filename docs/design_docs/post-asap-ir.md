# Post-ASAP IR

The goal of the post-ASAP IR is to primarily represent operations using ASAP primitive such as sketches and wavelets. Post-ASAP IR will also contain nodes from pre-ASAP IR since only some pre-ASAP IR nodes can be satisfied using ASAP primitives.
Much (all?) of this design is motivated by sketches. As we integrate other primitives, we can update this.

## ASAP-specific nodes operated over a summary structure, not raw data

- SummaryCreate: create a summary from raw data
- SummaryInsert: insert a raw data item into a summary
- SummaryEstimate: estimate statistic(s) from a summary
- SummaryMerge: merge two or more summaries
- SummarySubtract: subtract one summary from another
- SummaryDelete: delete a raw data item from a summary
- SummaryJoin: join two summaries based on "foreign-key" of the summaries, the resulting summary can estimate statistics 
