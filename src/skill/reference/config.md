# Config

Tunables live in `~/.config/sclerox/config.toml` (created by `sclerox install`).

```bash
sclerox config show                       # effective settings (file + env + defaults)
sclerox config init [--force]             # write a commented config.toml
sclerox config path                       # where the config file lives
```

Precedence: CLI flag > env var > `~/.config/sclerox/config.toml` > built-in default.

Tunables include `search.semantic_threshold`, dedup thresholds,
`session_context.max_tokens` (real MiniLM-token budget for injected context),
`index.auto` (git|off) and `index.max_files` (reject folders over this; `--force`
overrides), `install.overwrite_*` (protect customizations on upgrade), distill
chunking, and embed chunk size.
