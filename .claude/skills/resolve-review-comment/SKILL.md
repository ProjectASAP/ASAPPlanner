---
name: resolve-review-comment
description: Ground a PR review comment in the actual code/docs it refers to, then either explain it in plain English or draft a concrete fix (doc rewrite, dev guide, code change) — never guesses, never auto-pushes.
---

# Resolve a review comment

Use this when a user is working through a reviewer's comments on a PR (e.g.
"solve so-and-so's comment on PR #N", or a pasted GitHub comment) and wants
either an explanation to post back, or an actual fix.

Args: the PR number and, if there are several open threads, enough of the
comment text or thread id to identify which one.

## Steps

1. **Fetch the real comment and PR context** — don't work from the user's
   paraphrase alone. Use `gh pr view <N> --json body,files,headRefName` for
   the PR description/diff, and `gh api repos/{owner}/{repo}/pulls/<N>/comments`
   (or `gh pr view <N> --comments` for issue-level comments) for the actual
   review thread text, author, and the file/line it's anchored to.

2. **Find the ground truth the comment is about** — the actual code, doc
   section, or design doc paragraph the comment refers to. Read it in full;
   don't paraphrase from memory or from the PR description alone. If the
   comment references an identifier (a type, trait, function), grep for its
   real definition and doc comments before explaining or rewriting anything.

3. **Classify the ask**, since the two calls for action are different:
   - **"I don't understand X" / "can you explain this?"** → the fix is
     clarity, not new content. Rewrite the confusing passage in plain
     English, grounded in what the code/doc actually does — don't just
     restate it with different words if the underlying explanation itself
     has a gap; find the concrete thing that's unclear (an undefined term,
     a skipped step, jargon with no referent) and fix that.
   - **"can you document/create dev docs for this?"** → check what
     documentation-level is actually missing before writing anything.
     Module-level rustdoc and a design doc are different audiences than a
     "how do I extend this" dev guide — figure out which one is genuinely
     absent (don't duplicate what already exists) before drafting.
   - **A substantive objection** (e.g. "this doesn't belong in this doc",
     "this is wrong") → don't just reword; change the actual content/scope
     to address the objection, and say plainly what changed and why.

4. **Propose before applying.** Show the user the rewritten passage or new
   doc content and ask for confirmation before editing tracked files —
   this repo's convention (see recent conversation history) is "here's the
   rewrite, want me to swap it in?" rather than editing silently.

5. **Never push or reply on GitHub without being asked.** Committing and
   pushing, or posting a reply on the PR thread, are separate explicit
   asks from "draft this" — confirm which the user wants. If multiple
   people have pushed to the same branch, `git fetch` + rebase onto the
   remote tip rather than force-pushing over their work.

## Non-goals

- Don't invent design decisions the code/docs don't already support —
  ground every explanation in something read this session, not assumed.
- Don't resolve/reply to a GitHub thread the user hasn't confirmed the fix
  for, and never resolve a thread that isn't activated for Claude.
