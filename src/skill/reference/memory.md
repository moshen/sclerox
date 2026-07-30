# Memory

Recording decisions is a core behavior — see the "Decisions" section in
SKILL.md; do it immediately and unprompted.

```bash
ol memory set <key> "<value>" --type user|feedback|project|reference|session
ol memory get <key>
ol memory search "<query>"           # active only by default
ol memory search "<query>" --all     # include stale/superseded
ol memory stale <key> [--reason "why it's no longer valid"]
ol memory supersede <old_key> <new_key> "<new_value>"
ol memory review <key>               # confirm memory is still accurate
ol memory needs-review [--days 30]   # list memories not reviewed recently
ol memory distill <key>              # compress verbose entry via your agent CLI
ol memory distill --from <file>      # extract memories from a file via your agent CLI
ol memory distill --from <file> --model <model>  # specify model explicitly
ol memory import --agent claude      # import from Claude Code auto-memory
ol memory import --path <dir>        # import from any directory of .md files
ol memory people add|remove|list <key> <person_id>
```
