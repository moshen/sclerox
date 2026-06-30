# ol - Operating Layer Knowledge Base

Use when the user asks about people, meetings, projects, todos, past decisions, research, or code — or when knowledge base context would help.

## When to use

- Starting work: search for related meetings, todos, investigations, project context
- Colleague mentioned: look them up for contact details
- Past decision referenced: search memory and investigations
- After learning something: save to memory for future sessions
- When a memory is wrong or outdated: mark it stale or supersede it
- Looking for code: search symbols across all indexed repos

## Commands

```bash
# Global search (memory, people, meetings, projects, todos, investigations)
ol search "<query>"

# Memory
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

# People  — ALWAYS use --name flag, never positional: ol people add --name "Alice"
ol people search "<name or email or identifier>"
ol people add --name "<name>" [--email "<e>"] [--github "<u>"] [--slack "<id>"] [--atlassian "<e>"]
ol people get <id>
ol people update <id> [--name] [--notes]
ol people identifier add <person_id> <type> <value>   # add any identifier
ol people types list                                  # see valid identifier types

# Meetings
ol meeting search "<topic>"
ol meeting add --title "<title>" --date <YYYY-MM-DD> --notes "<notes>"
ol meeting people add|remove|list <meeting_id> <person_id> [--role "<role>"]

# Todos  — ALWAYS use --title flag, never positional: ol todo add --title "Fix X"
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

# Research / Investigations
# COMMAND NAMES: start (not create/add/new), get (not show), list --status all (not --all)
# --name is REQUIRED as a flag, never positional
ol research list                           # open investigations (default)
ol research list --status all              # all statuses
ol research start --name "<name>" --slug "<slug>" [--plan "<scope>"]
ol research get <id-or-slug>               # NOT 'show' — get
ol research add-source <id> --url "<url>" [--label "<label>"] [--notes "<notes>"]
ol research sources <id>
ol research update <id> [--plan "<text>"] [--findings "<text>"]
ol research conclude <id> --findings "<findings>"
ol research reopen <id>
ol research search "<query>"
ol research people add|remove|list <id> <person_id>   # NOT link-person
ol research projects add|remove|list <id> <project_id>

# Projects
ol project search "<description>"
ol project get <id>
ol project add --name "<name>" [--description "<desc>"] [--link "url|label"]
ol project people add|remove|list <project_id> <person_id> [--role "<role>"]
ol project meetings add|remove|list <project_id> <meeting_id>
ol project repos add|remove|list <project_id> <repo_id>

# Repos
ol repo list                         # all indexed repos
ol repo search "<query>"             # find repos by name/description
ol repo index [path]                 # index a repo (embeddings on by default)
ol repo index --no-embed [path]      # index without generating embeddings
ol repo show [path] [--symbols "<query>"]
ol repo sync                         # heal registry: remove stale, reindex missing

# Code search and navigation (call graph across indexed repos)
ol code search "<query>"                        # find symbols by name
ol code search --repo "<name>" "<query>"        # scoped to one repo
ol code calls <symbol>                          # what does this symbol call?
ol code calls --repo "<name>" <symbol>          # scoped
ol code refs <symbol>                           # what calls/uses this symbol?
ol code refs --repo "<name>" <symbol>           # scoped
ol code graph <symbol>                          # BFS call graph (depth 3)
ol code graph --depth 5 <symbol>               # deeper traversal

# Advanced code search patterns
# Find files containing a symbol then pipe to ast-grep for structural matching:
ol code search "<symbol>" --format json \
  | jq -r '.[] | select(.type=="Symbol") | "\(.repo_path)/\(.file_path)"' \
  | sort -u \
  | xargs ast-grep --pattern '<pattern>' --lang <lang>
```

## Patterns

**Before any task:** `ol search "<topic>"`
**Finding code:** `ol code search "<function or type name>"` or `ol code search --repo <name> "<query>"`
**Understanding a function's dependencies:** `ol code calls <symbol_name>`
**Finding what uses a function (impact of changes):** `ol code refs <symbol_name>`
**Exploring call chains:** `ol code graph <symbol_name> --depth 4`
**After a decision:** `ol memory set "<key>" "<decision>" --type project`
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

## Workflows

### When working on a repo

After `ol repo index` or when the user asks about a codebase:
1. Check whether a project already tracks this repo: `ol project search "<repo name>"`
2. **If found:** link if not already linked: `ol project repos add <project_id> <repo_id>`
3. **If not found:** create one from the repo metadata, then link:
   ```
   ol project add --name "<repo name>" --description "<what this service does>" \
     [--link "<git remote url>|GitHub"]
   ol project repos add <new_project_id> <repo_id>
   ```
   Use `git remote get-url origin` to get the remote URL for the project link.

### When starting a research investigation

After `ol research start`:
1. Search for a related project: `ol project search "<investigation topic>"`
2. If found, link the investigation: `ol research projects add <investigation_id> <project_id>`
3. When the investigation concludes, save the key finding as a memory:
   `ol memory set "research/<slug>/finding" "<one-line finding>" --type project`

### When creating a todo that belongs to a project

1. Add the todo: `ol todo add --title "..." --category github`
2. Find or confirm the project: `ol project search "<area>"`
3. Link it: `ol todo projects add <todo_id> <project_id>`

### When meeting notes mention a project

After `ol meeting add`:
1. Search for the project: `ol project search "<project name from notes>"`
2. Link the meeting: `ol project meetings add <project_id> <meeting_id>`
