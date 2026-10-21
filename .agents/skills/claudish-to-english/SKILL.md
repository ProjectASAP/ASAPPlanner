---
name: claudish-to-english
description: Rewrite dense, jargon-heavy agent text ("Claudish") into plain English. Use when asked to rewrite, simplify, or make readable a previous reply, a pasted passage, a file, a PR or issue body, or a review, or when asked for a TL;DR of one.
---

# Claudish to plain English

Rewrite text so a reader who did not watch the work can follow it on the first
read. This is a rewrite, not a summary, unless a TL;DR is asked for.

## What to rewrite

- **"your last reply", "that", or no target given:** your own previous message
  in this conversation.
- **Pasted text:** the pasted text.
- **A path or URL:** that file or page. Read all of it first.
- **A PR, issue, or comment number:** fetch its body with `gh api`.

Do not rewrite prose the user wrote unless they explicitly name it.

## Output

- **Chat targets:** print only the rewrite, with no preamble and no closing
  commentary.
- **Files:** write `NAME.plain.md` next to `NAME.md` and leave the original
  untouched. Overwrite the original only when asked.
- **PR or issue bodies:** show the rewrite. Update GitHub only when asked, and
  then through the REST API.
- **Language:** write in the language of the source text, unless the user names
  another one, e.g. "in Chinese".

## Rules

**Keep every fact.**
- Every number, unit, name, identifier, file path, command, PR or issue number,
  SHA, and error message stays exactly as written.
- Never round, merge, or drop a number. Never invent detail.
- Fenced code blocks, inline code, tables of data, and quoted output stay
  byte-for-byte.

**Plain words.**
- Use short sentences, one idea each, and everyday words.
- Use active voice with a named subject: "the planner picks", not "is picked".
- At the first use of a term of art the reader may not know, either replace it
  with plain words or add a short definition in the same sentence. Keep the
  term itself when the reader will see it in code or the UI.
- Expand an abbreviation on first use, then keep using it.
- Cut filler, hedges, and stacked qualifiers. Keep one hedge where the
  uncertainty is real, and say what it depends on.

**Structure.**
- Lead with the result or answer, then the reasons, then the details.
- Use a heading or a bold lead-in where the source changes topic.
- Use a list for parallel items and a numbered list only for real sequences.
- Keep the source's tables when they carry data, and simplify only their prose
  cells.
- No em-dash asides, no "not X, but Y" framing, no colon-then-reveal sentences,
  and no scare quotes. Use a full stop or a parenthesis instead.

**Point of view.** Keep the source's speakers. In an assistant reply, "I" is
the assistant and "you" is the user; never swap them.

**Length.**
- The default rewrite may be about as long as the source; clarity matters more
  than brevity.
- For "tldr", "short", or "summary": half the length or less. Keep decisions,
  numbers, blockers, and next steps, and drop secondary detail. You may omit
  code blocks, since the original still has them.

## Check before answering

1. Every number, path, and identifier in the source appears in the rewrite,
   unchanged. Count them if the source is long.
2. No claim is stronger or weaker than in the source.
3. A reader new to the task could act on the rewrite without the original.
