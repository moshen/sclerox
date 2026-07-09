# ol - Operating Layer Knowledge Base

Use when the user asks about people, meetings, projects, todos, past decisions, research, or code — or when knowledge base context would help.

## When to use

- Starting work: search for related meetings, todos, investigations, project context
- Colleague mentioned: look them up for contact details
- Past decision referenced: search memory and investigations
- After learning something: save to memory for future sessions
- When a memory is wrong or outdated: mark it stale or supersede it
- Looking for code: use `ol code` (see below) BEFORE Grep/Glob

## Code search — prefer this over Grep/Glob

When searching for symbols, functions, types, or callers in ANY indexed repo,
use `ol code` BEFORE reaching for Grep or Glob. It is pre-indexed (no directory
walk), cross-repo (finds callers in OTHER repos Grep can't see), structural
(matches symbols, not raw strings), and call-graph aware. Fall back to Grep
only when the repo is not indexed (`ol repo list`) or the query is free text
rather than a symbol.

```bash
# "Where is X defined?"                    → find the symbol
ol code search "X"
ol code search --repo "<name>" "X"         # scope to one repo
# "What calls X? / what breaks if I change X?" → impact analysis across repos
ol code refs X
# "What does X depend on?"                 → outgoing calls
ol code calls X
# "Trace the flow from X"                  → BFS call graph
ol code graph X --depth 4

# Compose with ast-grep for structural matching: ol finds the files, ast-grep
# matches the pattern. NOTE the flag is --output json (not --format).
ol code search "<symbol>" --output json \
  | jq -r '.[] | select(.type=="Symbol") | "\(.repo_path)/\(.file_path)"' \
  | sort -u \
  | xargs ast-grep --pattern '<pattern>' --lang <lang>
```

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
# ALWAYS store the FULL transcript when you have one, not just a summary.
# --notes is a short summary; --transcript-file is the complete text (chunked
# and embedded for semantic search). Write the transcript to a temp file and
# pass its path. Only fall back to notes-only when no transcript exists.
ol meeting search "<topic>"
ol meeting add --title "<title>" --date <YYYY-MM-DD> \
  --transcript-file <path> [--notes "<summary>"]
ol meeting add --title "<title>" --date <YYYY-MM-DD> --notes "<notes>"   # no transcript available
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
# --name is REQUIRED as a flag, never positional. Use --status all (not --all).
# Aliases: start=create/add/new, get=show, conclude=close/finish.
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

# Code search and navigation — see the "Code search" section above (prefer over Grep)
ol code search "<query>"                         # find symbols by name
ol code refs <symbol>                            # what calls/uses this symbol?
ol code calls <symbol>                           # what does this symbol call?
ol code graph <symbol> --depth 4                 # BFS call graph

# Config  — tunables live in ~/.ol/config.toml (created by `ol install`)
ol config show                       # effective settings (file + env + defaults)
ol config init [--force]             # write a commented config.toml
ol config path                       # where the config file lives
# Precedence: CLI flag > env var > ~/.ol/config.toml > built-in default.
# Tunables include search.semantic_threshold, dedup thresholds,
# session_context sizes, distill chunking, embed chunk size.
```

## Patterns

**Before any task:** `ol search "<topic>"`
**Finding code (prefer over Grep):** `ol code search "<symbol>"` / `ol code refs <symbol>`
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

### When recording a meeting

1. If you have the full transcript (e.g. Gemini/Zoom notes, a pasted log),
   store ALL of it — write it to a temp file and pass `--transcript-file`.
   The transcript is chunked and embedded, so semantic search can later find
   what was actually said, not just a summary. Add `--notes` for a short recap
   on top. Do NOT collapse a real transcript into a notes summary.
2. If a project is mentioned: `ol project search "<name>"`, then link with
   `ol project meetings add <project_id> <meeting_id>`.
