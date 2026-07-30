# Config

Tunables live in `~/.ol/config.toml` (created by `ol install`).

```bash
ol config show                       # effective settings (file + env + defaults)
ol config init [--force]             # write a commented config.toml
ol config path                       # where the config file lives
```

Precedence: CLI flag > env var > `~/.ol/config.toml` > built-in default.

Tunables include `search.semantic_threshold`, dedup thresholds,
`session_context.max_tokens` (real MiniLM-token budget for injected context),
`index.auto` (git|off) and `index.max_files` (reject folders over this; `--force`
overrides), `install.overwrite_*` (protect customizations on upgrade), distill
chunking, and embed chunk size.
