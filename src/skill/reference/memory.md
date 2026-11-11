# Memory

Recording decisions is a core behavior - see the "Decisions" section in
SKILL.md; do it immediately and unprompted.

```bash
sclerox memory set <key> "<value>" --type user|feedback|project|reference|session
sclerox memory set <key> "<value>" --repo        # scope it to the current git repo
sclerox memory set <key> "<value>" --global      # drop its repo scope
sclerox memory set <key> "<value>" --project <id>  # link it to a project (repeatable)
sclerox memory get <key>                  # includes its origin, scope, projects and sources
sclerox memory get <key> --history        # current value, every earlier value + supersession chain
sclerox memory search "<query>"           # active only by default
sclerox memory search "<query>" --status all  # also stale/superseded (or one: stale, superseded)
# Only active memories match by meaning: stale/superseded ones need a keyword in the query
sclerox memory list [--type <t>] [--status active|stale|superseded|all]
sclerox memory stale <key> [--reason "why it's no longer valid"]
sclerox memory supersede <old_key> <new_key> "<new_value>"
sclerox memory conflicts                  # near-duplicate clusters flagged at distillation
sclerox memory distinct add <a> <b> [--reason "<why>"]   # these two are different facts
sclerox memory distinct remove <a> <b>    # undo that
sclerox memory distinct list              # pairs marked distinct
sclerox memory review <key>               # confirm memory is still accurate
sclerox memory needs-review [--days 30]   # list memories not reviewed recently
sclerox memory distill <key>              # compress verbose entry via your agent CLI
sclerox memory distill --from <file>      # extract memories from a file via your agent CLI
sclerox memory distill --from <file> --model <model>  # specify model explicitly
sclerox memory import --agent claude      # import from Claude Code auto-memory
sclerox memory import --path <dir>        # import from any directory of .md files
sclerox memory people add|remove|list <key> <person_id>
```

## Where a memory came from

`memory get` shows each memory's origin (hand-written, distilled or imported),
its scope, linked projects, and every recorded source: the command or session
that wrote it, the repo, commit and branch it ran in, and for a distilled
memory the transcript and the line range it was read from (with the first and
last message ids, in case the lines ever move). Sources are pointers, never
copies of the transcript. Memories written before sources were recorded show
them as unknown. A merged memory keeps the sources of everything merged into
it, and `--history` rows keep the sources they had.

A memory is global unless scoped. A scoped memory shows in session start only
in its repo, recognised by the repo's first commit or its remote, so another
clone of the same repo counts. A repo with neither (no commits and no remote
when the memory was scoped) is recognised by its directory name alone, which
an unrelated repo of the same name also matches. After scoped memories,
session start favours global memories that name the repo. Distillation
scopes a memory to the session's repo when it only matters there, and may
link it to a project.

Supersede requires an ACTIVE old key and a DIFFERENT new key (a same-key
update is `sclerox memory set`, which keeps the value it replaces in
`--history`). To merge several duplicates, supersede each into ONE
canonical key - repeated supersedes converge on it. Retired keys never
block reuse: setting a superseded key creates a fresh entry instead of
resurrecting the old row.

A conflict is a near-duplicate cluster distillation refused to auto-merge:
the matches were too far apart to be confidently the same fact, or the only
close ones were written by hand and must not be overwritten automatically.
Resolve one by reading both sides and either superseding the loser into the
winner or staling it; the pair leaves the list as soon as either side is
not active.

When a session restates a hand-written memory differently, distillation
never rewrites it: the new wording goes to a sibling key, usually
`<key>-distilled` (`-distilled-2` and so on when that key is hand-written
or marked distinct), and is flagged against the original. Take the sibling's
exact key from `memory conflicts`. If it is a real correction, `memory set`
the original with the corrected value; either way, then stale that sibling.

Resolving by merging or staling assumes the pair IS a duplicate. When it is
not - two genuinely different facts that happen to read alike - say so instead:

```bash
sclerox memory distinct add retry-policy-http-client retry-policy-queue-worker \
  --reason "same retry count, different services"
```

That clears the flag, stops the pair being re-flagged, and stops distillation
merging one into the other. Without it a deliberate split has no protection:
the pair stays flagged forever, and the only way to silence it is to merge -
the opposite of the judgement being made.
