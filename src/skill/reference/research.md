# Research / Investigations

`--name` is REQUIRED as a flag, never positional. Use `--status all` (not
`--all`). Aliases: start = create/add/new, get = show, conclude = close/finish.

```bash
sclerox research list                           # open investigations (default)
sclerox research list --status all              # all statuses
sclerox research start --name "<name>" --slug "<slug>" [--plan "<scope>"]
sclerox research get <id-or-slug>
sclerox research add-source <id> --url "<url>" [--label "<label>"] [--notes "<notes>"]
sclerox research sources <id>
sclerox research update <id> [--plan "<text>"] [--findings "<text>"]
sclerox research conclude <id> --findings "<findings>"
sclerox research reopen <id>
sclerox research search "<query>"
sclerox research people add|remove|list <id> <person_id>   # NOT link-person
sclerox research projects add|remove|list <id> <project_id>
```

## Workflow: starting a research investigation

After `sclerox research start`:

1. Search for a related project: `sclerox project search "<investigation topic>"`
2. If found, link the investigation:
   `sclerox research projects add <investigation_id> <project_id>`
3. When the investigation concludes, save the key finding as a memory:
   `sclerox memory set "research/<slug>/finding" "<one-line finding>" --type project`
