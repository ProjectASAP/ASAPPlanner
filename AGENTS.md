# ProjectASAP agent policy

- Work in a dedicated Git worktree unless the user explicitly says otherwise.
- Create the dedicated Git worktree inside the repository root at
  `.worktrees/<worktree-name>`. Do not create worktrees as sibling directories
  outside the repository root.
- Create new worktrees from `origin/main` unless another base is specified.
  Fetch the base first, and never disturb an existing dirty working tree.
- Do not use `git add -A` or other broad staging commands; stage only the files
  relevant to the requested change.
- Do not amend commits unless the user explicitly asks you to.
- Keep communication and generated prose concise.
- Prefer the minimally complex implementation that satisfies the requirement.
- Do not introduce a conceptual layer, abstraction, or public interface without
  explaining the concrete need it addresses.
- Comments explain non-obvious intent, invariants, or constraints. Do not
  duplicate the implementation or use comments as a substitute for maintained
  documentation.
- Choose a document's audience before writing it: user, developer, or
  designer/architect/researcher. Do not mix implementation details into user
  documentation.
- Add focused unit and end-to-end coverage in proportion to the behavior being
  changed. Give each new test a short statement of the behavior it verifies.
- Define acceptance behavior before implementation when practical. For
  consequential changes, prefer test design or final review by a person or agent
  that did not implement the change; never claim independence when it did not
  occur.
- For correctness fixes, first establish a regression test that fails for the
  original behavior, then confirm it passes after the fix.
- Commit at incremental logical boundaries when commits are requested.
- Keep pull-request descriptions top-down and concise, in the sections of the
  `asap-pr-authoring` skill: **Summary**, **Review guide**, **Behavior: before
  → after** (one end-to-end example), **Interfaces** (diff blocks, or "None"),
  **Algorithm changes** (or "None"), **Decisions and open questions**,
  **Verification**, and **Follow-ups**.
- A PR that changes planner snapshots
  (`crates/devtools/tests/corpus_snapshots/`) says under **Verification**
  which queries changed and why; a person confirms the why.
- Agents must not complete fields reserved for human judgment, approval, or
  attestation.
- Do not create, push, or publish commits or pull requests unless the user has
  authorized those external changes.
