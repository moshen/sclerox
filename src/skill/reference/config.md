# Config

Tunables live in `~/.config/sclerox/config.toml` (created by `sclerox install`).

```bash
sclerox config show                       # effective settings (file + env + defaults)
sclerox config init [--force]             # write a commented config.toml
sclerox config path                       # where the config file lives
```

Precedence: CLI flag > env var > `~/.config/sclerox/config.toml` > built-in default.

Tunables include `embed.model` (see below), `search.semantic_threshold`, dedup thresholds,
`session_context.max_tokens` (real MiniLM-token budget for injected context),
`index.auto` (git|off) and `index.max_files` (reject folders over this; `--force`
overrides), `install.overwrite_*` (protect customizations on upgrade), distill
chunking, and embedding sizes (below).

Two settings govern how hard distillation works to avoid duplicate memories:
`dedup.merge_threshold` (cosine at/above which a new fact merges into its best
match instead of being flagged) and
`distill.context_memories` (how many related existing memories the distiller is
shown so it can reuse a key rather than invent a near-identical one). Set
`distill.context_memories = 0` to distill without that context.

## Embedding sizes

How much text each vector covers:

- `embed.memory_chars`: characters of a memory's value that get embedded
  (default per model: 4000 for Granite, 800 for MiniLM). The full value is
  always stored and keyword-searchable. Existing memories keep their vectors
  until rewritten; `sclerox db reembed --force` re-embeds them all at the new
  size.
- `embed.chunk_size` / `embed.chunk_overlap` (800 / 200): chunks of meeting,
  todo and research text. New sizes apply to text saved from then on;
  `sclerox db reembed` (or `sclerox install`) re-cuts stored text at them.
- `index.chunk_chars` (800): code chunks; longer functions are split.
  `index.chunk_lines` (15): lines per chunk for files without a tree-sitter
  grammar. Changing either re-chunks each repo on its next index run.

A model only reads so much of one input: Granite 2048 tokens (about 8000
characters of prose), MiniLM 256 (about 1000). Text past the token limit is
not embedded. Characters per token vary with the text, so a size past the
character estimate warns rather than fails.
Larger inputs also cost more: on one CPU core Granite takes about 0.2 s for
4000 characters and over 1 s near its limit.

## Embedding model

`embed.model` picks the model behind semantic search and dedup:
`granite-small-r2` (default) or `minilm-l6-v2`. It downloads once, on first use
or during `sclerox install`, into `$XDG_CACHE_HOME/sclerox/models/`
(`~/.cache/sclerox/models/` by default), along with the tokenizer used for
token budgets. Every file is pinned to a commit and hash-checked before use;
`HF_ENDPOINT` points the download at a mirror.

`embed.enabled = false` (env `SCLEROX_EMBED_ENABLED=false`) turns embeddings
off: the model never loads, nothing gets a vector, and searches are
keyword-only. After turning them back on, `sclerox db reembed --force` fills in
vectors for what was stored meanwhile. If the model can't load, commands carry
on keyword-only with a warning; only the `reembed` commands fail.

Each database records the model its vectors came from. Vectors from different
models can't be compared, so when the recorded model differs from `embed.model`
(after changing it, or after upgrading from a MiniLM-era version) sclerox warns
and turns semantic search and dedup off for that database; keyword search keeps
working. Convert everything with:

```bash
sclerox db migrate                        # shows each database's embedding model
sclerox db reembed                        # re-embed every mismatched database
sclerox db reembed --force                # re-embed even ones already current
```

`db reembed` covers the main database and every indexed repo, from stored text
(no re-parsing). Each database switches all-or-nothing; rerun it if
interrupted, and it goes on from where it stopped.
It first waits (up to 5 minutes) for running session distillations, which may
be from the version you upgraded from.
It also embeds main-database items saved without a vector (model unavailable,
or `--no-embed`); code chunks indexed without vectors are left to
`sclerox repo reembed`. Meeting, todo and research text cut at other
`embed.chunk_size` / `chunk_overlap` values (or before sclerox recorded them) is
re-chunked from its stored text first, and always with `--force`.

`sclerox install` runs the same re-embed for you when it finds a database on
another model or items without a vector (skipped with `--no-model-download`).

With the model server on (below), these commands queue the work for it and
return; add `--wait` to follow it to the end.

Unset similarity thresholds follow the model, since each scores on its own
scale (`sclerox config show` prints the effective values).
`search.code_text_limit` caps the code lines `code search` and `sclerox search`
show for matching the query word for word (default 20).
`search.semantic_margin` drops semantic hits scoring more than that below the
best hit for the query (Granite default 0.05; `1.0` turns it off).
Distillation merges a near-duplicate into the memory it matched only at or
above `dedup.merge_threshold` (Granite 0.97, where every pair read was a real
duplicate). Between `dedup.cosine_threshold` and that, a score cannot tell a
restated fact from a similar but distinct one, so both are kept and flagged in
`sclerox memory conflicts` instead.

## Model server

Loading the model costs each command about a quarter of a second. With
`embed.server = auto` (the default) the first command that needs vectors starts
a background model server, which keeps the model loaded, and later commands get
their vectors from it. There is one server per user. It serves every database
and session, loads another model the first time a command configured for it
asks, and exits after `server.idle_minutes` (15) unused.

```bash
sclerox server status                     # running server: memory, requests, its models; recent runs
sclerox server stop                       # stop it (the next command starts a new one)
sclerox server start [--foreground] [--model M]  # start it yourself, loading M (default embed.model)
```

Commands never depend on it. If the server is off, from another build, or keeps
failing, they load the model themselves as before; a newer build replaces an
older build's server. A command only talks to a server running as the same
user.
`embed.server = off` (env `SCLEROX_EMBED_SERVER=off`) makes commands load the
model themselves and never start a server; it doesn't stop one already running
(`sclerox server stop` does), and `sclerox server start` still works. Runs are
recorded in `~/.local/state/sclerox/server.db`.

Bulk work goes through the server too, and a query waits for at most the one
text being embedded. The `reembed` commands, `repo index`, `install` and the
session hooks' background embedding queue their work in the database it
belongs to and return; the server embeds it and writes the vectors, taking
the databases in turn. `--wait` follows it, and `sclerox server status` lists
it. Queued work outlives the server: the next one goes on with it. Until it is
done, keyword search still finds what has no vector. Session distillation
embeds as it goes, through the server.

The model runtime keeps the memory a long input needed. Over
`server.max_memory_mb` (2000; 0 for no limit) the server exits once nothing has
used it for a minute, and the next command starts a fresh one. Over
`server.hard_memory_mb` (off by default) it exits after the request in
progress, even in the middle of bulk work, which carries on with a new server
(started by the next command, or by one waiting with `--wait`).
A limit the loaded models alone exceed is ignored, as is the soft limit after
three memory exits in 30 minutes. Env: `SCLEROX_SERVER_MAX_MEMORY_MB`,
`SCLEROX_SERVER_HARD_MEMORY_MB`.
