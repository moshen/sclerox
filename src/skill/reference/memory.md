# Memory

Recording decisions is a core behavior - see the "Decisions" section in
SKILL.md; do it immediately and unprompted.

```bash
sclerox memory set <key> "<value>" --type user|feedback|project|reference|session
sclerox memory get <key>
sclerox memory get <key> --history        # every row the key ever had + supersession chain
sclerox memory search "<query>"           # active only by default
sclerox memory search "<query>" --all     # include stale/superseded
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

Supersede requires an ACTIVE old key and a DIFFERENT new key (a same-key
update is `sclerox memory set`). To merge several duplicates, supersede each
into ONE canonical key - repeated supersedes converge on it. Retired keys
never block reuse: setting a superseded key creates a fresh entry instead
of resurrecting the old row.

A conflict is a near-duplicate cluster distillation refused to auto-merge:
the matches were too far apart to be confidently the same fact, or the only
close ones were written by hand and must not be overwritten automatically.
Resolve one by reading both sides and either superseding the loser into the
winner or staling it; the pair leaves the list as soon as either side is
not active.

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
