# Repos and code search

## Repos

```bash
ol repo list                         # all indexed repos
ol repo search "<query>"             # find repos by name/description
ol repo index [path]                 # index a repo (embeddings on by default)
ol repo index --no-embed [path]      # index without generating embeddings
ol repo index --force [path]         # index even if over the max-files cap
ol repo show [path] [--symbols "<query>"]
ol repo sync                         # heal registry: remove stale/nested, reindex missing
ol repo reembed [--repo <name>] [--force]  # backfill embeddings for indexed chunks
```

Only git repos are auto-indexed (a folder needs a `.git`). Indexing happens at
the git root; nested git repos each get their own index (one per `.git` level),
and indexing a parent also indexes the nested repos it contains. Opt a folder
OUT of indexing: create `<repo-root>/.ol/config.toml` with `index = false`; the
SessionStart hook and `ol repo index` both skip it.

## Code search and navigation

Prefer `ol code` over Grep/Glob for symbols (see the rule in SKILL.md).

```bash
ol code search "<query>"                    # find symbols by name
ol code search --repo "<name>" "<query>"    # scope to one repo
ol code refs <symbol>                       # what calls/uses this symbol?
ol code calls <symbol>                      # what does this symbol call?
ol code graph <symbol> --depth 4            # BFS call graph
```

Compose with ast-grep for structural matching: `ol code` finds the files,
ast-grep matches the pattern. NOTE the flag is `--output json` (not `--format`).

```bash
ol code search "<symbol>" --output json \
  | jq -r '.[] | select(.type=="Symbol") | "\(.repo_path)/\(.file_path)"' \
  | sort -u \
  | xargs ast-grep --pattern '<pattern>' --lang <lang>
```

## Workflow: entering a repo/folder not yet indexed

The SessionStart hook auto-indexes the git repo you start in. If you're about to
work in a folder that is NOT already indexed (check `ol repo list`) — especially
a large monorepo or a catch-all working directory — ASK the user whether to
index it before running `ol repo index`. Indexing large trees adds many embedded
chunks and slows search. If they decline, create `<repo-root>/.ol/config.toml`
with `index = false` so the hook and future `ol repo index` calls skip it
permanently.

## Workflow: working on a repo

After `ol repo index` or when the user asks about a codebase:

1. Check whether a project already tracks this repo:
   `ol project search "<repo name>"`
2. **If found:** link if not already linked:
   `ol project repos add <project_id> <repo_id>`
3. **If not found:** create one from the repo metadata, then link:
   ```
   ol project add --name "<repo name>" --description "<what this service does>" \
     [--link "<git remote url>|GitHub"]
   ol project repos add <new_project_id> <repo_id>
   ```
   Use `git remote get-url origin` to get the remote URL for the project link.
