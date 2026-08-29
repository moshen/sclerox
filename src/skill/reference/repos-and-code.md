# Repos and code search

## Repos

```bash
sclerox repo list                         # all indexed repos
sclerox repo search "<query>"             # find repos by name/description
sclerox repo index [path]                 # index a repo (embeddings on by default)
sclerox repo index --no-embed [path]      # index without generating embeddings
sclerox repo index --force [path]         # index even if over the max-files cap
sclerox repo show [path] [--symbols "<query>"]
sclerox repo sync                         # heal registry: remove stale/nested, reindex missing
sclerox repo reembed [--repo <name>] [--force]  # backfill embeddings for indexed chunks
```

Only git repos are auto-indexed (a folder needs a `.git`). Indexing happens at
the git root; nested git repos each get their own index (one per `.git` level),
and indexing a parent also indexes the nested repos it contains. Opt a folder
OUT of indexing: create `<repo-root>/.sclerox/config.toml` with `index = false`; the
SessionStart hook and `sclerox repo index` both skip it.

## Code search and navigation

Prefer `sclerox code` over Grep/Glob for symbols (see the rule in SKILL.md).

```bash
sclerox code search "<query>"                    # find symbols by name
sclerox code search --repo "<name>" "<query>"    # scope to one repo
sclerox code refs <symbol>                       # what calls/uses this symbol?
sclerox code calls <symbol>                      # what does this symbol call?
sclerox code graph <symbol> --depth 4            # BFS call graph
```

Compose with ast-grep for structural matching: `sclerox code` finds the files,
ast-grep matches the pattern. NOTE the flag is `--output json` (not `--format`).

```bash
sclerox code search "<symbol>" --output json \
  | jq -r '.[] | select(.type=="Symbol") | "\(.repo_path)/\(.file_path)"' \
  | sort -u \
  | xargs ast-grep --pattern '<pattern>' --lang <lang>
```

## Workflow: entering a repo/folder not yet indexed

The SessionStart hook auto-indexes the git repo you start in. If you're about to
work in a folder that is NOT already indexed (check `sclerox repo list`) - especially
a large monorepo or a catch-all working directory - ASK the user whether to
index it before running `sclerox repo index`. Indexing large trees adds many embedded
chunks and slows search. If they decline, create `<repo-root>/.sclerox/config.toml`
with `index = false` so the hook and future `sclerox repo index` calls skip it
permanently.

## Workflow: working on a repo

After `sclerox repo index` or when the user asks about a codebase:

1. Check whether a project already tracks this repo:
   `sclerox project search "<repo name>"`
2. **If found:** link if not already linked:
   `sclerox project repos add <project_id> <repo_id>`
3. **If not found:** create one from the repo metadata, then link:
   ```
   sclerox project add --name "<repo name>" --description "<what this service does>" \
     [--link "<git remote url>|GitHub"]
   sclerox project repos add <new_project_id> <repo_id>
   ```
   Use `git remote get-url origin` to get the remote URL for the project link.
