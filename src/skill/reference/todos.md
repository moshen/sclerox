# Todos

ALWAYS use the `--title` flag, never positional: `ol todo add --title "Fix X"`.

```bash
ol todo list                         # open todos (default)
ol todo list --status all            # all statuses (NOT --all)
ol todo add --title "<title>" [--category slack|github|email|meeting|general]
ol todo update <id> [--title] [--notes] [--deadline] [--category]
ol todo done <id> [--note "<resolution>"]
ol todo watch <id>
ol todo reopen <id>
ol todo history [<query>]
ol todo search "<query>"
ol todo people add|remove|list <todo_id> <person_id>
ol todo projects add|remove|list <todo_id> <project_id>
```

## Workflow: a todo that belongs to a project

1. Add the todo: `ol todo add --title "..." --category github`
2. Find or confirm the project: `ol project search "<area>"`
3. Link it: `ol todo projects add <todo_id> <project_id>`
