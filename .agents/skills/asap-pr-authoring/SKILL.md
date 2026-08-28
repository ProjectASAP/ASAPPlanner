---
name: asap-pr-authoring
description: Prepare concise ProjectASAP commits and pull-request descriptions. Use when asked to organize commits, draft a PR, or explain a completed change for review.
---

# Commits and pull requests

Organize commits at incremental logical boundaries. Do not include unrelated
working-tree changes or rewrite existing history without explicit authorization.

Write the pull-request description top-down:

1. **Why**: the user, design, correctness, or MVP need.
2. **What**: the observable behavior and scope changed.
3. **How**: the core implementation or algorithm, without narrating every file.
4. **Before this PR**: the previous end-to-end behavior, limitation, failure
   mode, architecture, or workflow.
5. **After this PR**: the new end-to-end behavior and what users, developers, or
   operators can do differently.
6. **Verification**: focused tests and applicable end-to-end evidence.

Include a compact before/after example, screenshot, or diagram when it provides
real evidence or makes behavior easier to understand. For significant design
changes, summarize architectural decisions and alternatives.

Use the evidence form appropriate to the change: screenshots for visual output,
execution examples for behavior, measurements for performance, or diagrams for
architecture. Explicitly mark an evidence type as not applicable instead of
fabricating it.

State limitations and follow-up work explicitly. Never fabricate screenshots,
test results, measurements, or human approval. Creating or publishing a commit or
PR still requires user authorization.
