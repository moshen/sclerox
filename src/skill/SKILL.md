---
name: ol-kb
description: Operating Layer knowledge base for people, meetings, projects, todos, decisions, research, and cross-repo code search. Use when the user asks about colleagues, past decisions, meetings, projects, todos, or research; when a decision is made (record it immediately, unprompted); or when searching for code symbols or callers (prefer `ol code` over Grep). Full command reference and workflows live in reference/.
---

# ol - Operating Layer Knowledge Base

Use when the user asks about people, meetings, projects, todos, past
decisions, research, or code, or when knowledge base context would help.

Per-domain detail (each area's commands plus its workflow) lives in
`reference/`. Read the one file for whatever you're working on — see the
Reference index at the end.

## When to use

- Starting work: search for related meetings, todos, investigations,
  project context
- Colleague mentioned: look them up for contact details
- Past decision referenced: search memory and investigations
- A decision is made: record it as a memory IMMEDIATELY, unprompted (see below)
- After learning something: save to memory for future sessions
- When a memory is wrong or outdated: mark it stale or supersede it
- Looking for code: use `ol code` (see below) BEFORE Grep/Glob

## Decisions — record automatically, don't wait to be asked

Whenever a decision is encountered in conversation, write it to memory at that
moment. Do not ask permission and do not defer to session distillation — the
decision and its WHY should survive even if the session is never distilled.

Triggers (any of these = a decision):

- The user picks an approach, option, or tool ("let's go with X", "do it that way")
- The user accepts or rejects a recommendation you made
- A convention, threshold, or default is settled ("always X", "keep it as-is")
- Something is deliberately deferred or descoped ("not now", "revisit when Y")
- A meeting note or document you're processing states a decision

```bash
ol memory set "<area>-<decision-slug>" \
  "<what was decided>. Why: <reasoning>. Rejected: <alternatives, if any>" \
  --type project
```

Include the why and the rejected alternatives — future sessions need the
reasoning, not just the verdict. If the decision reverses an earlier one, use
`ol memory supersede <old_key> <new_key> "<new value>"` instead of a bare set.

## Code search — prefer this over Grep/Glob

When searching for symbols, functions, types, or callers in ANY indexed repo,
use `ol code` BEFORE reaching for Grep or Glob. It is pre-indexed (no directory
walk), cross-repo (finds callers in OTHER repos Grep can't see), structural
(matches symbols, not raw strings), and call-graph aware. Fall back to Grep
only when the repo is not indexed (`ol repo list`) or the query is free text
rather than a symbol.

```bash
ol code search "X"                  # where is X defined?
ol code refs X                      # what calls X? (impact analysis, cross-repo)
ol code calls X                     # what does X call?
ol code graph X --depth 4           # BFS call graph
```

For scoping to one repo and composing with ast-grep for structural matching,
see [reference/repos-and-code.md](reference/repos-and-code.md).

## Patterns

**Before any task:** `ol search "<topic>"`
**Finding code (prefer over Grep):** `ol code search "<symbol>"` / `ol code refs <symbol>`
**After a decision:** record immediately, unprompted (see "Decisions" above)
**When a memory is wrong:** `ol memory stale <key> --reason "<why>"`
**When a memory is outdated:** `ol memory supersede <old> <new> "<updated value>"`
**After research:** `ol research conclude <id> --findings "<findings>"`
**Attributing a memory:** `ol memory people add <key> <person_id>`
**Session summary:** `ol memory set "session/<YYYY-MM-DD>/<slug>" "<what was done>" --type session`

## Privacy — wrap secrets in `<private>` BEFORE you reply

The Stop hook distills every session into long-lived memory. **Anything you
include in a reply that isn't wrapped in `<private>...</private>` is fair game
for distillation and will persist.** The tags are stripped before the AI
distiller sees the text, so wrapped regions never land in memory.

**Wrap before responding** whenever output contains:

- Credentials, API keys, tokens, passwords, session cookies
- Output of commands that print secrets: `aws sts ...`, `gcloud auth ...`,
  `kubectl config view`, `cat .env`, `printenv`, `op item get`, anything
  reading from a credentials file
- Customer PII, employee personal data, salary/comp, HR discussions
- Private URLs containing tokens (`?key=...`, signed S3 URLs)
- Internal hostnames, IPs, or infrastructure detail the user flags as sensitive

When in doubt, wrap. Over-redacting is cheap; a leaked credential in memory
is not.

```
Here are the prod credentials I just pulled:
<private>
AWS_ACCESS_KEY_ID=AKIA...
AWS_SECRET_ACCESS_KEY=...
</private>
We need to rotate them tomorrow.
```

Only "Here are the prod credentials I just pulled:" and "We need to rotate
them tomorrow." can be distilled. The keys themselves are dropped.

**Do not** wrap normal technical discussion, error messages without
credentials, file paths, or commit hashes — that's the content the agent
needs to recall later.

## Session start context

When a session starts in a git repo, `ol hook start` automatically injects a
compact index of open todos, open research, recent sessions, and memory keys
(capped at ~750 tokens). You do NOT need to run `ol todo list` or `ol memory
list` at session start — they're already visible. Use `ol memory get <key>`,
`ol todo get <id>`, etc. to fetch full content for items that look relevant.

## Reference

Global search across everything: `ol search "<query>"`. For an area's full
commands and its workflow, read the matching file on demand:

- [reference/memory.md](reference/memory.md) — set/get/search, stale, supersede, review, distill, import
- [reference/people.md](reference/people.md) — people and their identifiers
- [reference/meetings.md](reference/meetings.md) — meetings, storing full transcripts, recording a meeting
- [reference/todos.md](reference/todos.md) — todos, linking a todo to a project
- [reference/research.md](reference/research.md) — investigations, starting one and linking it to a project
- [reference/projects.md](reference/projects.md) — projects and their people/meetings/repos links
- [reference/repos-and-code.md](reference/repos-and-code.md) — repo indexing, `ol code` search/refs/graph, ast-grep, indexing workflows
- [reference/config.md](reference/config.md) — `ol config`, tunables, precedence
