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
chunking, and embed chunk size.

Two settings govern how hard distillation works to avoid duplicate memories:
`dedup.merge_threshold` (cosine at/above which a new fact merges into its best
match even when several match, instead of being flagged) and
`distill.context_memories` (how many related existing memories the distiller is
shown so it can reuse a key rather than invent a near-identical one). Set
`distill.context_memories = 0` to distill without that context.

## Embedding model

`embed.model` picks the model behind semantic search and dedup:
`granite-small-r2` (default) or `minilm-l6-v2`. It downloads once, on first use
or during `sclerox install`, into `$XDG_CACHE_HOME/sclerox/models/`
(`~/.cache/sclerox/models/` by default), along with the tokenizer used for
token budgets. Every file is pinned to a commit and hash-checked before use;
`HF_ENDPOINT` points the download at a mirror.

If the model can't load, commands carry on keyword-only with a warning; only
the `reembed` commands fail.

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
(no re-parsing). Each database switches all-or-nothing; rerun it if interrupted.
It first waits (up to 5 minutes) for running session distillations, which may
be from the version you upgraded from.
It also embeds main-database items saved without a vector (model unavailable,
or `--no-embed`); code chunks indexed without vectors are left to
`sclerox repo reembed`.

`sclerox install` runs the same re-embed for you when it finds a database on
another model or items without a vector (skipped with `--no-model-download`).

Unset similarity thresholds follow the model, since each scores on its own
scale (`sclerox config show` prints the effective values).
`search.semantic_margin` drops semantic hits scoring more than that below the
best hit for the query (Granite default 0.05; `1.0` turns it off). Granite small does
not merge near-duplicates on score until its thresholds are calibrated; it
flags them in `sclerox memory conflicts` instead.
