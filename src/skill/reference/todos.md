# Todos

ALWAYS use the `--title` flag, never positional: `sclerox todo add --title "Fix X"`.

```bash
sclerox todo list                         # open todos (default)
sclerox todo list --status all            # all statuses (NOT --all)
sclerox todo add --title "<title>" [--category slack|github|email|meeting|general]
sclerox todo update <id> [--title] [--notes] [--deadline] [--category]
sclerox todo done <id> [--note "<resolution>"]
sclerox todo watch <id>
sclerox todo reopen <id>
sclerox todo history [<query>]
sclerox todo search "<query>"
sclerox todo people add|remove|list <todo_id> <person_id>
sclerox todo projects add|remove|list <todo_id> <project_id>
```

## Workflow: a todo that belongs to a project

1. Add the todo: `sclerox todo add --title "..." --category github`
2. Find or confirm the project: `sclerox project search "<area>"`
3. Link it: `sclerox todo projects add <todo_id> <project_id>`
