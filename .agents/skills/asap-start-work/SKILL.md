---
name: asap-start-work
description: Prepare an isolated ProjectASAP Git worktree before implementation. Use when starting repository changes; skip only when the user specifies another workspace strategy.
---

# Start work

Inspect repository status without modifying the current working tree. Preserve
all existing and untracked user changes.

Unless the user specifies otherwise, fetch `origin`, verify `origin/main`, and
create a dedicated worktree and task branch from that remote commit. Choose a
worktree path and branch name scoped to the task, check for collisions first, and
report both to the user.

Do not delete, overwrite, prune, or reuse an occupied worktree without explicit
authorization. If network access or `origin/main` is unavailable, report the
condition and use another base only with user direction.
