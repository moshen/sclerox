# Research / Investigations

`--name` is REQUIRED as a flag, never positional. Use `--status all` (not
`--all`). Aliases: start = create/add/new, get = show, conclude = close/finish.

```bash
ol research list                           # open investigations (default)
ol research list --status all              # all statuses
ol research start --name "<name>" --slug "<slug>" [--plan "<scope>"]
ol research get <id-or-slug>
ol research add-source <id> --url "<url>" [--label "<label>"] [--notes "<notes>"]
ol research sources <id>
ol research update <id> [--plan "<text>"] [--findings "<text>"]
ol research conclude <id> --findings "<findings>"
ol research reopen <id>
ol research search "<query>"
ol research people add|remove|list <id> <person_id>   # NOT link-person
ol research projects add|remove|list <id> <project_id>
```

## Workflow: starting a research investigation

After `ol research start`:

1. Search for a related project: `ol project search "<investigation topic>"`
2. If found, link the investigation:
   `ol research projects add <investigation_id> <project_id>`
3. When the investigation concludes, save the key finding as a memory:
   `ol memory set "research/<slug>/finding" "<one-line finding>" --type project`
